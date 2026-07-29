//! The `ServicesApi` service: service CRUD, unit rendering and unit control.

use std::pin::Pin;
use std::time::Duration;

use futures_util::Stream;
use nudo_proto::services_api_server::ServicesApi;
use nudo_proto::*;
use tonic::{Request, Response, Status};

use super::{Context, internal};
use crate::store::{page_offset, page_size};
use crate::{systemd, units};

/// How often the dashboard's service list is refreshed.
///
/// Each tick is one SSH connection per distinct target, so this trades staleness
/// against load on hosts that are not supposed to be doing anything else.
const WATCH_INTERVAL: Duration = Duration::from_secs(5);

pub struct ServicesApiService {
    context: Context,
}

impl ServicesApiService {
    pub fn new(context: Context) -> Self {
        Self { context }
    }
}

#[tonic::async_trait]
impl ServicesApi for ServicesApiService {
    async fn create(
        &self,
        request: Request<CreateServiceRequest>,
    ) -> Result<Response<Service>, Status> {
        let request = request.into_inner();
        let service = request
            .service
            .ok_or_else(|| Status::invalid_argument("create requires a service"))?;

        let target = self.context.require_target(&service.target_id).await?;
        let authorized = self
            .context
            .authorize(
                request.mutation.as_ref(),
                "ServicesApi.Create",
                "",
                Some(&target),
                format!("created service {} on {}", service.name, target.name),
            )
            .await?;

        if authorized.dry_run {
            return Ok(Response::new(service));
        }

        let created = self
            .context
            .store
            .create_service(&service)
            .await
            .map_err(super::invalid)?;
        Ok(Response::new(created))
    }

    async fn get(&self, request: Request<GetServiceRequest>) -> Result<Response<Service>, Status> {
        let service = self
            .context
            .require_service(&request.into_inner().id)
            .await?;
        Ok(Response::new(service))
    }

    async fn list(
        &self,
        request: Request<ListServicesRequest>,
    ) -> Result<Response<ListServicesResponse>, Status> {
        let request = request.into_inner();
        let limit = page_size(request.page_size);
        let offset = page_offset(&request.page_token);

        let services = self
            .context
            .store
            .list_services(&request.target_id, limit, offset)
            .await
            .map_err(internal)?;

        let next_page_token = crate::store::next_page_token(offset, services.len(), limit);
        Ok(Response::new(ListServicesResponse {
            services,
            next_page_token,
        }))
    }

    async fn update(
        &self,
        request: Request<UpdateServiceRequest>,
    ) -> Result<Response<Service>, Status> {
        let request = request.into_inner();
        let (existing, target) = self.context.require_service_and_target(&request.id).await?;

        let authorized = self
            .context
            .authorize(
                request.mutation.as_ref(),
                "ServicesApi.Update",
                &request.id,
                Some(&target),
                format!("updated service {}", existing.name),
            )
            .await?;

        let update = request
            .service
            .ok_or_else(|| Status::invalid_argument("update requires a service"))?;

        if authorized.dry_run {
            return Ok(Response::new(existing));
        }

        let updated = self
            .context
            .store
            .update_service(&request.id, &update, &request.update_mask)
            .await
            .map_err(super::invalid)?;

        // A domain change takes effect now rather than at the next deploy.
        // Setting a domain and finding the site still unreachable until
        // something unrelated is deployed would be a confusing way to learn how
        // this works.
        //
        // Compared against what was there before so an update that did not
        // touch routing does not reload the proxy for nothing.
        let route_changed = updated.routes != existing.routes;
        if route_changed
            && crate::ingress::is_managed(&target)
            && let Err(error) = self.reload_ingress(&target).await
        {
            tracing::warn!(
                %error,
                service = %updated.name,
                "reloading the proxy after a route change failed"
            );
        }

        Ok(Response::new(updated))
    }

