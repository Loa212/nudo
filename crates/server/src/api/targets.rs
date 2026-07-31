//! The `Targets` service.

use nudo_proto::targets_server::Targets;
use nudo_proto::*;
use tonic::{Request, Response, Status};

use super::{Context, internal};
use crate::store::{SshHost, TargetInput, page_offset, page_size};

pub struct TargetsService {
    context: Context,
}

impl TargetsService {
    pub fn new(context: Context) -> Self {
        Self { context }
    }
}

#[tonic::async_trait]
impl Targets for TargetsService {
    async fn create(
        &self,
        request: Request<CreateTargetRequest>,
    ) -> Result<Response<Target>, Status> {
        let request = request.into_inner();

        // The guardrail is checked against the target being *created*, so a
        // client cannot create a latency-critical host without saying so.
        let intended = Target {
            name: request.name.clone(),
            latency_critical: request.latency_critical,
            ..Default::default()
        };
        let authorized = self
            .context
            .authorize(
                request.mutation.as_ref(),
                "Targets.Create",
                "",
                request.latency_critical.then_some(&intended),
                format!("created target {} ({})", request.name, request.host),
            )
            .await?;

        if authorized.dry_run {
            // Return what would be created, without touching the database.
            return Ok(Response::new(Target {
                name: request.name,
                host: request.host,
                port: if request.port == 0 { 22 } else { request.port },
                user: request.user,
                ssh_key_id: request.ssh_key_id,
                latency_critical: request.latency_critical,
                labels: request.labels,
                status: target::Status::Unknown as i32,
                ..Default::default()
            }));
        }

        let created = self
            .context
            .store
            .create_target(&TargetInput {
                name: request.name,
                host: request.host,
                port: request.port,
                user: request.user,
                ssh_key_id: request.ssh_key_id,
                latency_critical: request.latency_critical,
                labels: request.labels,
            })
            .await
            .map_err(super::invalid)?;

        Ok(Response::new(created))
    }

    async fn get(&self, request: Request<GetTargetRequest>) -> Result<Response<Target>, Status> {
        let target = self
            .context
            .require_target(&request.into_inner().id)
            .await?;
        Ok(Response::new(target))
    }

    async fn list(
        &self,
        request: Request<ListTargetsRequest>,
    ) -> Result<Response<ListTargetsResponse>, Status> {
        let request = request.into_inner();
        let limit = page_size(request.page_size);
        let offset = page_offset(&request.page_token);

        let targets = self
            .context
            .store
            .list_targets(&request.label_selector, limit, offset)
            .await
            .map_err(internal)?;

        let next_page_token = crate::store::next_page_token(offset, targets.len(), limit);
        Ok(Response::new(ListTargetsResponse {
            targets,
            next_page_token,
        }))
    }

    async fn update(
        &self,
        request: Request<UpdateTargetRequest>,
    ) -> Result<Response<Target>, Status> {
        let request = request.into_inner();
        let existing = self.context.require_target(&request.id).await?;

        let authorized = self
            .context
            .authorize(
                request.mutation.as_ref(),
                "Targets.Update",
                &request.id,
                Some(&existing),
                format!("updated target {}", existing.name),
            )
            .await?;

        let update = request
            .target
            .ok_or_else(|| Status::invalid_argument("update requires a target"))?;

        if authorized.dry_run {
            return Ok(Response::new(existing));
        }

        let updated = self
            .context
            .store
            .update_target(&request.id, &update, &request.update_mask)
            .await
            .map_err(super::invalid)?;

        Ok(Response::new(updated))
    }

    async fn delete(&self, request: Request<DeleteTargetRequest>) -> Result<Response<()>, Status> {
        let request = request.into_inner();
        let existing = self.context.require_target(&request.id).await?;

        // Deleting a target removes its services' rows, so say so rather than
        // letting the cascade be a surprise.
        let services = self
            .context
            .store
            .list_services(&request.id, 500, 0)
            .await
            .map_err(internal)?;

        let authorized = self
            .context
            .authorize(
                request.mutation.as_ref(),
                "Targets.Delete",
                &request.id,
                Some(&existing),
                format!(
                    "deleted target {} and {} service definition(s)",
                    existing.name,
                    services.len()
                ),
            )
            .await?;

        if authorized.dry_run {
            return Ok(Response::new(()));
        }

        self.context
            .store
            .delete_target(&request.id)
            .await
            .map_err(super::invalid)?;

        // The units and releases stay on the host — deleting the row is not the
        // same as decommissioning the machine, and silently stopping services on
        // a box we are forgetting about would be worse.
        for service in services {
            self.context.bus.forget_service(&service.id);
        }

        Ok(Response::new(()))
    }

    async fn check(
        &self,
        request: Request<CheckTargetRequest>,
    ) -> Result<Response<CheckTargetResponse>, Status> {
        let target = self
            .context
            .require_target(&request.into_inner().id)
            .await?;

        // A check is read-only, so it is allowed against a latency-critical box
        // without an opt-in: it opens one SSH connection and runs a handful of
        // trivial commands. Refusing would mean the one host you most want to
        // verify is the one you cannot.
        //
        // Assembling the SSH details is a precondition failure (no key
        // configured), which is a different thing from the target being
        // unreachable, so it is reported as an error rather than as a failed
        // check.
        let ssh_target = self
            .context
            .engine
            .ssh_target_for(&target)
            .await
            .map_err(|error| Status::failed_precondition(format!("{error:#}")))?;

        // This is where a target is deliberately pinned: `targets check` is what
        // an operator runs after registering a host, so it is the natural place
        // for first use to happen and be reported. A refused connection — a
        // changed host key — is a check result rather than an error, since
        // showing which part is broken is the whole point of this RPC.
        let connection = self
            .context
            .engine
            .connect_prepared(&target, &ssh_target)
            .await;

        // Probe the conventional release root; a service's own root is checked
        // when it deploys.
        let (ok, checks) = crate::probe::check_target(connection, "/opt").await;

        let status = if ok {
            target::Status::Reachable
        } else {
            target::Status::Unreachable
        };
        if let Err(error) = self
            .context
            .store
            .set_target_status(&target.id, status)
            .await
        {
            tracing::warn!(%error, "recording target status failed");
        }

        Ok(Response::new(CheckTargetResponse { ok, checks }))
    }

