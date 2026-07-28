//! The `BuildHosts` service.
//!
//! Mirrors `Targets` in shape, because an operator registering a build host is
//! doing the same job as registering a target: name it, say how to reach it,
//! then check it. It is deliberately a separate service rather than flags on
//! `Targets`, so a build host can never be handed to a deploy and a target can
//! never be handed a build.

use nudo_proto::build_hosts_server::BuildHosts;
use nudo_proto::*;
use tonic::{Request, Response, Status};

use super::{Context, internal};
use crate::store::{BuildHostInput, page_offset, page_size};

pub struct BuildHostsService {
    context: Context,
}

impl BuildHostsService {
    pub fn new(context: Context) -> Self {
        Self { context }
    }
}

#[tonic::async_trait]
impl BuildHosts for BuildHostsService {
    async fn create(
        &self,
        request: Request<CreateBuildHostRequest>,
    ) -> Result<Response<BuildHost>, Status> {
        let request = request.into_inner();

        // As for targets, the guardrail is checked against the host being
        // *created*, so a client cannot register a latency-critical build host
        // without saying so.
        let intended = BuildHost {
            name: request.name.clone(),
            latency_critical: request.latency_critical,
            ..Default::default()
        };
        let authorized = self
            .context
            .authorize_build_host(
                request.mutation.as_ref(),
                "BuildHosts.Create",
                "",
                request.latency_critical.then_some(&intended),
                format!("created build host {} ({})", request.name, request.host),
            )
            .await?;

        if authorized.dry_run {
            return Ok(Response::new(BuildHost {
                name: request.name,
                host: request.host,
                port: if request.port == 0 { 22 } else { request.port },
                user: request.user,
                ssh_key_id: request.ssh_key_id,
                workspace_root: if request.workspace_root.trim().is_empty() {
                    crate::store::DEFAULT_WORKSPACE_ROOT.to_string()
                } else {
                    request.workspace_root
                },
                latency_critical: request.latency_critical,
                labels: request.labels,
                status: build_host::Status::Unknown as i32,
                ..Default::default()
            }));
        }

        let created = self
            .context
            .store
            .create_build_host(&BuildHostInput {
                name: request.name,
                host: request.host,
                port: request.port,
                user: request.user,
                ssh_key_id: request.ssh_key_id,
                workspace_root: request.workspace_root,
                latency_critical: request.latency_critical,
                labels: request.labels,
            })
            .await
            .map_err(super::invalid)?;

        Ok(Response::new(created))
    }

    async fn get(
        &self,
        request: Request<GetBuildHostRequest>,
    ) -> Result<Response<BuildHost>, Status> {
        let build_host = self
            .context
            .require_build_host(&request.into_inner().id)
            .await?;
        Ok(Response::new(build_host))
    }

    async fn list(
        &self,
        request: Request<ListBuildHostsRequest>,
    ) -> Result<Response<ListBuildHostsResponse>, Status> {
        let request = request.into_inner();
        let limit = page_size(request.page_size);
        let offset = page_offset(&request.page_token);

        let build_hosts = self
            .context
            .store
            .list_build_hosts(&request.label_selector, limit, offset)
            .await
            .map_err(internal)?;

        let next_page_token = crate::store::next_page_token(offset, build_hosts.len(), limit);
        Ok(Response::new(ListBuildHostsResponse {
            build_hosts,
            next_page_token,
        }))
    }

    async fn update(
        &self,
        request: Request<UpdateBuildHostRequest>,
    ) -> Result<Response<BuildHost>, Status> {
        let request = request.into_inner();
        let existing = self.context.require_build_host(&request.id).await?;

        let authorized = self
            .context
            .authorize_build_host(
                request.mutation.as_ref(),
                "BuildHosts.Update",
                &request.id,
                Some(&existing),
                format!("updated build host {}", existing.name),
            )
            .await?;

        let update = request
            .build_host
            .ok_or_else(|| Status::invalid_argument("update requires a build host"))?;

        if authorized.dry_run {
            return Ok(Response::new(existing));
        }

        let updated = self
            .context
            .store
            .update_build_host(&request.id, &update, &request.update_mask)
            .await
            .map_err(super::invalid)?;

        Ok(Response::new(updated))
    }