    async fn delete(&self, request: Request<DeleteServiceRequest>) -> Result<Response<()>, Status> {
        let request = request.into_inner();
        let (existing, target) = self.context.require_service_and_target(&request.id).await?;
        let unit_name = systemd::unit_file_name(&existing);

        let mut summary = format!("deleted service {}", existing.name);
        if request.stop_and_disable_unit {
            summary.push_str(&format!(", stopped and disabled {unit_name}"));
        }
        if request.remove_release_dir {
            summary.push_str(&format!(", removed {}", existing.release_root));
        }

        let authorized = self
            .context
            .authorize(
                request.mutation.as_ref(),
                "ServicesApi.Delete",
                &request.id,
                Some(&target),
                summary,
            )
            .await?;

        if authorized.dry_run {
            return Ok(Response::new(()));
        }

        // Validated before connecting, because it is a property of the request
        // rather than of the host: a service row whose release root is `/` must
        // be refused whether or not the target happens to be reachable.
        let release_dir = if request.remove_release_dir {
            let root = existing.release_root.trim().trim_end_matches('/');
            // Requiring at least two slashes means `/opt/bot` is fine but `/`,
            // `/opt` and an empty value are not — none of those is a directory
            // this tool created, and `rm -rf` on them would be catastrophic.
            if root.is_empty() || root == "/" || root.matches('/').count() < 2 {
                return Err(Status::failed_precondition(format!(
                    "refusing to remove {root:?}: it is not a service-specific directory"
                )));
            }
            Some(root.to_string())
        } else {
            None
        };

        // Touch the host first: if that fails, the row stays and the operator
        // can retry, rather than losing the only record of what is running
        // there.
        if request.stop_and_disable_unit || release_dir.is_some() {
            let session = self.context.connect(&target).await?;

            if request.stop_and_disable_unit {
                // A unit that is already stopped or was never installed makes
                // these fail, which must not block the delete.
                let _ = session
                    .exec(&format!(
                        "systemctl disable --now {}",
                        crate::ssh::quote(&unit_name)
                    ))
                    .await;
                let _ = session
                    .exec(&format!(
                        "rm -f {}",
                        crate::ssh::quote(&systemd::unit_file_path(&existing))
                    ))
                    .await;
                let _ = session.exec("systemctl daemon-reload").await;
            }

            if let Some(root) = &release_dir {
                session
                    .exec(&format!("rm -rf {}", crate::ssh::quote(root)))
                    .await
                    .map_err(internal)?
                    .require_success("removing the release directory")
                    .map_err(internal)?;
            }

            let _ = session.close().await;
        }

        self.context
            .store
            .delete_service(&request.id)
            .await
            .map_err(super::invalid)?;
        self.context.bus.forget_service(&request.id);

        // Take the route away after the row is gone, since the config is
        // rendered from the database — doing it first would re-render a config
        // that still contained this service. Only for a service that had a
        // route: nothing else changes the proxy's config.
        //
        // A failure here does not fail the delete. The service is already gone,
        // and the worst case is a route pointing at a port nothing answers on,
        // which the target's ingress check reports.
        if !existing.routes.is_empty()
            && crate::ingress::is_managed(&target)
            && let Err(error) = self.reload_ingress(&target).await
        {
            tracing::warn!(
                %error,
                service = %existing.name,
                "removing the deleted service's route failed"
            );
        }

        Ok(Response::new(()))
    }

    async fn render_unit(
        &self,
        request: Request<RenderUnitRequest>,
    ) -> Result<Response<RenderUnitResponse>, Status> {
        let service = self
            .context
            .require_service(&request.into_inner().service_id)
            .await?;

        // The same function the deploy engine uses, so a preview cannot differ
        // from what actually gets written.
        Ok(Response::new(RenderUnitResponse {
            unit_file: systemd::render_unit(&service),
        }))
    }