    /// Accepts a target's pending host key, making it the pinned one.
    ///
    /// The flow for a legitimately rebuilt host. Guarded and audited like any
    /// other mutation — accepting a key is a security decision, and the audit
    /// entry records which fingerprint was accepted and by whom.
    async fn accept_host_key(
        &self,
        request: Request<AcceptHostKeyRequest>,
    ) -> Result<Response<Target>, Status> {
        let request = request.into_inner();
        let existing = self.context.require_target(&request.id).await?;

        let host_key = existing.host_key.clone().unwrap_or_default();
        if host_key.pending_key.trim().is_empty() {
            return Err(Status::failed_precondition(format!(
                "target {} has no pending host-key change to accept",
                existing.name
            )));
        }

        // The operator accepts a fingerprint they have looked at, not "whatever
        // is pending" — otherwise a key that changed again between the review
        // and the click would be accepted unseen.
        let offered = request.fingerprint.trim();
        if offered.is_empty() {
            return Err(Status::invalid_argument(
                "accepting a host key requires the fingerprint being accepted",
            ));
        }
        if offered != host_key.pending_fingerprint {
            return Err(Status::failed_precondition(format!(
                "that is not the key waiting for review: pending is {}, you sent {offered}",
                host_key.pending_fingerprint
            )));
        }

        let authorized = self
            .context
            .authorize(
                request.mutation.as_ref(),
                "Targets.AcceptHostKey",
                &request.id,
                Some(&existing),
                format!(
                    "accepted a new ssh host key for {}: {} replaces {}",
                    existing.name,
                    host_key.pending_fingerprint,
                    if host_key.fingerprint.is_empty() {
                        "no previously pinned key"
                    } else {
                        &host_key.fingerprint
                    }
                ),
            )
            .await?;

        if authorized.dry_run {
            return Ok(Response::new(existing));
        }

        self.context
            .store
            .pin_host_key(
                SshHost::Target,
                &request.id,
                &host_key.pending_key,
                &host_key.pending_fingerprint,
            )
            .await
            .map_err(internal)?;

        Ok(Response::new(
            self.context.require_target(&request.id).await?,
        ))
    }

    /// Forgets a target's pinned host key, so the next connection pins afresh.
    ///
    /// For a host rebuilt while nobody was watching, whose old key is of no
    /// further interest. This reopens the first-use window, which is a weaker
    /// position than accepting a reviewed key, so it is audited as its own
    /// action rather than folded into acceptance.
    async fn forget_host_key(
        &self,
        request: Request<ForgetHostKeyRequest>,
    ) -> Result<Response<Target>, Status> {
        let request = request.into_inner();
        let existing = self.context.require_target(&request.id).await?;

        let previous = existing
            .host_key
            .as_ref()
            .map(|k| k.fingerprint.clone())
            .unwrap_or_default();

        let authorized = self
            .context
            .authorize(
                request.mutation.as_ref(),
                "Targets.ForgetHostKey",
                &request.id,
                Some(&existing),
                format!(
                    "forgot the pinned ssh host key for {} ({}); the next connection will pin afresh",
                    existing.name,
                    if previous.is_empty() {
                        "none was pinned"
                    } else {
                        &previous
                    }
                ),
            )
            .await?;

        if authorized.dry_run {
            return Ok(Response::new(existing));
        }

        self.context
            .store
            .forget_host_key(SshHost::Target, &request.id)
            .await
            .map_err(super::invalid)?;

        Ok(Response::new(
            self.context.require_target(&request.id).await?,
        ))
    }

    /// Turns ingress on for a target, or changes its settings.
    ///
    /// Guarded like any other mutation of the host, which is the answer to
    /// "what about a latency-critical target": installing a reverse proxy on a
    /// box tuned for latency is exactly the sort of change the flag exists to
    /// make somebody say out loud, so it needs `allow_latency_critical` — the
    /// same rule as everything else that touches that machine, rather than a
    /// second rule to learn.
    async fn enable_ingress(
        &self,
        request: Request<EnableIngressRequest>,
    ) -> Result<Response<Target>, Status> {
        let request = request.into_inner();
        let existing = self.context.require_target(&request.target_id).await?;

        let mode = ingress::Mode::try_from(request.mode).unwrap_or(ingress::Mode::Unspecified);
        if mode == ingress::Mode::Unspecified {
            return Err(Status::invalid_argument(
                "set a mode: managed for nudo to install and drive Caddy, \
                 external to render the config for a proxy you run yourself",
            ));
        }

        let authorized = self
            .context
            .authorize(
                request.mutation.as_ref(),
                "Targets.EnableIngress",
                &request.target_id,
                Some(&existing),
                format!("enabled {} ingress on {}", mode.as_str(), existing.name),
            )
            .await?;

        if authorized.dry_run {
            return Ok(Response::new(existing));
        }

        self.context
            .store
            .set_ingress(
                &request.target_id,
                mode,
                request.admin_port,
                &request.acme_email,
            )
            .await
            .map_err(super::invalid)?;

        let target = self.context.require_target(&request.target_id).await?;

        // Install and start the proxy now, so enabling ingress is one step
        // rather than a setting plus a reload somebody has to know to run.
        //
        // A failure here does not fail the request: the configuration is
        // already stored and correct, and the host may simply be down. The
        // target is left pending with the reason recorded, and the next reload
        // or deploy retries — which is a better outcome than refusing to
        // remember what the operator asked for.
        if mode == ingress::Mode::Managed
            && let Err(error) = self.provision(&target).await
        {
            let message = format!("{error:#}");
            tracing::warn!(%message, target = %target.name, "provisioning ingress failed");
            if let Err(error) = self
                .context
                .store
                .set_ingress_status(&target.id, ingress::Status::Pending, None, &message)
                .await
            {
                tracing::warn!(%error, "recording ingress status failed");
            }
        }

        Ok(Response::new(
            self.context.require_target(&request.target_id).await?,
        ))
    }