    async fn delete(
        &self,
        request: Request<DeleteBuildHostRequest>,
    ) -> Result<Response<()>, Status> {
        let request = request.into_inner();
        let existing = self.context.require_build_host(&request.id).await?;

        // Services pointing here keep pointing here and will fail their next
        // build with a message naming this id, rather than silently falling
        // back to the default — which would move a build nobody asked to move.
        // Saying so up front is the difference between that being a surprise
        // and being a decision.
        let dependants = self
            .context
            .store
            .services_using_build_host(&request.id)
            .await
            .map_err(internal)?;
        let summary = if dependants.is_empty() {
            format!("deleted build host {}", existing.name)
        } else {
            format!(
                "deleted build host {}, leaving {} service(s) pointing at it: {}",
                existing.name,
                dependants.len(),
                dependants.join(", ")
            )
        };

        let authorized = self
            .context
            .authorize_build_host(
                request.mutation.as_ref(),
                "BuildHosts.Delete",
                &request.id,
                Some(&existing),
                summary,
            )
            .await?;

        if authorized.dry_run {
            return Ok(Response::new(()));
        }

        self.context
            .store
            .delete_build_host(&request.id)
            .await
            .map_err(super::invalid)?;

        // An instance default pointing at a deleted host would fail every
        // git-backed deploy at once, so it is cleared rather than left dangling.
        // A service's own setting is left alone deliberately: it is a per-service
        // decision that should fail loudly, not be reinterpreted.
        if self
            .context
            .store
            .default_build_host_id()
            .await
            .unwrap_or_default()
            == request.id
            && let Err(error) = self.context.store.set_default_build_host_id("").await
        {
            tracing::warn!(%error, "clearing the deleted build host as the instance default failed");
        }

        Ok(Response::new(()))
    }

    async fn check(
        &self,
        request: Request<CheckBuildHostRequest>,
    ) -> Result<Response<CheckBuildHostResponse>, Status> {
        let build_host = self
            .context
            .require_build_host(&request.into_inner().id)
            .await?;

        // Read-only, so allowed against a latency-critical host without an
        // opt-in — the host you most want to verify must not be the one you
        // cannot. The warning about contention is reported in the response.
        let ssh_target = self
            .context
            .engine
            .ssh_target_for_build_host(&build_host)
            .await
            .map_err(|error| Status::failed_precondition(format!("{error:#}")))?;

        // `build-hosts check` is what an operator runs after registering one, so
        // this is the natural place for first-use pinning to happen and be
        // reported. A refused connection is a check result, not an error.
        let connection = match crate::ssh::SshSession::connect(&ssh_target).await {
            Ok(session) => {
                if let crate::ssh::HostKeyOutcome::Pinned { key, fingerprint } = session.host_key()
                {
                    if let Err(error) = self
                        .context
                        .store
                        .pin_build_host_key(&build_host.id, key, fingerprint)
                        .await
                    {
                        tracing::warn!(%error, "pinning the build host's key failed");
                    }
                } else if let Err(error) = self
                    .context
                    .store
                    .clear_pending_build_host_key(&build_host.id)
                    .await
                {
                    tracing::warn!(%error, "clearing the pending host key failed");
                }
                Ok(session)
            }
            Err(error) => {
                if let Some(changed) = error.downcast_ref::<crate::ssh::HostKeyChanged>()
                    && let Err(error) = self
                        .context
                        .store
                        .record_pending_build_host_key(
                            &build_host.id,
                            &changed.key,
                            &changed.fingerprint,
                        )
                        .await
                {
                    tracing::warn!(%error, "recording the changed host key failed");
                }
                Err(error)
            }
        };

        let (ok, checks, warnings) = crate::probe::check_build_host(
            connection,
            &build_host.workspace_root,
            build_host.latency_critical,
        )
        .await;

        let status = if ok {
            build_host::Status::Reachable
        } else {
            build_host::Status::Unreachable
        };
        if let Err(error) = self
            .context
            .store
            .set_build_host_status(&build_host.id, status)
            .await
        {
            tracing::warn!(%error, "recording build host status failed");
        }

        Ok(Response::new(CheckBuildHostResponse {
            ok,
            checks,
            warnings,
        }))
    }