    async fn unit_action(
        &self,
        request: Request<UnitActionRequest>,
    ) -> Result<Response<UnitStatus>, Status> {
        let request = request.into_inner();
        let action = unit_action_request::Action::try_from(request.action)
            .map_err(|_| Status::invalid_argument("unknown unit action"))?;
        let verb = units::action_verb(action).map_err(super::invalid)?;

        let (service, target) = self
            .context
            .require_service_and_target(&request.service_id)
            .await?;
        let unit_name = systemd::unit_file_name(&service);

        let authorized = self
            .context
            .authorize(
                request.mutation.as_ref(),
                "ServicesApi.UnitAction",
                &request.service_id,
                Some(&target),
                format!("{verb} {unit_name} on {}", target.name),
            )
            .await?;

        let session = self.context.connect(&target).await?;

        if authorized.dry_run {
            // Report current state rather than acting.
            let status = units::read_status(&session, &service, &unit_name)
                .await
                .unwrap_or_else(|_| units::unknown_status(&service.id));
            let _ = session.close().await;
            return Ok(Response::new(status));
        }

        units::run_action(&session, &unit_name, action)
            .await
            .map_err(|error| Status::failed_precondition(format!("{error:#}")))?;

        let status = units::read_status(&session, &service, &unit_name)
            .await
            .unwrap_or_else(|_| units::unknown_status(&service.id));
        let _ = session.close().await;

        Ok(Response::new(status))
    }

    async fn get_unit_status(
        &self,
        request: Request<GetUnitStatusRequest>,
    ) -> Result<Response<UnitStatus>, Status> {
        let (service, target) = self
            .context
            .require_service_and_target(&request.into_inner().service_id)
            .await?;
        let unit_name = systemd::unit_file_name(&service);

        // An unreachable host yields an "unknown" status rather than an error:
        // the dashboard has to render the row either way.
        let session = match self.context.connect(&target).await {
            Ok(session) => session,
            Err(_) => return Ok(Response::new(units::unknown_status(&service.id))),
        };

        let status = units::read_status(&session, &service, &unit_name)
            .await
            .unwrap_or_else(|_| units::unknown_status(&service.id));
        let _ = session.close().await;

        Ok(Response::new(status))
    }

    type WatchUnitStatusStream =
        Pin<Box<dyn Stream<Item = Result<UnitStatus, Status>> + Send + 'static>>;

    async fn watch_unit_status(
        &self,
        request: Request<ListServicesRequest>,
    ) -> Result<Response<Self::WatchUnitStatusStream>, Status> {
        let request = request.into_inner();
        let context = self.context.clone();

        let stream = async_stream::try_stream! {
            let mut ticker = tokio::time::interval(WATCH_INTERVAL);

            loop {
                ticker.tick().await;

                let services = context
                    .store
                    .list_services(&request.target_id, 500, 0)
                    .await
                    .map_err(internal)?;

                // Group by target so a host with ten services gets one SSH
                // connection per tick, not ten.
                let mut by_target: std::collections::HashMap<String, Vec<Service>> =
                    std::collections::HashMap::new();
                for service in services {
                    by_target
                        .entry(service.target_id.clone())
                        .or_default()
                        .push(service);
                }

                for (target_id, services) in by_target {
                    let Ok(target) = context.require_target(&target_id).await else {
                        for service in &services {
                            yield units::unknown_status(&service.id);
                        }
                        continue;
                    };

                    match context.connect(&target).await {
                        Ok(session) => {
                            for service in &services {
                                let unit_name = systemd::unit_file_name(service);
                                let status = units::read_status(&session, service, &unit_name)
                                    .await
                                    .unwrap_or_else(|_| units::unknown_status(&service.id));
                                yield status;
                            }
                            let _ = session.close().await;
                        }
                        Err(_) => {
                            for service in &services {
                                yield units::unknown_status(&service.id);
                            }
                        }
                    }
                }
            }
        };

        Ok(Response::new(Box::pin(stream)))
    }
}