    /// Turns ingress off, stopping the proxy but leaving its config on disk.
    async fn disable_ingress(
        &self,
        request: Request<DisableIngressRequest>,
    ) -> Result<Response<Target>, Status> {
        let request = request.into_inner();
        let existing = self.context.require_target(&request.target_id).await?;

        let authorized = self
            .context
            .authorize(
                request.mutation.as_ref(),
                "Targets.DisableIngress",
                &request.target_id,
                Some(&existing),
                format!("disabled ingress on {}", existing.name),
            )
            .await?;

        if authorized.dry_run {
            return Ok(Response::new(existing));
        }

        // Stop the proxy before forgetting that it is ours to manage. The other
        // order leaves a proxy running on a target nudo no longer believes has
        // one — still serving, still holding :443, and now invisible.
        if crate::ingress::is_managed(&existing)
            && let Ok(session) = self.connect_for_ingress(&existing).await
        {
            if let Err(error) = crate::ingress::reconcile::stop(&session).await {
                tracing::warn!(%error, "stopping the proxy failed");
            }
            let _ = session.close().await;
        }

        self.context
            .store
            .set_ingress(&request.target_id, ingress::Mode::Unspecified, 0, "")
            .await
            .map_err(super::invalid)?;

        Ok(Response::new(
            self.context.require_target(&request.target_id).await?,
        ))
    }

    /// The config nudo would write, without writing it.
    ///
    /// Read-only and host-free: it renders from the database, so it works on an
    /// unreachable target and is the whole of what external mode offers.
    async fn render_ingress(
        &self,
        request: Request<RenderIngressRequest>,
    ) -> Result<Response<RenderIngressResponse>, Status> {
        let target = self
            .context
            .require_target(&request.into_inner().target_id)
            .await?;

        let services = self
            .context
            .store
            .routed_services(&target.id)
            .await
            .map_err(internal)?;

        Ok(Response::new(RenderIngressResponse {
            config: crate::ingress::render(&target, &services),
            path: crate::ingress::CONFIG_PATH.to_string(),
            routes: crate::ingress::routes_for(&services),
        }))
    }

    /// Writes the config and reloads the proxy.
    async fn reload_ingress(
        &self,
        request: Request<ReloadIngressRequest>,
    ) -> Result<Response<ReloadIngressResponse>, Status> {
        let request = request.into_inner();
        let target = self.context.require_target(&request.target_id).await?;

        if !crate::ingress::is_managed(&target) {
            return Err(Status::failed_precondition(format!(
                "{} does not have managed ingress; nudo renders the config but \
                 does not drive the proxy on this host",
                target.name
            )));
        }

        let authorized = self
            .context
            .authorize(
                request.mutation.as_ref(),
                "Targets.ReloadIngress",
                &request.target_id,
                Some(&target),
                format!("reloaded the proxy on {}", target.name),
            )
            .await?;

        let services = self
            .context
            .store
            .routed_services(&target.id)
            .await
            .map_err(internal)?;

        if authorized.dry_run {
            return Ok(Response::new(ReloadIngressResponse {
                ok: true,
                error: String::new(),
                routes: crate::ingress::routes_for(&services),
            }));
        }

        let session = self.connect_for_ingress(&target).await?;
        let outcome = crate::ingress::reconcile::reload(&session, &target, &services)
            .await
            .map_err(internal)?;
        let _ = session.close().await;

        self.record_reload(&target.id, &outcome).await;

        Ok(Response::new(ReloadIngressResponse {
            ok: outcome.ok,
            error: outcome.error,
            routes: outcome.routes,
        }))
    }

    /// Diagnoses whether ingress here can actually serve its domains.
    ///
    /// Read-only, so it is allowed against a latency-critical target without an
    /// opt-in, for the same reason `Check` is: the host you most want to
    /// diagnose must not be the one you cannot.
    async fn check_ingress(
        &self,
        request: Request<CheckIngressRequest>,
    ) -> Result<Response<CheckIngressResponse>, Status> {
        let target = self
            .context
            .require_target(&request.into_inner().target_id)
            .await?;

        if crate::ingress::mode_of(&target) == ingress::Mode::Unspecified {
            return Err(Status::failed_precondition(format!(
                "{} has no ingress configured",
                target.name
            )));
        }

        let services = self
            .context
            .store
            .routed_services(&target.id)
            .await
            .map_err(internal)?;

        let session = self.connect_for_ingress(&target).await?;
        let response = crate::ingress::reconcile::check(&session, &target, &services).await;
        let _ = session.close().await;

        Ok(Response::new(response))
    }
}