    async fn accept_host_key(
        &self,
        request: Request<AcceptBuildHostKeyRequest>,
    ) -> Result<Response<BuildHost>, Status> {
        let request = request.into_inner();
        let existing = self.context.require_build_host(&request.id).await?;

        let host_key = existing
            .host_key
            .clone()
            .filter(|k| !k.pending_key.is_empty())
            .ok_or_else(|| {
                Status::failed_precondition(format!(
                    "build host {} has no host-key change waiting to be accepted",
                    existing.name
                ))
            })?;

        // The fingerprint names the key that was reviewed. Without this, an
        // acceptance would apply to whatever is pending when the request lands,
        // which may not be what the operator looked at.
        let offered = request.fingerprint.trim();
        if offered != host_key.pending_fingerprint {
            return Err(Status::failed_precondition(format!(
                "the pending key for {} is {}, not {offered} — review the current \
                 change before accepting it",
                existing.name, host_key.pending_fingerprint
            )));
        }

        let authorized = self
            .context
            .authorize_build_host(
                request.mutation.as_ref(),
                "BuildHosts.AcceptHostKey",
                &request.id,
                Some(&existing),
                format!(
                    "accepted a new ssh host key for build host {}: {}",
                    existing.name, host_key.pending_fingerprint
                ),
            )
            .await?;

        if authorized.dry_run {
            return Ok(Response::new(existing));
        }

        self.context
            .store
            .pin_build_host_key(
                &request.id,
                &host_key.pending_key,
                &host_key.pending_fingerprint,
            )
            .await
            .map_err(internal)?;

        Ok(Response::new(
            self.context.require_build_host(&request.id).await?,
        ))
    }

    async fn forget_host_key(
        &self,
        request: Request<ForgetBuildHostKeyRequest>,
    ) -> Result<Response<BuildHost>, Status> {
        let request = request.into_inner();
        let existing = self.context.require_build_host(&request.id).await?;

        let authorized = self
            .context
            .authorize_build_host(
                request.mutation.as_ref(),
                "BuildHosts.ForgetHostKey",
                &request.id,
                Some(&existing),
                format!(
                    "forgot the pinned ssh host key for build host {}, reopening \
                     trust-on-first-use",
                    existing.name
                ),
            )
            .await?;

        if authorized.dry_run {
            return Ok(Response::new(existing));
        }

        self.context
            .store
            .forget_build_host_key(&request.id)
            .await
            .map_err(super::invalid)?;

        Ok(Response::new(
            self.context.require_build_host(&request.id).await?,
        ))
    }

    async fn get_defaults(
        &self,
        _request: Request<GetBuildDefaultsRequest>,
    ) -> Result<Response<BuildDefaults>, Status> {
        let build_host_id = self
            .context
            .store
            .default_build_host_id()
            .await
            .map_err(internal)?;
        Ok(Response::new(BuildDefaults { build_host_id }))
    }

