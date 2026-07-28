//! The `Targets` service.

use nudo_proto::targets_server::Targets;
use nudo_proto::*;
use tonic::{Request, Response, Status};

use super::{Context, internal};
use crate::store::{TargetInput, page_offset, page_size};

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
            .connect_ssh(&target.id, &ssh_target)
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
            .forget_host_key(&request.id)
            .await
            .map_err(super::invalid)?;

        Ok(Response::new(
            self.context.require_target(&request.id).await?,
        ))
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
            .pin_host_key(&created.id, KEY_A, "SHA256:aaa")
            .await
            .expect("pin");
        service
            .context
            .store
            .record_pending_host_key(&created.id, KEY_B, "SHA256:bbb")
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
            .pin_host_key(&created.id, KEY_A, "SHA256:aaa")
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
            .record_pending_host_key(&created.id, KEY_B, "SHA256:bbb")
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
}