impl TargetsService {
    /// Opens the SSH session ingress work runs over.
    ///
    /// Separate from the deploy engine's connect only so the failure reads as a
    /// precondition — no key configured, host unreachable — rather than as an
    /// internal error, which is what an operator needs to see.
    async fn connect_for_ingress(&self, target: &Target) -> Result<crate::ssh::SshSession, Status> {
        let ssh_target = self
            .context
            .engine
            .ssh_target_for(target)
            .await
            .map_err(|error| Status::failed_precondition(format!("{error:#}")))?;

        self.context
            .engine
            .connect_prepared(target, &ssh_target)
            .await
            .map_err(|error| Status::failed_precondition(format!("{error:#}")))
    }

    /// Installs the proxy, writes the current config and starts it.
    ///
    /// The whole of "enable ingress" against a real host, in the order that
    /// keeps it safe to repeat: install is idempotent, the config is written
    /// and validated before the proxy is asked to serve it, and starting an
    /// already-running unit is a no-op.
    async fn provision(&self, target: &Target) -> anyhow::Result<()> {
        let session = self
            .connect_for_ingress(target)
            .await
            .map_err(|status| anyhow::anyhow!("{}", status.message()))?;

        let result = async {
            let version = crate::ingress::reconcile::install(&session).await?;
            let services = self.context.store.routed_services(&target.id).await?;

            // Writes the config and brings the proxy up: on a host where it is
            // not running yet — every host, the first time — starting it is how
            // the config gets applied, and `reload` does that rather than
            // talking to an admin API that is not listening.
            let outcome = crate::ingress::reconcile::reload(&session, target, &services).await?;
            if !outcome.ok {
                anyhow::bail!("the proxy rejected the config: {}", outcome.error);
            }

            anyhow::Ok(version)
        }
        .await;

        let _ = session.close().await;
        let version = result?;

        self.context
            .store
            .set_ingress_status(&target.id, ingress::Status::Active, Some(&version), "")
            .await?;
        Ok(())
    }