    async fn set_defaults(
        &self,
        request: Request<SetBuildDefaultsRequest>,
    ) -> Result<Response<BuildDefaults>, Status> {
        let request = request.into_inner();
        let id = request.build_host_id.trim().to_string();

        // Naming the host in the audit summary rather than only its id, since
        // this setting changes where every unpinned service builds.
        let summary = if id.is_empty() || id == LOCAL_BUILD_HOST_ID {
            "set the default build location to the control plane".to_string()
        } else {
            let name = self
                .context
                .store
                .get_build_host(&id)
                .await
                .map_err(internal)?
                .map(|h| h.name)
                .unwrap_or_else(|| id.clone());
            format!("set the default build host to {name}")
        };

        let authorized = self
            .context
            .authorize_build_host(
                request.mutation.as_ref(),
                "BuildHosts.SetDefaults",
                &id,
                None,
                summary,
            )
            .await?;

        if authorized.dry_run {
            return Ok(Response::new(BuildDefaults {
                build_host_id: self
                    .context
                    .store
                    .default_build_host_id()
                    .await
                    .map_err(internal)?,
            }));
        }

        self.context
            .store
            .set_default_build_host_id(&id)
            .await
            .map_err(super::invalid)?;

        Ok(Response::new(BuildDefaults { build_host_id: id }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::Store;
    use nudo_proto::Actor;

    async fn service() -> BuildHostsService {
        BuildHostsService::new(crate::api::test_support::context().await)
    }

    fn mutation() -> Option<Mutation> {
        Some(Mutation {
            actor: Some(Actor::human("u", "alice")),
            ..Default::default()
        })
    }

    fn create(name: &str, latency_critical: bool) -> CreateBuildHostRequest {
        CreateBuildHostRequest {
            mutation: mutation(),
            name: name.to_string(),
            host: "10.0.0.9".to_string(),
            port: 22,
            user: "build".to_string(),
            latency_critical,
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn a_created_build_host_comes_back_with_its_defaults_applied() {
        let service = service().await;
        let created = service
            .create(Request::new(create("builder", false)))
            .await
            .expect("create")
            .into_inner();

        assert!(created.id.starts_with("bh_"));
        assert_eq!(created.name, "builder");
        assert_eq!(created.workspace_root, crate::store::DEFAULT_WORKSPACE_ROOT);
        assert_eq!(created.status, build_host::Status::Unknown as i32);
    }

    #[tokio::test]
    async fn a_dry_run_create_returns_the_plan_without_writing_it() {
        let service = service().await;
        let planned = service
            .create(Request::new(CreateBuildHostRequest {
                mutation: Some(Mutation {
                    actor: Some(Actor::human("u", "alice")),
                    dry_run: true,
                    ..Default::default()
                }),
                ..create("builder", false)
            }))
            .await
            .expect("dry run")
            .into_inner();

        assert_eq!(planned.name, "builder");
        assert!(planned.id.is_empty(), "a dry run must not allocate an id");

        let listed = service
            .list(Request::new(ListBuildHostsRequest::default()))
            .await
            .expect("list")
            .into_inner();
        assert!(listed.build_hosts.is_empty(), "a dry run wrote a row");
    }

    #[tokio::test]
    async fn creating_a_latency_critical_build_host_needs_the_opt_in() {
        // Permitted, but not by accident: the flag says the operator knows a
        // build here will contend with whatever else runs on the box.
        let service = service().await;
        let refused = service
            .create(Request::new(create("hot-box", true)))
            .await
            .expect_err("must be refused without the opt-in");
        assert_eq!(refused.code(), tonic::Code::FailedPrecondition);
        assert!(
            refused.message().contains("build host"),
            "the message must not call it a target: {}",
            refused.message()
        );

        let allowed = service
            .create(Request::new(CreateBuildHostRequest {
                mutation: Some(Mutation {
                    actor: Some(Actor::human("u", "alice")),
                    allow_latency_critical: true,
                    ..Default::default()
                }),
                ..create("hot-box", true)
            }))
            .await
            .expect("allowed with the opt-in")
            .into_inner();
        assert!(allowed.latency_critical);
    }

    #[tokio::test]
    async fn an_unknown_build_host_is_not_found_rather_than_an_internal_error() {
        let service = service().await;
        let error = service
            .get(Request::new(GetBuildHostRequest {
                id: "bh_missing".to_string(),
            }))
            .await
            .expect_err("must fail");
        assert_eq!(error.code(), tonic::Code::NotFound);
    }

    #[tokio::test]
    async fn an_instance_defaults_to_the_control_plane_and_can_be_pointed_at_a_host() {
        let service = service().await;
        let defaults = service
            .get_defaults(Request::new(GetBuildDefaultsRequest {}))
            .await
            .expect("defaults")
            .into_inner();
        assert_eq!(
            defaults.build_host_id, "",
            "an instance that configures nothing builds on the control plane"
        );

        let created = service
            .create(Request::new(create("builder", false)))
            .await
            .expect("create")
            .into_inner();

        let set = service
            .set_defaults(Request::new(SetBuildDefaultsRequest {
                mutation: mutation(),
                build_host_id: created.id.clone(),
            }))
            .await
            .expect("set")
            .into_inner();
        assert_eq!(set.build_host_id, created.id);
    }

    #[tokio::test]
    async fn an_instance_default_naming_a_missing_build_host_is_refused() {
        let service = service().await;
        let error = service
            .set_defaults(Request::new(SetBuildDefaultsRequest {
                mutation: mutation(),
                build_host_id: "bh_missing".to_string(),
            }))
            .await
            .expect_err("must fail");
        assert_eq!(error.code(), tonic::Code::InvalidArgument);
    }

    #[tokio::test]
    async fn deleting_the_default_build_host_returns_the_instance_to_the_control_plane() {
        // Left dangling, this setting would fail every git-backed deploy on the
        // instance at once.
        let service = service().await;
        let created = service
            .create(Request::new(create("builder", false)))
            .await
            .expect("create")
            .into_inner();
        service
            .set_defaults(Request::new(SetBuildDefaultsRequest {
                mutation: mutation(),
                build_host_id: created.id.clone(),
            }))
            .await
            .expect("set");

        service
            .delete(Request::new(DeleteBuildHostRequest {
                mutation: mutation(),
                id: created.id.clone(),
            }))
            .await
            .expect("delete");

        let defaults = service
            .get_defaults(Request::new(GetBuildDefaultsRequest {}))
            .await
            .expect("defaults")
            .into_inner();
        assert_eq!(defaults.build_host_id, "");
    }

    #[tokio::test]
    async fn accepting_a_host_key_requires_a_pending_change() {
        let service = service().await;
        let created = service
            .create(Request::new(create("builder", false)))
            .await
            .expect("create")
            .into_inner();

        let error = service
            .accept_host_key(Request::new(AcceptBuildHostKeyRequest {
                mutation: mutation(),
                id: created.id,
                fingerprint: "SHA256:whatever".to_string(),
            }))
            .await
            .expect_err("nothing is pending");
        assert_eq!(error.code(), tonic::Code::FailedPrecondition);
    }

    #[tokio::test]
    async fn accepting_a_host_key_refuses_a_fingerprint_that_is_not_the_pending_one() {
        // What makes an acceptance refer to the key the operator reviewed
        // rather than to whatever is pending when the request lands.
        let context = crate::api::test_support::context().await;
        let service = BuildHostsService::new(context.clone());
        let created = service
            .create(Request::new(create("builder", false)))
            .await
            .expect("create")
            .into_inner();

        context
            .store
            .pin_build_host_key(&created.id, "ssh-ed25519 OLD", "SHA256:old")
            .await
            .expect("pin");
        context
            .store
            .record_pending_build_host_key(&created.id, "ssh-ed25519 NEW", "SHA256:new")
            .await
            .expect("pending");

        let error = service
            .accept_host_key(Request::new(AcceptBuildHostKeyRequest {
                mutation: mutation(),
                id: created.id.clone(),
                fingerprint: "SHA256:something-else".to_string(),
            }))
            .await
            .expect_err("must refuse a mismatched fingerprint");
        assert_eq!(error.code(), tonic::Code::FailedPrecondition);

        // The right fingerprint promotes it.
        let accepted = service
            .accept_host_key(Request::new(AcceptBuildHostKeyRequest {
                mutation: mutation(),
                id: created.id,
                fingerprint: "SHA256:new".to_string(),
            }))
            .await
            .expect("accept")
            .into_inner();
        let key = accepted.host_key.expect("host key");
        assert_eq!(key.key, "ssh-ed25519 NEW");
        assert!(key.pending_key.is_empty());
    }

    #[tokio::test]
    async fn deleting_a_build_host_records_the_services_left_pointing_at_it() {
        // They keep pointing at it and fail their next build with a message
        // naming it, rather than being silently moved to the default.
        let context = crate::api::test_support::context().await;
        let service = BuildHostsService::new(context.clone());
        let created = service
            .create(Request::new(create("builder", false)))
            .await
            .expect("create")
            .into_inner();

        let target = context
            .store
            .create_target(&crate::store::TargetInput {
                name: "edge".to_string(),
                host: "10.0.0.5".to_string(),
                ..Default::default()
            })
            .await
            .expect("target");
        context
            .store
            .create_service(&Service {
                target_id: target.id,
                name: "bot".to_string(),
                artifact: Some(ArtifactSource {
                    kind: Some(artifact_source::Kind::Git(GitSource {
                        repo: "o/r".to_string(),
                        build_command: "make".to_string(),
                        artifact_path: "bot".to_string(),
                        build_host_id: created.id.clone(),
                        ..Default::default()
                    })),
                }),
                ..Default::default()
            })
            .await
            .expect("service");

        service
            .delete(Request::new(DeleteBuildHostRequest {
                mutation: mutation(),
                id: created.id.clone(),
            }))
            .await
            .expect("delete");

        let entries = context
            .store
            .list_audit("", actor::Kind::Unspecified, 50, 0)
            .await
            .expect("audit");
        assert!(
            entries
                .iter()
                .any(|e| e.action == "BuildHosts.Delete" && e.summary.contains("bot")),
            "the deletion must record what was left pointing at it: {entries:?}"
        );
    }

    #[tokio::test]
    async fn a_build_host_and_a_target_can_share_a_name_without_colliding() {
        // They are separate entities; a machine that is both a target and a
        // build host is a configuration error, not a naming one.
        let context = crate::api::test_support::context().await;
        let service = BuildHostsService::new(context.clone());

        context
            .store
            .create_target(&crate::store::TargetInput {
                name: "shared-name".to_string(),
                host: "10.0.0.5".to_string(),
                ..Default::default()
            })
            .await
            .expect("target");

        service
            .create(Request::new(create("shared-name", false)))
            .await
            .expect("a build host may share a target's name");
    }

    // Keeps the unused-import warning away in the test module.
    #[allow(dead_code)]
    fn _store_is_used(_: &Store) {}
}