impl ServicesApiService {
    /// Re-renders and reloads a target's proxy config.
    ///
    /// Used when a service's route changed outside a deploy — a domain set or
    /// cleared, or a routed service deleted. A deploy does this itself, on the
    /// SSH connection it already has open.
    ///
    /// Returns `Ok(())` when the proxy rejected the config as well as when it
    /// accepted it: the caller's own operation succeeded either way, and the
    /// target is recorded as degraded so the failure is visible where an
    /// operator looks for it.
    async fn reload_ingress(&self, target: &Target) -> anyhow::Result<()> {
        let services = self.context.store.routed_services(&target.id).await?;

        let ssh_target = self.context.engine.ssh_target_for(target).await?;
        let session = self
            .context
            .engine
            .connect_ssh(&target.id, &ssh_target)
            .await?;

        let outcome = crate::ingress::reconcile::reload(&session, target, &services).await;
        let _ = session.close().await;
        let outcome = outcome?;

        let status = if outcome.ok {
            nudo_proto::ingress::Status::Active
        } else {
            nudo_proto::ingress::Status::Degraded
        };
        self.context
            .store
            .set_ingress_status(
                &target.id,
                status,
                outcome.version.as_deref(),
                &outcome.error,
            )
            .await?;

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::test_support;

    async fn fixture() -> (ServicesApiService, String) {
        let (context, target) = test_support::context_with_target().await;
        (ServicesApiService::new(context), target.id)
    }

    fn create(target_id: &str, name: &str) -> CreateServiceRequest {
        CreateServiceRequest {
            mutation: Some(Mutation::by(Actor::human("usr_1", "alice"))),
            service: Some(Service {
                target_id: target_id.to_string(),
                name: name.to_string(),
                unit: Some(SystemdUnit {
                    unit_name: format!("{name}.service"),
                    cpu_affinity: "2-5".to_string(),
                    ..Default::default()
                }),
                ..Default::default()
            }),
        }
    }

    #[tokio::test]
    async fn a_service_is_created_and_readable() {
        let (service, target_id) = fixture().await;
        let created = service
            .create(Request::new(create(&target_id, "bot")))
            .await
            .expect("create")
            .into_inner();

        assert_eq!(created.name, "bot");
        assert_eq!(created.release_root, "/opt/bot");

        let fetched = service
            .get(Request::new(GetServiceRequest {
                id: created.id.clone(),
            }))
            .await
            .expect("get")
            .into_inner();
        assert_eq!(fetched.id, created.id);
    }

    #[tokio::test]
    async fn creating_a_service_without_the_message_is_an_invalid_argument() {
        let (service, _) = fixture().await;
        let status = service
            .create(Request::new(CreateServiceRequest {
                mutation: Some(Mutation::by(Actor::human("u", "alice"))),
                service: None,
            }))
            .await
            .expect_err("err");
        assert_eq!(status.code(), tonic::Code::InvalidArgument);
    }

    #[tokio::test]
    async fn creating_a_service_on_a_missing_target_is_not_found() {
        let (service, _) = fixture().await;
        let status = service
            .create(Request::new(create("tgt_nope", "bot")))
            .await
            .expect_err("err");
        assert_eq!(status.code(), tonic::Code::NotFound);
    }

    #[tokio::test]
    async fn creating_a_service_on_a_latency_critical_target_needs_the_opt_in() {
        let (service, _) = fixture().await;
        let hot = test_support::create_latency_critical_target(&service.context).await;

        let status = service
            .create(Request::new(create(&hot.id, "hft")))
            .await
            .expect_err("must be refused");
        assert_eq!(status.code(), tonic::Code::FailedPrecondition);
    }

    #[tokio::test]
    async fn a_dry_run_create_does_not_persist() {
        let (service, target_id) = fixture().await;
        service
            .create(Request::new(CreateServiceRequest {
                mutation: Some(Mutation {
                    actor: Some(Actor::agent("s", "claude")),
                    dry_run: true,
                    ..Default::default()
                }),
                ..create(&target_id, "phantom")
            }))
            .await
            .expect("dry run");

        let listed = service
            .list(Request::new(ListServicesRequest::default()))
            .await
            .expect("list")
            .into_inner();
        assert!(listed.services.is_empty());
    }

    #[tokio::test]
    async fn render_unit_returns_exactly_what_a_deploy_would_write() {
        // A preview that could differ from reality is worse than no preview, so
        // both go through systemd::render_unit.
        let (service, target_id) = fixture().await;
        let created = service
            .create(Request::new(create(&target_id, "bot")))
            .await
            .expect("create")
            .into_inner();

        let rendered = service
            .render_unit(Request::new(RenderUnitRequest {
                service_id: created.id.clone(),
            }))
            .await
            .expect("render")
            .into_inner()
            .unit_file;

        assert_eq!(rendered, systemd::render_unit(&created));
        assert!(rendered.contains("ExecStart=/opt/bot/current/bin"));
        assert!(rendered.contains("CPUAffinity=2-5"));
    }

    #[tokio::test]
    async fn rendering_a_missing_service_is_not_found() {
        let (service, _) = fixture().await;
        let status = service
            .render_unit(Request::new(RenderUnitRequest {
                service_id: "svc_nope".to_string(),
            }))
            .await
            .expect_err("err");
        assert_eq!(status.code(), tonic::Code::NotFound);
    }

    #[tokio::test]
    async fn listing_filters_by_target_and_paginates() {
        let (service, target_id) = fixture().await;
        let other = test_support::create_target(&service.context, "other", "10.0.0.3", false).await;

        service
            .create(Request::new(create(&target_id, "a")))
            .await
            .expect("a");
        service
            .create(Request::new(create(&other.id, "b")))
            .await
            .expect("b");

        let all = service
            .list(Request::new(ListServicesRequest::default()))
            .await
            .expect("list")
            .into_inner();
        assert_eq!(all.services.len(), 2);

        let filtered = service
            .list(Request::new(ListServicesRequest {
                target_id: target_id.clone(),
                ..Default::default()
            }))
            .await
            .expect("list")
            .into_inner();
        assert_eq!(filtered.services.len(), 1);
        assert_eq!(filtered.services[0].name, "a");
    }

    #[tokio::test]
    async fn an_update_applies_its_mask() {
        let (service, target_id) = fixture().await;
        let created = service
            .create(Request::new(create(&target_id, "bot")))
            .await
            .expect("create")
            .into_inner();

        let updated = service
            .update(Request::new(UpdateServiceRequest {
                mutation: Some(Mutation::by(Actor::human("u", "alice"))),
                id: created.id.clone(),
                service: Some(Service {
                    keep_releases: 10,
                    release_root: "/ignored".to_string(),
                    ..Default::default()
                }),
                update_mask: vec!["keep_releases".to_string()],
            }))
            .await
            .expect("update")
            .into_inner();

        assert_eq!(updated.keep_releases, 10);
        assert_eq!(updated.release_root, "/opt/bot", "outside the mask");
    }

    #[tokio::test]
    async fn an_unspecified_unit_action_is_refused() {
        let (service, target_id) = fixture().await;
        let created = service
            .create(Request::new(create(&target_id, "bot")))
            .await
            .expect("create")
            .into_inner();

        let status = service
            .unit_action(Request::new(UnitActionRequest {
                mutation: Some(Mutation::by(Actor::human("u", "alice"))),
                service_id: created.id,
                action: unit_action_request::Action::Unspecified as i32,
            }))
            .await
            .expect_err("must be refused");
        assert_eq!(status.code(), tonic::Code::InvalidArgument);
    }

    #[tokio::test]
    async fn an_unknown_unit_action_value_is_refused() {
        let (service, target_id) = fixture().await;
        let created = service
            .create(Request::new(create(&target_id, "bot")))
            .await
            .expect("create")
            .into_inner();

        let status = service
            .unit_action(Request::new(UnitActionRequest {
                mutation: Some(Mutation::by(Actor::human("u", "alice"))),
                service_id: created.id,
                action: 999,
            }))
            .await
            .expect_err("must be refused");
        assert_eq!(status.code(), tonic::Code::InvalidArgument);
    }

    #[tokio::test]
    async fn a_unit_action_against_a_latency_critical_target_needs_the_opt_in() {
        // Checked before any SSH connection is attempted.
        let (service, _) = fixture().await;
        let hot = test_support::create_latency_critical_target(&service.context).await;
        let created = test_support::create_service(&service.context, &hot.id, "hft").await;

        let status = service
            .unit_action(Request::new(UnitActionRequest {
                mutation: Some(Mutation::by(Actor::agent("s", "claude"))),
                service_id: created.id,
                action: unit_action_request::Action::Restart as i32,
            }))
            .await
            .expect_err("must be refused");
        assert_eq!(status.code(), tonic::Code::FailedPrecondition);
        assert!(status.message().contains("latency-critical"));
    }

    #[tokio::test]
    async fn the_status_of_an_unreachable_service_is_unknown_rather_than_an_error() {
        // The dashboard must still be able to render the row.
        let (service, target_id) = fixture().await;
        let created = service
            .create(Request::new(create(&target_id, "bot")))
            .await
            .expect("create")
            .into_inner();

        let status = service
            .get_unit_status(Request::new(GetUnitStatusRequest {
                service_id: created.id.clone(),
            }))
            .await
            .expect("must not error")
            .into_inner();

        assert_eq!(status.service_id, created.id);
        assert_eq!(status.active_state, "unknown");
    }

    #[tokio::test]
    async fn deleting_a_service_with_a_dangerous_release_root_is_refused() {
        // A row whose release_root is "/" must never become `rm -rf /`.
        let (service, target_id) = fixture().await;
        let created = service
            .context
            .store
            .create_service(&Service {
                target_id,
                name: "reckless".to_string(),
                release_root: "/".to_string(),
                ..Default::default()
            })
            .await
            .expect("service");

        let status = service
            .delete(Request::new(DeleteServiceRequest {
                mutation: Some(Mutation::by(Actor::human("u", "alice"))),
                id: created.id,
                stop_and_disable_unit: false,
                remove_release_dir: true,
            }))
            .await
            .expect_err("must be refused");
        assert_eq!(status.code(), tonic::Code::FailedPrecondition);
        assert!(status.message().contains("refusing to remove"));
    }

    #[tokio::test]
    async fn deleting_a_service_without_touching_the_host_removes_only_the_row() {
        let (service, target_id) = fixture().await;
        let created = service
            .create(Request::new(create(&target_id, "bot")))
            .await
            .expect("create")
            .into_inner();

        service
            .delete(Request::new(DeleteServiceRequest {
                mutation: Some(Mutation::by(Actor::human("u", "alice"))),
                id: created.id.clone(),
                stop_and_disable_unit: false,
                remove_release_dir: false,
            }))
            .await
            .expect("delete");

        let status = service
            .get(Request::new(GetServiceRequest { id: created.id }))
            .await
            .expect_err("err");
        assert_eq!(status.code(), tonic::Code::NotFound);
    }

    #[tokio::test]
    async fn a_dry_run_delete_records_what_it_would_do_without_doing_it() {
        let (service, target_id) = fixture().await;
        let created = service
            .create(Request::new(create(&target_id, "bot")))
            .await
            .expect("create")
            .into_inner();

        service
            .delete(Request::new(DeleteServiceRequest {
                mutation: Some(Mutation {
                    actor: Some(Actor::agent("s", "claude")),
                    dry_run: true,
                    ..Default::default()
                }),
                id: created.id.clone(),
                stop_and_disable_unit: true,
                remove_release_dir: true,
            }))
            .await
            .expect("dry run");

        // Still there.
        assert!(
            service
                .get(Request::new(GetServiceRequest {
                    id: created.id.clone()
                }))
                .await
                .is_ok()
        );

        let audit = service
            .context
            .store
            .list_audit(&created.id, actor::Kind::Unspecified, 50, 0)
            .await
            .expect("audit");
        let entry = audit
            .iter()
            .find(|e| e.action == "ServicesApi.Delete")
            .expect("entry");
        assert!(entry.dry_run);
        // The summary spells out the consequences.
        assert!(entry.summary.contains("stopped and disabled"));
        assert!(entry.summary.contains("/opt/bot"));
    }
}