    /// Records what a reload did against the target.
    ///
    /// A failed reload leaves the proxy serving the previous config — Caddy
    /// restores it itself — so the target is degraded rather than broken, and
    /// the reason is stored where someone looking at the target will find it.
    async fn record_reload(
        &self,
        target_id: &str,
        outcome: &crate::ingress::reconcile::ReloadOutcome,
    ) {
        let status = if outcome.ok {
            ingress::Status::Active
        } else {
            ingress::Status::Degraded
        };

        if let Err(error) = self
            .context
            .store
            .set_ingress_status(
                target_id,
                status,
                outcome.version.as_deref(),
                &outcome.error,
            )
            .await
        {
            tracing::warn!(%error, "recording ingress status failed");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::Bus;
    use crate::store::Store;
    use std::sync::Arc;

    async fn service() -> TargetsService {
        TargetsService::new(Context::new(
            Store::open_in_memory().await.expect("store"),
            Bus::default(),
            crate::crypto::SecretKey::generate(),
            Arc::new(crate::Config::default()),
        ))
    }

    fn create(name: &str, latency_critical: bool) -> CreateTargetRequest {
        CreateTargetRequest {
            mutation: Some(Mutation::by(Actor::human("usr_1", "alice"))),
            name: name.to_string(),
            host: "10.0.0.5".to_string(),
            port: 22,
            user: "root".to_string(),
            latency_critical,
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn a_target_is_created_and_readable() {
        let service = service().await;
        let created = service
            .create(Request::new(create("edge-1", false)))
            .await
            .expect("create")
            .into_inner();

        assert_eq!(created.name, "edge-1");
        let fetched = service
            .get(Request::new(GetTargetRequest {
                id: created.id.clone(),
            }))
            .await
            .expect("get")
            .into_inner();
        assert_eq!(fetched.id, created.id);
    }

    #[tokio::test]
    async fn creating_a_latency_critical_target_requires_the_opt_in() {
        // Otherwise a client could create the hot-path box without ever
        // acknowledging what it is.
        let service = service().await;
        let status = service
            .create(Request::new(create("hot-box", true)))
            .await
            .expect_err("must be refused");
        assert_eq!(status.code(), tonic::Code::FailedPrecondition);

        let allowed = CreateTargetRequest {
            mutation: Some(Mutation {
                actor: Some(Actor::human("usr_1", "alice")),
                allow_latency_critical: true,
                ..Default::default()
            }),
            ..create("hot-box", true)
        };
        let created = service
            .create(Request::new(allowed))
            .await
            .expect("must be allowed")
            .into_inner();
        assert!(created.latency_critical);
    }

    #[tokio::test]
    async fn a_dry_run_create_returns_the_plan_without_persisting_it() {
        let service = service().await;
        let planned = service
            .create(Request::new(CreateTargetRequest {
                mutation: Some(Mutation {
                    actor: Some(Actor::agent("sess_1", "claude")),
                    dry_run: true,
                    ..Default::default()
                }),
                ..create("would-be", false)
            }))
            .await
            .expect("dry run")
            .into_inner();

        assert_eq!(planned.name, "would-be");
        // Nothing was written.
        assert!(planned.id.is_empty());
        let listed = service
            .list(Request::new(ListTargetsRequest::default()))
            .await
            .expect("list")
            .into_inner();
        assert!(listed.targets.is_empty());
    }

    #[tokio::test]
    async fn a_dry_run_fills_in_the_defaults_it_would_apply() {
        let service = service().await;
        let planned = service
            .create(Request::new(CreateTargetRequest {
                mutation: Some(Mutation {
                    actor: Some(Actor::human("u", "alice")),
                    dry_run: true,
                    ..Default::default()
                }),
                port: 0,
                ..create("defaulted", false)
            }))
            .await
            .expect("dry run")
            .into_inner();
        assert_eq!(planned.port, 22);
    }

    #[tokio::test]
    async fn getting_a_missing_target_is_not_found() {
        let service = service().await;
        let status = service
            .get(Request::new(GetTargetRequest {
                id: "tgt_nope".to_string(),
            }))
            .await
            .expect_err("err");
        assert_eq!(status.code(), tonic::Code::NotFound);
    }

    #[tokio::test]
    async fn a_duplicate_name_is_an_invalid_argument_not_an_internal_error() {
        let service = service().await;
        service
            .create(Request::new(create("dup", false)))
            .await
            .expect("first");
        let status = service
            .create(Request::new(create("dup", false)))
            .await
            .expect_err("second");
        assert_eq!(status.code(), tonic::Code::InvalidArgument);
    }

    #[tokio::test]
    async fn listing_paginates_with_an_opaque_token() {
        let service = service().await;
        for i in 0..5 {
            service
                .create(Request::new(create(&format!("box-{i}"), false)))
                .await
                .expect("create");
        }

        let first = service
            .list(Request::new(ListTargetsRequest {
                page_size: 2,
                ..Default::default()
            }))
            .await
            .expect("list")
            .into_inner();
        assert_eq!(first.targets.len(), 2);
        assert!(!first.next_page_token.is_empty());

        let second = service
            .list(Request::new(ListTargetsRequest {
                page_size: 2,
                page_token: first.next_page_token,
                ..Default::default()
            }))
            .await
            .expect("list")
            .into_inner();
        assert_eq!(second.targets.len(), 2);
        assert!(
            first
                .targets
                .iter()
                .all(|a| second.targets.iter().all(|b| a.id != b.id))
        );

        // The last page ends the sequence.
        let last = service
            .list(Request::new(ListTargetsRequest {
                page_size: 2,
                page_token: "4".to_string(),
                ..Default::default()
            }))
            .await
            .expect("list")
            .into_inner();
        assert_eq!(last.targets.len(), 1);
        assert!(last.next_page_token.is_empty());
    }

    #[tokio::test]
    async fn listing_filters_by_label_selector() {
        let service = service().await;
        service
            .create(Request::new(CreateTargetRequest {
                labels: std::collections::HashMap::from([("env".to_string(), "prod".to_string())]),
                ..create("prod-box", false)
            }))
            .await
            .expect("create");
        service
            .create(Request::new(create("other-box", false)))
            .await
            .expect("create");

        let filtered = service
            .list(Request::new(ListTargetsRequest {
                label_selector: "env=prod".to_string(),
                ..Default::default()
            }))
            .await
            .expect("list")
            .into_inner();
        assert_eq!(filtered.targets.len(), 1);
        assert_eq!(filtered.targets[0].name, "prod-box");
    }

    #[tokio::test]
    async fn an_update_applies_its_mask_and_is_audited() {
        let service = service().await;
        let created = service
            .create(Request::new(create("renameable", false)))
            .await
            .expect("create")
            .into_inner();

        let updated = service
            .update(Request::new(UpdateTargetRequest {
                mutation: Some(Mutation::by(Actor::human("usr_1", "alice"))),
                id: created.id.clone(),
                target: Some(Target {
                    name: "renamed".to_string(),
                    host: "192.168.0.1".to_string(),
                    ..Default::default()
                }),
                update_mask: vec!["name".to_string()],
            }))
            .await
            .expect("update")
            .into_inner();

        assert_eq!(updated.name, "renamed");
        assert_eq!(updated.host, "10.0.0.5", "outside the mask");

        let audit = service
            .context
            .store
            .list_audit(&created.id, actor::Kind::Unspecified, 50, 0)
            .await
            .expect("audit");
        assert!(audit.iter().any(|e| e.action == "Targets.Update"));
    }

    #[tokio::test]
    async fn an_update_with_no_target_message_is_an_invalid_argument() {
        let service = service().await;
        let created = service
            .create(Request::new(create("box", false)))
            .await
            .expect("create")
            .into_inner();

        let status = service
            .update(Request::new(UpdateTargetRequest {
                mutation: Some(Mutation::by(Actor::human("u", "alice"))),
                id: created.id,
                target: None,
                update_mask: vec![],
            }))
            .await
            .expect_err("err");
        assert_eq!(status.code(), tonic::Code::InvalidArgument);
    }

    #[tokio::test]
    async fn updating_a_latency_critical_target_needs_the_opt_in() {
        let service = service().await;
        let created = service
            .create(Request::new(CreateTargetRequest {
                mutation: Some(Mutation {
                    actor: Some(Actor::human("u", "alice")),
                    allow_latency_critical: true,
                    ..Default::default()
                }),
                ..create("hot-box", true)
            }))
            .await
            .expect("create")
            .into_inner();

        let status = service
            .update(Request::new(UpdateTargetRequest {
                mutation: Some(Mutation::by(Actor::human("u", "alice"))),
                id: created.id,
                target: Some(Target {
                    host: "10.0.0.9".to_string(),
                    ..Default::default()
                }),
                update_mask: vec!["host".to_string()],
            }))
            .await
            .expect_err("must be refused");
        assert_eq!(status.code(), tonic::Code::FailedPrecondition);
    }

    #[tokio::test]
    async fn a_delete_says_how_many_services_it_takes_with_it() {
        let service = service().await;
        let created = service
            .create(Request::new(create("doomed", false)))
            .await
            .expect("create")
            .into_inner();

        service
            .context
            .store
            .create_service(&Service {
                target_id: created.id.clone(),
                name: "bot".to_string(),
                ..Default::default()
            })
            .await
            .expect("service");

        service
            .delete(Request::new(DeleteTargetRequest {
                mutation: Some(Mutation::by(Actor::human("usr_1", "alice"))),
                id: created.id.clone(),
            }))
            .await
            .expect("delete");

        let audit = service
            .context
            .store
            .list_audit(&created.id, actor::Kind::Unspecified, 50, 0)
            .await
            .expect("audit");
        let summary = &audit
            .iter()
            .find(|e| e.action == "Targets.Delete")
            .expect("delete entry")
            .summary;
        assert!(summary.contains("1 service"), "got: {summary}");
    }

    #[tokio::test]
    async fn a_dry_run_delete_leaves_the_target_in_place() {
        let service = service().await;
        let created = service
            .create(Request::new(create("spared", false)))
            .await
            .expect("create")
            .into_inner();

        service
            .delete(Request::new(DeleteTargetRequest {
                mutation: Some(Mutation {
                    actor: Some(Actor::agent("s", "claude")),
                    dry_run: true,
                    ..Default::default()
                }),
                id: created.id.clone(),
            }))
            .await
            .expect("dry run");

        assert!(
            service
                .get(Request::new(GetTargetRequest { id: created.id }))
                .await
                .is_ok()
        );
    }

    #[tokio::test]
    async fn deleting_a_missing_target_is_not_found() {
        let service = service().await;
        let status = service
            .delete(Request::new(DeleteTargetRequest {
                mutation: Some(Mutation::by(Actor::human("u", "alice"))),
                id: "tgt_nope".to_string(),
            }))
            .await
            .expect_err("err");
        assert_eq!(status.code(), tonic::Code::NotFound);
    }

    #[tokio::test]
    async fn a_check_against_a_target_with_no_key_explains_the_precondition() {
        let service = service().await;
        let created = service
            .create(Request::new(create("keyless", false)))
            .await
            .expect("create")
            .into_inner();

        let status = service
            .check(Request::new(CheckTargetRequest { id: created.id }))
            .await
            .expect_err("err");
        assert_eq!(status.code(), tonic::Code::FailedPrecondition);
        assert!(status.message().contains("SSH key"));
    }

    // ---- host keys ----

    const KEY_A: &str =
        "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAINbQLN3OR4KHUki7vfmdITOI3q+Nfu9w3X2agJ+IDHXR";
    const KEY_B: &str =
        "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIIwpYjPDsSOQR3dD30dkum4PseIZzCiIqleJEkpyKBfu";

    /// A target with a pinned key and a change waiting to be reviewed.
    async fn target_with_a_pending_change(service: &TargetsService, name: &str) -> Target {
        let created = service
            .create(Request::new(create(name, false)))
            .await
            .expect("create")
            .into_inner();
        service
            .context
            .store
            .pin_host_key(SshHost::Target, &created.id, KEY_A, "SHA256:aaa")
            .await
            .expect("pin");
        service
            .context
            .store
            .record_pending_host_key(SshHost::Target, &created.id, KEY_B, "SHA256:bbb")
            .await
            .expect("record");
        service
            .context
            .require_target(&created.id)
            .await
            .expect("get")
    }

    fn accept(id: &str, fingerprint: &str) -> AcceptHostKeyRequest {
        AcceptHostKeyRequest {
            mutation: Some(Mutation::by(Actor::human("usr_1", "alice"))),
            id: id.to_string(),
            fingerprint: fingerprint.to_string(),
        }
    }

    #[tokio::test]
    async fn accepting_a_reviewed_change_pins_it_and_is_audited_with_the_fingerprint() {
        let service = service().await;
        let target = target_with_a_pending_change(&service, "rebuilt").await;

        let updated = service
            .accept_host_key(Request::new(accept(&target.id, "SHA256:bbb")))
            .await
            .expect("accept")
            .into_inner();

        let host_key = updated.host_key.expect("host key");
        assert_eq!(host_key.key, KEY_B);
        assert_eq!(host_key.fingerprint, "SHA256:bbb");
        assert!(host_key.pending_key.is_empty());

        // Accepting a host key is a security decision, so the audit line has to
        // say which fingerprint was accepted and what it replaced.
        let audit = service
            .context
            .store
            .list_audit(&target.id, actor::Kind::Unspecified, 50, 0)
            .await
            .expect("audit");
        let entry = audit
            .iter()
            .find(|e| e.action == "Targets.AcceptHostKey")
            .expect("an audit entry");
        assert!(
            entry.summary.contains("SHA256:bbb"),
            "got: {}",
            entry.summary
        );
        assert!(
            entry.summary.contains("SHA256:aaa"),
            "got: {}",
            entry.summary
        );
    }

    #[tokio::test]
    async fn accepting_a_fingerprint_that_is_not_the_pending_one_is_refused() {
        // The operator accepts the key they looked at. If it changed again
        // between the review and the click, accepting it unseen is exactly the
        // outcome this whole feature exists to prevent.
        let service = service().await;
        let target = target_with_a_pending_change(&service, "moving").await;

        let status = service
            .accept_host_key(Request::new(accept(&target.id, "SHA256:something-else")))
            .await
            .expect_err("must be refused");
        assert_eq!(status.code(), tonic::Code::FailedPrecondition);

        // And nothing was pinned.
        let unchanged = service
            .context
            .require_target(&target.id)
            .await
            .expect("get")
            .host_key
            .expect("host key");
        assert_eq!(unchanged.key, KEY_A);
        assert_eq!(unchanged.pending_fingerprint, "SHA256:bbb");
    }

    #[tokio::test]
    async fn accepting_without_naming_a_fingerprint_is_an_invalid_argument() {
        let service = service().await;
        let target = target_with_a_pending_change(&service, "unnamed").await;

        let status = service
            .accept_host_key(Request::new(accept(&target.id, "  ")))
            .await
            .expect_err("must be refused");
        assert_eq!(status.code(), tonic::Code::InvalidArgument);
    }

    #[tokio::test]
    async fn accepting_when_nothing_is_pending_explains_that_rather_than_pinning() {
        let service = service().await;
        let created = service
            .create(Request::new(create("settled", false)))
            .await
            .expect("create")
            .into_inner();
        service
            .context
            .store
            .pin_host_key(SshHost::Target, &created.id, KEY_A, "SHA256:aaa")
            .await
            .expect("pin");

        let status = service
            .accept_host_key(Request::new(accept(&created.id, "SHA256:aaa")))
            .await
            .expect_err("must be refused");
        assert_eq!(status.code(), tonic::Code::FailedPrecondition);
        assert!(
            status.message().contains("no pending"),
            "{}",
            status.message()
        );
    }

    #[tokio::test]
    async fn accepting_a_key_on_a_latency_critical_host_needs_the_opt_in() {
        // Same guardrail as every other mutation: accepting a key is what lets
        // nudo talk to that box again, so it is not exempt.
        let service = service().await;
        let created = service
            .create(Request::new(CreateTargetRequest {
                mutation: Some(Mutation {
                    actor: Some(Actor::human("u", "alice")),
                    allow_latency_critical: true,
                    ..Default::default()
                }),
                ..create("hot-box", true)
            }))
            .await
            .expect("create")
            .into_inner();
        service
            .context
            .store
            .record_pending_host_key(SshHost::Target, &created.id, KEY_B, "SHA256:bbb")
            .await
            .expect("record");

        let status = service
            .accept_host_key(Request::new(accept(&created.id, "SHA256:bbb")))
            .await
            .expect_err("must be refused");
        assert_eq!(status.code(), tonic::Code::FailedPrecondition);
    }

    #[tokio::test]
    async fn a_dry_run_acceptance_changes_nothing() {
        let service = service().await;
        let target = target_with_a_pending_change(&service, "planned").await;

        service
            .accept_host_key(Request::new(AcceptHostKeyRequest {
                mutation: Some(Mutation {
                    actor: Some(Actor::agent("sess_1", "claude")),
                    dry_run: true,
                    ..Default::default()
                }),
                ..accept(&target.id, "SHA256:bbb")
            }))
            .await
            .expect("dry run");

        let unchanged = service
            .context
            .require_target(&target.id)
            .await
            .expect("get")
            .host_key
            .expect("host key");
        assert_eq!(unchanged.key, KEY_A);
        assert_eq!(unchanged.pending_fingerprint, "SHA256:bbb");
    }

    #[tokio::test]
    async fn forgetting_a_key_clears_it_and_says_so_in_the_audit() {
        let service = service().await;
        let target = target_with_a_pending_change(&service, "wiped").await;

        let updated = service
            .forget_host_key(Request::new(ForgetHostKeyRequest {
                mutation: Some(Mutation::by(Actor::human("usr_1", "alice"))),
                id: target.id.clone(),
            }))
            .await
            .expect("forget")
            .into_inner();
        assert!(updated.host_key.is_none());

        let audit = service
            .context
            .store
            .list_audit(&target.id, actor::Kind::Unspecified, 50, 0)
            .await
            .expect("audit");
        let entry = audit
            .iter()
            .find(|e| e.action == "Targets.ForgetHostKey")
            .expect("an audit entry");
        // The old fingerprint, since after this it is the only record there was
        // one.
        assert!(
            entry.summary.contains("SHA256:aaa"),
            "got: {}",
            entry.summary
        );
    }

    #[tokio::test]
    async fn host_key_operations_on_a_missing_target_are_not_found() {
        let service = service().await;
        for status in [
            service
                .accept_host_key(Request::new(accept("tgt_nope", "SHA256:x")))
                .await
                .expect_err("accept"),
            service
                .forget_host_key(Request::new(ForgetHostKeyRequest {
                    mutation: Some(Mutation::by(Actor::human("u", "alice"))),
                    id: "tgt_nope".to_string(),
                }))
                .await
                .expect_err("forget"),
        ] {
            assert_eq!(status.code(), tonic::Code::NotFound);
        }
    }

    #[tokio::test]
    async fn checking_a_latency_critical_target_is_allowed_since_it_is_read_only() {
        // The box you most want to verify must not be the one you cannot.
        let service = service().await;
        let created = service
            .create(Request::new(CreateTargetRequest {
                mutation: Some(Mutation {
                    actor: Some(Actor::human("u", "alice")),
                    allow_latency_critical: true,
                    ..Default::default()
                }),
                ..create("hot-box", true)
            }))
            .await
            .expect("create")
            .into_inner();

        // Fails on the missing key, not on the guardrail.
        let status = service
            .check(Request::new(CheckTargetRequest { id: created.id }))
            .await
            .expect_err("err");
        assert!(
            status.message().contains("SSH key"),
            "got: {}",
            status.message()
        );
    }

    // ---- ingress ----

    async fn target_with_ingress(service: &TargetsService, name: &str) -> Target {
        let created = service
            .create(Request::new(create(name, false)))
            .await
            .expect("create")
            .into_inner();

        service
            .enable_ingress(Request::new(EnableIngressRequest {
                mutation: Some(Mutation::by(Actor::human("usr_1", "alice"))),
                target_id: created.id,
                mode: ingress::Mode::Managed as i32,
                ..Default::default()
            }))
            .await
            .expect("enable ingress")
            .into_inner()
    }

    #[tokio::test]
    async fn enabling_ingress_on_a_latency_critical_target_needs_the_opt_in() {
        // Putting a reverse proxy in front of a box tuned for latency is exactly
        // the kind of change the flag exists to make somebody say out loud. It
        // is the same rule as every other mutation of that host, rather than a
        // second rule to learn.
        let service = service().await;
        let created = service
            .create(Request::new(CreateTargetRequest {
                mutation: Some(Mutation {
                    actor: Some(Actor::human("u", "alice")),
                    allow_latency_critical: true,
                    ..Default::default()
                }),
                ..create("hot-box", true)
            }))
            .await
            .expect("create")
            .into_inner();

        let refused = service
            .enable_ingress(Request::new(EnableIngressRequest {
                mutation: Some(Mutation::by(Actor::human("usr_1", "alice"))),
                target_id: created.id.clone(),
                mode: ingress::Mode::Managed as i32,
                ..Default::default()
            }))
            .await
            .expect_err("must be refused without the opt-in");
        assert_eq!(refused.code(), tonic::Code::FailedPrecondition);
        assert!(
            refused.message().contains("latency-critical"),
            "got: {}",
            refused.message()
        );

        // And is allowed when the caller says so.
        let allowed = service
            .enable_ingress(Request::new(EnableIngressRequest {
                mutation: Some(Mutation {
                    actor: Some(Actor::human("u", "alice")),
                    allow_latency_critical: true,
                    ..Default::default()
                }),
                target_id: created.id,
                mode: ingress::Mode::Managed as i32,
                ..Default::default()
            }))
            .await
            .expect("allowed with the opt-in")
            .into_inner();
        assert_eq!(
            allowed.ingress.expect("ingress").mode,
            ingress::Mode::Managed as i32
        );
    }

    #[tokio::test]
    async fn ingress_defaults_to_caddys_admin_port() {
        let service = service().await;
        let target = target_with_ingress(&service, "prod-1").await;
        let ingress = target.ingress.expect("ingress");
        assert_eq!(ingress.admin_port, nudo_proto::DEFAULT_ADMIN_PORT);
        assert_eq!(ingress.status, ingress::Status::Pending as i32);
    }

    #[tokio::test]
    async fn enabling_ingress_without_a_mode_is_refused() {
        // The zero value means "no ingress", so accepting it here would silently
        // do nothing while reporting success.
        let service = service().await;
        let created = service
            .create(Request::new(create("prod-1", false)))
            .await
            .expect("create")
            .into_inner();

        let status = service
            .enable_ingress(Request::new(EnableIngressRequest {
                mutation: Some(Mutation::by(Actor::human("usr_1", "alice"))),
                target_id: created.id,
                ..Default::default()
            }))
            .await
            .expect_err("must be refused");
        assert_eq!(status.code(), tonic::Code::InvalidArgument);
    }

    #[tokio::test]
    async fn a_target_without_ingress_has_none_rather_than_a_disabled_one() {
        // Every target that predates this feature reads exactly as it did.
        let service = service().await;
        let created = service
            .create(Request::new(create("prod-1", false)))
            .await
            .expect("create")
            .into_inner();
        assert!(created.ingress.is_none());
    }

    #[tokio::test]
    async fn rendering_the_config_needs_no_host() {
        // Read-only and rendered from the database, so it works on a target that
        // is unreachable — and it is the whole of what external mode offers.
        let service = service().await;
        let target = target_with_ingress(&service, "prod-1").await;

        let rendered = service
            .render_ingress(Request::new(RenderIngressRequest {
                target_id: target.id,
            }))
            .await
            .expect("render")
            .into_inner();

        assert_eq!(rendered.path, crate::ingress::CONFIG_PATH);
        assert!(rendered.config.contains("admin 127.0.0.1:2019"));
        assert!(rendered.routes.is_empty());
    }

    #[tokio::test]
    async fn reloading_a_target_without_managed_ingress_is_refused() {
        // External means the operator drives their own proxy. Reloading would
        // mean touching a host nudo was told to stay off.
        let service = service().await;
        let created = service
            .create(Request::new(create("prod-1", false)))
            .await
            .expect("create")
            .into_inner();

        service
            .enable_ingress(Request::new(EnableIngressRequest {
                mutation: Some(Mutation::by(Actor::human("usr_1", "alice"))),
                target_id: created.id.clone(),
                mode: ingress::Mode::External as i32,
                ..Default::default()
            }))
            .await
            .expect("enable");

        let status = service
            .reload_ingress(Request::new(ReloadIngressRequest {
                mutation: Some(Mutation::by(Actor::human("usr_1", "alice"))),
                target_id: created.id,
            }))
            .await
            .expect_err("must be refused");
        assert_eq!(status.code(), tonic::Code::FailedPrecondition);
        assert!(
            status.message().contains("managed ingress"),
            "got: {}",
            status.message()
        );
    }

    #[tokio::test]
    async fn disabling_ingress_clears_the_mode() {
        let service = service().await;
        let target = target_with_ingress(&service, "prod-1").await;

        let disabled = service
            .disable_ingress(Request::new(DisableIngressRequest {
                mutation: Some(Mutation::by(Actor::human("usr_1", "alice"))),
                target_id: target.id,
            }))
            .await
            .expect("disable")
            .into_inner();

        assert!(
            disabled.ingress.is_none(),
            "a target with ingress off reads the same as one that never had it"
        );
    }
}
