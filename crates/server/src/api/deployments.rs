//! The `Deployments` service: deploy, rollback, cancel, history and live watch.

use std::pin::Pin;

use futures_util::Stream;
use nudo_proto::deployments_server::Deployments;
use nudo_proto::*;
use tonic::{Request, Response, Status};

use super::{Context, internal};
use crate::deploy::DeployOptions;
use crate::events::DeploymentEvent;
use crate::store::{DeployTrigger, NewDeployment, page_offset, page_size};
use crate::systemd;

pub struct DeploymentsService {
    context: Context,
}

impl DeploymentsService {
    pub fn new(context: Context) -> Self {
        Self { context }
    }
}

#[tonic::async_trait]
impl Deployments for DeploymentsService {
    async fn deploy(
        &self,
        request: Request<DeployRequest>,
    ) -> Result<Response<Deployment>, Status> {
        let request = request.into_inner();
        let (service, target) = self
            .context
            .require_service_and_target(&request.service_id)
            .await?;

        let mut summary = format!("deployed {} to {}", service.name, target.name);
        if !request.git_ref.trim().is_empty() {
            summary.push_str(&format!(" at {}", request.git_ref.trim()));
        }
        if request.skip_health_check {
            summary.push_str(" (health check skipped)");
        }

        let authorized = self
            .context
            .authorize(
                request.mutation.as_ref(),
                "Deployments.Deploy",
                &request.service_id,
                Some(&target),
                summary,
            )
            .await?;

        // A retry after a dropped connection must not deploy twice.
        if !authorized.idempotency_key.is_empty() {
            if let Some(existing) = self
                .context
                .store
                .check_idempotency(&authorized.idempotency_key, "Deploy")
                .await
                .map_err(internal)?
            {
                let deployment = self
                    .context
                    .store
                    .get_deployment(&existing)
                    .await
                    .map_err(internal)?
                    .ok_or_else(|| Status::internal("the recorded deployment is gone"))?;
                return Ok(Response::new(deployment));
            }
        }

        if authorized.dry_run {
            // The plan, not a queued deployment: what would be shipped, and
            // where it would go back to if it failed.
            return Ok(Response::new(Deployment {
                service_id: request.service_id,
                status: deployment::Status::Queued as i32,
                actor: Some(authorized.actor),
                previous_release_id: service.current_release_id,
                error: String::new(),
                ..Default::default()
            }));
        }

        // The proto documents auto-rollback as defaulting to true server-side.
        // proto3 cannot distinguish "false" from "unset", so a caller that wants
        // it off must set skip_health_check or explicitly not want a rollback —
        // documented in CHANGES.md.
        let auto_rollback = request.auto_rollback_on_failure || !request.skip_health_check;

        let deployment = self
            .context
            .engine
            .start_deploy(
                &request.service_id,
                authorized.actor,
                DeployOptions {
                    git_ref: request.git_ref,
                    artifact_url: request.artifact_url,
                    uploaded_artifact: None,
                    skip_health_check: request.skip_health_check,
                    auto_rollback_on_failure: auto_rollback,
                },
            )
            .await
            .map_err(super::invalid)?;

        if !authorized.idempotency_key.is_empty() {
            let _ = self
                .context
                .store
                .record_idempotency(&authorized.idempotency_key, "Deploy", &deployment.id)
                .await;
        }

        Ok(Response::new(deployment))
    }

    async fn rollback(
        &self,
        request: Request<RollbackRequest>,
    ) -> Result<Response<Deployment>, Status> {
        let request = request.into_inner();
        let (service, target) = self
            .context
            .require_service_and_target(&request.service_id)
            .await?;

        // Resolve which release before authorizing, so the audit entry names it.
        let releases = self
            .context
            .store
            .list_releases(&request.service_id)
            .await
            .map_err(internal)?;
        let ids: Vec<&str> = releases.iter().map(|r| r.id.as_str()).collect();

        let chosen = systemd::rollback_target(&ids, &service.current_release_id, &request.release_id)
            .map_err(|error| Status::failed_precondition(error.to_string()))?
            .to_string();

        let authorized = self
            .context
            .authorize(
                request.mutation.as_ref(),
                "Deployments.Rollback",
                &request.service_id,
                Some(&target),
                format!("rolled {} back to release {chosen}", service.name),
            )
            .await?;

        if authorized.dry_run {
            return Ok(Response::new(Deployment {
                service_id: request.service_id,
                release_id: chosen,
                status: deployment::Status::Queued as i32,
                actor: Some(authorized.actor),
                previous_release_id: service.current_release_id,
                ..Default::default()
            }));
        }

        // A rollback is recorded as a deployment so it appears in the history
        // alongside the deploy that made it necessary.
        let mut deployment = self
            .context
            .store
            .create_deployment(&NewDeployment {
                service_id: request.service_id.clone(),
                actor: authorized.actor,
                previous_release_id: service.current_release_id.clone(),
                git_ref: String::new(),
                trigger: DeployTrigger::Rollback,
            })
            .await
            .map_err(internal)?;

        self.context
            .store
            .set_deployment_release(&deployment.id, &chosen, "")
            .await
            .map_err(internal)?;
        self.context
            .store
            .set_deployment_status(&deployment.id, deployment::Status::Activating)
            .await
            .map_err(internal)?;

        // Reflect what was just written, so the caller learns which release the
        // rollback targets without a follow-up Get.
        deployment.release_id = chosen.clone();
        deployment.status = deployment::Status::Activating as i32;

        // Runs in the background so the caller gets an id to watch immediately.
        let engine = self.context.engine.clone();
        let service_id = request.service_id.clone();
        let deployment_id = deployment.id.clone();
        let release_id = chosen.clone();
        tokio::spawn(async move {
            match engine.activate_release(&service_id, &release_id).await {
                Ok(messages) => {
                    for message in messages {
                        let _ = engine
                            .store
                            .append_deployment_log(&deployment_id, &message, false)
                            .await;
                        engine.bus.publish_deployment(
                            &deployment_id,
                            DeploymentEvent::Output {
                                line: message,
                                stderr: false,
                            },
                        );
                    }
                    let _ = engine
                        .store
                        .set_deployment_status(&deployment_id, deployment::Status::Succeeded)
                        .await;
                    engine.bus.publish_deployment(
                        &deployment_id,
                        DeploymentEvent::Finished(deployment::Status::Succeeded),
                    );
                }
                Err(error) => {
                    let message = format!("{error:#}");
                    let _ = engine
                        .store
                        .append_deployment_log(&deployment_id, &message, true)
                        .await;
                    let _ = engine.store.set_deployment_error(&deployment_id, &message).await;
                    let _ = engine
                        .store
                        .set_deployment_status(&deployment_id, deployment::Status::Failed)
                        .await;
                    engine.bus.publish_deployment(
                        &deployment_id,
                        DeploymentEvent::Finished(deployment::Status::Failed),
                    );
                }
            }
        });

        Ok(Response::new(deployment))
    }

    async fn cancel(
        &self,
        request: Request<CancelDeploymentRequest>,
    ) -> Result<Response<Deployment>, Status> {
        let request = request.into_inner();
        let deployment = self
            .context
            .store
            .get_deployment(&request.deployment_id)
            .await
            .map_err(internal)?
            .ok_or_else(|| {
                Status::not_found(format!("no such deployment: {}", request.deployment_id))
            })?;

        let (service, target) = self
            .context
            .require_service_and_target(&deployment.service_id)
            .await?;

        let authorized = self
            .context
            .authorize(
                request.mutation.as_ref(),
                "Deployments.Cancel",
                &request.deployment_id,
                Some(&target),
                format!("cancelled a deployment of {}", service.name),
            )
            .await?;

        if authorized.dry_run {
            return Ok(Response::new(deployment));
        }

        // Sets the flag; the engine unwinds at its next checkpoint rather than
        // being killed mid-write.
        self.context
            .store
            .request_cancel(&request.deployment_id)
            .await
            .map_err(|error| Status::failed_precondition(format!("{error:#}")))?;

        let updated = self
            .context
            .store
            .get_deployment(&request.deployment_id)
            .await
            .map_err(internal)?
            .ok_or_else(|| Status::internal("the deployment disappeared"))?;
        Ok(Response::new(updated))
    }

    async fn get(
        &self,
        request: Request<GetDeploymentRequest>,
    ) -> Result<Response<Deployment>, Status> {
        let id = request.into_inner().id;
        let deployment = self
            .context
            .store
            .get_deployment(&id)
            .await
            .map_err(internal)?
            .ok_or_else(|| Status::not_found(format!("no such deployment: {id}")))?;
        Ok(Response::new(deployment))
    }

    async fn list(
        &self,
        request: Request<ListDeploymentsRequest>,
    ) -> Result<Response<ListDeploymentsResponse>, Status> {
        let request = request.into_inner();
        let limit = page_size(request.page_size);
        let offset = page_offset(&request.page_token);

        let deployments = self
            .context
            .store
            .list_deployments(&request.service_id, limit, offset)
            .await
            .map_err(internal)?;

        let next_page_token = crate::store::next_page_token(offset, deployments.len(), limit);
        Ok(Response::new(ListDeploymentsResponse {
            deployments,
            next_page_token,
        }))
    }

    async fn list_releases(
        &self,
        request: Request<ListReleasesRequest>,
    ) -> Result<Response<ListReleasesResponse>, Status> {
        let request = request.into_inner();
        // Only retained releases: offering a pruned one would let a rollback
        // point at a directory that is no longer there.
        let releases = self
            .context
            .store
            .list_releases(&request.service_id)
            .await
            .map_err(internal)?;
        Ok(Response::new(ListReleasesResponse { releases }))
    }

    type WatchStream =
        Pin<Box<dyn Stream<Item = Result<DeploymentEvent2, Status>> + Send + 'static>>;

    async fn watch(
        &self,
        request: Request<WatchDeploymentRequest>,
    ) -> Result<Response<Self::WatchStream>, Status> {
        let deployment_id = request.into_inner().deployment_id;
        let context = self.context.clone();

        let deployment = context
            .store
            .get_deployment(&deployment_id)
            .await
            .map_err(internal)?
            .ok_or_else(|| Status::not_found(format!("no such deployment: {deployment_id}")))?;

        // Subscribe before replaying history, so an event that lands between the
        // two is not lost.
        let mut receiver = context.bus.watch_deployment(&deployment_id);

        let stream = async_stream::try_stream! {
            let now = || Some(nudo_proto::to_timestamp(chrono::Utc::now()));

            // Backfill what already happened, so a view opened mid-deploy — or
            // after it finished — is not empty.
            let history = context
                .store
                .deployment_logs(&deployment_id)
                .await
                .map_err(internal)?;
            for line in history {
                yield DeploymentEvent2 {
                    at: Some(nudo_proto::to_timestamp(line.at)),
                    event: Some(deployment_event::Event::OutputLine(line.line)),
                };
            }

            // A deployment that is already over gets its verdict and ends,
            // rather than the client waiting for events that will never come.
            let status = deployment::Status::try_from(deployment.status)
                .unwrap_or(deployment::Status::Unspecified);
            if status.is_terminal() {
                let latest = context
                    .store
                    .get_deployment(&deployment_id)
                    .await
                    .map_err(internal)?;
                if let Some(latest) = latest {
                    yield DeploymentEvent2 {
                        at: now(),
                        event: Some(deployment_event::Event::TerminalState(latest)),
                    };
                }
                return;
            }

            loop {
                match receiver.recv().await {
                    Ok(DeploymentEvent::Status(status)) => {
                        yield DeploymentEvent2 {
                            at: now(),
                            event: Some(deployment_event::Event::StatusChange(status as i32)),
                        };
                    }
                    Ok(DeploymentEvent::Output { line, .. }) => {
                        yield DeploymentEvent2 {
                            at: now(),
                            event: Some(deployment_event::Event::OutputLine(line)),
                        };
                    }
                    Ok(DeploymentEvent::Finished(_)) => {
                        // The final message carries the whole row, so a client
                        // does not need a follow-up Get to learn the outcome.
                        if let Some(final_state) = context
                            .store
                            .get_deployment(&deployment_id)
                            .await
                            .map_err(internal)?
                        {
                            yield DeploymentEvent2 {
                                at: now(),
                                event: Some(deployment_event::Event::TerminalState(final_state)),
                            };
                        }
                        return;
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(skipped)) => {
                        // Tell the client rather than silently dropping output.
                        yield DeploymentEvent2 {
                            at: now(),
                            event: Some(deployment_event::Event::OutputLine(format!(
                                "[{skipped} line(s) skipped: this viewer fell behind]"
                            ))),
                        };
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                        // The engine finished and released the channel; report
                        // the row's final state.
                        if let Some(final_state) = context
                            .store
                            .get_deployment(&deployment_id)
                            .await
                            .map_err(internal)?
                        {
                            yield DeploymentEvent2 {
                                at: now(),
                                event: Some(deployment_event::Event::TerminalState(final_state)),
                            };
                        }
                        return;
                    }
                }
            }
        };

        Ok(Response::new(Box::pin(stream)))
    }
}

/// The generated proto message shares a name with our internal enum, so it is
/// aliased here rather than renaming either.
use nudo_proto::DeploymentEvent as DeploymentEvent2;

/// Extracts the error from a streaming call's result.
///
/// The success type is a boxed `Stream`, which has no `Debug`, so `expect_err`
/// cannot be used directly.
#[cfg(test)]
fn expect_status<T>(result: Result<tonic::Response<T>, Status>, what: &str) -> Status {
    match result {
        Ok(_) => panic!("expected an error: {what}"),
        Err(status) => status,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::Bus;
    use crate::store::{Store, TargetInput};
    use std::sync::Arc;
    use tokio_stream::StreamExt;

    async fn fixture() -> (DeploymentsService, String) {
        let context = Context::new(
            Store::open_in_memory().await.expect("store"),
            Bus::default(),
            crate::crypto::SecretKey::generate(),
            Arc::new(crate::Config::default()),
        );
        let target = context
            .store
            .create_target(&TargetInput {
                name: "box".to_string(),
                host: "10.0.0.1".to_string(),
                ..Default::default()
            })
            .await
            .expect("target");
        let service = context
            .store
            .create_service(&Service {
                target_id: target.id,
                name: "bot".to_string(),
                ..Default::default()
            })
            .await
            .expect("service");
        (DeploymentsService::new(context), service.id)
    }

    fn deploy(service_id: &str) -> DeployRequest {
        DeployRequest {
            mutation: Some(Mutation::by(Actor::human("usr_1", "alice"))),
            service_id: service_id.to_string(),
            ..Default::default()
        }
    }

    async fn add_release(service: &DeploymentsService, service_id: &str, n: u32) -> String {
        let release = service
            .context
            .store
            .create_release(&Release {
                service_id: service_id.to_string(),
                path: format!("/opt/bot/releases/r{n}"),
                ..Default::default()
            })
            .await
            .expect("release");
        // Distinct stored timestamps so ordering is deterministic.
        tokio::time::sleep(std::time::Duration::from_millis(2)).await;
        release.id
    }

    #[tokio::test]
    async fn a_deploy_is_queued_and_watchable() {
        let (service, service_id) = fixture().await;
        let deployment = service
            .deploy(Request::new(deploy(&service_id)))
            .await
            .expect("deploy")
            .into_inner();

        assert!(deployment.id.starts_with("dep_"));
        assert_eq!(deployment.service_id, service_id);
    }

    #[tokio::test]
    async fn deploying_a_missing_service_is_not_found() {
        let (service, _) = fixture().await;
        let status = service
            .deploy(Request::new(deploy("svc_nope")))
            .await
            .expect_err("err");
        assert_eq!(status.code(), tonic::Code::NotFound);
    }

    #[tokio::test]
    async fn a_dry_run_deploy_returns_a_plan_without_queueing_anything() {
        let (service, service_id) = fixture().await;
        let planned = service
            .deploy(Request::new(DeployRequest {
                mutation: Some(Mutation {
                    actor: Some(Actor::agent("s", "claude")),
                    dry_run: true,
                    ..Default::default()
                }),
                ..deploy(&service_id)
            }))
            .await
            .expect("dry run")
            .into_inner();

        assert!(planned.id.is_empty(), "nothing was queued");
        assert!(
            service
                .context
                .store
                .list_deployments("", 50, 0)
                .await
                .expect("list")
                .is_empty()
        );
    }

    #[tokio::test]
    async fn a_repeated_idempotency_key_returns_the_original_deployment() {
        // A CI job retrying after a dropped connection must not deploy twice.
        let (service, service_id) = fixture().await;
        let request = || DeployRequest {
            mutation: Some(Mutation {
                actor: Some(Actor::human("u", "alice")),
                idempotency_key: "ci-run-42".to_string(),
                ..Default::default()
            }),
            ..deploy(&service_id)
        };

        let first = service.deploy(Request::new(request())).await.expect("first").into_inner();
        let second = service.deploy(Request::new(request())).await.expect("second").into_inner();

        assert_eq!(first.id, second.id);
        assert_eq!(
            service.context.store.list_deployments("", 50, 0).await.expect("list").len(),
            1
        );
    }

    #[tokio::test]
    async fn a_deploy_to_a_latency_critical_target_is_refused_without_the_opt_in() {
        let (service, _) = fixture().await;
        let hot = service
            .context
            .store
            .create_target(&TargetInput {
                name: "hot-box".to_string(),
                host: "10.0.0.2".to_string(),
                latency_critical: true,
                ..Default::default()
            })
            .await
            .expect("target");
        let hot_service = service
            .context
            .store
            .create_service(&Service {
                target_id: hot.id,
                name: "hft".to_string(),
                ..Default::default()
            })
            .await
            .expect("service");

        let status = service
            .deploy(Request::new(deploy(&hot_service.id)))
            .await
            .expect_err("must be refused");
        assert_eq!(status.code(), tonic::Code::FailedPrecondition);
        // Nothing was queued.
        assert!(
            service
                .context
                .store
                .list_deployments("", 50, 0)
                .await
                .expect("list")
                .is_empty()
        );
    }

    #[tokio::test]
    async fn a_rollback_with_no_previous_release_is_refused() {
        let (service, service_id) = fixture().await;
        let status = service
            .rollback(Request::new(RollbackRequest {
                mutation: Some(Mutation::by(Actor::human("u", "alice"))),
                service_id,
                release_id: String::new(),
            }))
            .await
            .expect_err("must be refused");
        assert_eq!(status.code(), tonic::Code::FailedPrecondition);
    }

    #[tokio::test]
    async fn a_rollback_resolves_the_previous_release_and_names_it_in_the_audit_log() {
        let (service, service_id) = fixture().await;
        let first = add_release(&service, &service_id, 1).await;
        let second = add_release(&service, &service_id, 2).await;
        service
            .context
            .store
            .set_current_release(&service_id, &second)
            .await
            .expect("set current");

        let deployment = service
            .rollback(Request::new(RollbackRequest {
                mutation: Some(Mutation::by(Actor::human("u", "alice"))),
                service_id: service_id.clone(),
                release_id: String::new(),
            }))
            .await
            .expect("rollback")
            .into_inner();

        assert_eq!(deployment.release_id, first);
        assert_eq!(deployment.previous_release_id, second);

        let audit = service
            .context
            .store
            .list_audit(&service_id, actor::Kind::Unspecified, 50, 0)
            .await
            .expect("audit");
        let entry = audit
            .iter()
            .find(|e| e.action == "Deployments.Rollback")
            .expect("entry");
        assert!(entry.summary.contains(&first), "the audit entry must name the release");
    }

    #[tokio::test]
    async fn rolling_back_to_a_pruned_release_is_refused() {
        let (service, service_id) = fixture().await;
        let first = add_release(&service, &service_id, 1).await;
        let second = add_release(&service, &service_id, 2).await;
        service
            .context
            .store
            .set_current_release(&service_id, &second)
            .await
            .expect("set current");
        service.context.store.mark_release_pruned(&first).await.expect("prune");

        let status = service
            .rollback(Request::new(RollbackRequest {
                mutation: Some(Mutation::by(Actor::human("u", "alice"))),
                service_id,
                release_id: first,
            }))
            .await
            .expect_err("must be refused");
        assert_eq!(status.code(), tonic::Code::FailedPrecondition);
    }

    #[tokio::test]
    async fn a_dry_run_rollback_names_the_release_without_acting() {
        let (service, service_id) = fixture().await;
        let first = add_release(&service, &service_id, 1).await;
        let second = add_release(&service, &service_id, 2).await;
        service
            .context
            .store
            .set_current_release(&service_id, &second)
            .await
            .expect("set current");

        let planned = service
            .rollback(Request::new(RollbackRequest {
                mutation: Some(Mutation {
                    actor: Some(Actor::agent("s", "claude")),
                    dry_run: true,
                    ..Default::default()
                }),
                service_id: service_id.clone(),
                release_id: String::new(),
            }))
            .await
            .expect("dry run")
            .into_inner();

        assert_eq!(planned.release_id, first);
        assert!(planned.id.is_empty());
        // Nothing was recorded and the live release is unchanged.
        assert!(
            service
                .context
                .store
                .list_deployments("", 50, 0)
                .await
                .expect("list")
                .is_empty()
        );
        let reloaded = service
            .context
            .store
            .get_service(&service_id)
            .await
            .expect("get")
            .expect("some");
        assert_eq!(reloaded.current_release_id, second);
    }

    #[tokio::test]
    async fn cancelling_a_finished_deployment_is_refused() {
        let (service, service_id) = fixture().await;
        let deployment = service
            .context
            .store
            .create_deployment(&NewDeployment {
                service_id,
                actor: Actor::human("u", "alice"),
                previous_release_id: String::new(),
                git_ref: String::new(),
                trigger: DeployTrigger::Manual,
            })
            .await
            .expect("create");
        service
            .context
            .store
            .set_deployment_status(&deployment.id, deployment::Status::Succeeded)
            .await
            .expect("finish");

        let status = service
            .cancel(Request::new(CancelDeploymentRequest {
                mutation: Some(Mutation::by(Actor::human("u", "alice"))),
                deployment_id: deployment.id,
            }))
            .await
            .expect_err("must be refused");
        assert_eq!(status.code(), tonic::Code::FailedPrecondition);
    }

    #[tokio::test]
    async fn cancelling_an_unknown_deployment_is_not_found() {
        let (service, _) = fixture().await;
        let status = service
            .cancel(Request::new(CancelDeploymentRequest {
                mutation: Some(Mutation::by(Actor::human("u", "alice"))),
                deployment_id: "dep_nope".to_string(),
            }))
            .await
            .expect_err("err");
        assert_eq!(status.code(), tonic::Code::NotFound);
    }

    #[tokio::test]
    async fn only_retained_releases_are_listed() {
        let (service, service_id) = fixture().await;
        let first = add_release(&service, &service_id, 1).await;
        add_release(&service, &service_id, 2).await;
        service.context.store.mark_release_pruned(&first).await.expect("prune");

        let listed = service
            .list_releases(Request::new(ListReleasesRequest {
                service_id: service_id.clone(),
            }))
            .await
            .expect("list")
            .into_inner();
        assert_eq!(listed.releases.len(), 1);
        assert!(!listed.releases.iter().any(|r| r.id == first));
    }

    #[tokio::test]
    async fn watching_a_finished_deployment_replays_its_output_then_ends() {
        // Opening the view after the fact must show what happened rather than
        // hanging on a stream that will never produce anything.
        let (service, service_id) = fixture().await;
        let deployment = service
            .context
            .store
            .create_deployment(&NewDeployment {
                service_id,
                actor: Actor::human("u", "alice"),
                previous_release_id: String::new(),
                git_ref: String::new(),
                trigger: DeployTrigger::Manual,
            })
            .await
            .expect("create");

        service
            .context
            .store
            .append_deployment_log(&deployment.id, "compiling", false)
            .await
            .expect("log");
        service
            .context
            .store
            .append_deployment_log(&deployment.id, "done", false)
            .await
            .expect("log");
        service
            .context
            .store
            .set_deployment_status(&deployment.id, deployment::Status::Succeeded)
            .await
            .expect("finish");

        let mut stream = service
            .watch(Request::new(WatchDeploymentRequest {
                deployment_id: deployment.id.clone(),
            }))
            .await
            .expect("watch")
            .into_inner();

        let mut lines = Vec::new();
        let mut terminal = None;
        while let Some(event) = stream.next().await {
            match event.expect("event").event {
                Some(deployment_event::Event::OutputLine(line)) => lines.push(line),
                Some(deployment_event::Event::TerminalState(state)) => terminal = Some(state),
                _ => {}
            }
        }

        assert_eq!(lines, vec!["compiling", "done"]);
        let terminal = terminal.expect("a terminal state must be sent");
        assert_eq!(terminal.status, deployment::Status::Succeeded as i32);
    }

    #[tokio::test]
    async fn watching_an_unknown_deployment_is_not_found() {
        let (service, _) = fixture().await;
        let status = expect_status(
            service
                .watch(Request::new(WatchDeploymentRequest {
                deployment_id: "dep_nope".to_string(),
                }))
                .await,
            "err",
        );
        assert_eq!(status.code(), tonic::Code::NotFound);
    }

    #[tokio::test]
    async fn a_live_watch_receives_events_and_ends_on_the_terminal_one() {
        let (service, service_id) = fixture().await;
        let deployment = service
            .context
            .store
            .create_deployment(&NewDeployment {
                service_id,
                actor: Actor::human("u", "alice"),
                previous_release_id: String::new(),
                git_ref: String::new(),
                trigger: DeployTrigger::Manual,
            })
            .await
            .expect("create");

        let mut stream = service
            .watch(Request::new(WatchDeploymentRequest {
                deployment_id: deployment.id.clone(),
            }))
            .await
            .expect("watch")
            .into_inner();

        let bus = service.context.bus.clone();
        let store = service.context.store.clone();
        let deployment_id = deployment.id.clone();
        tokio::spawn(async move {
            bus.publish_deployment(
                &deployment_id,
                crate::events::DeploymentEvent::Status(deployment::Status::Building),
            );
            bus.publish_deployment(
                &deployment_id,
                crate::events::DeploymentEvent::Output {
                    line: "building".to_string(),
                    stderr: false,
                },
            );
            let _ = store
                .set_deployment_status(&deployment_id, deployment::Status::Succeeded)
                .await;
            bus.publish_deployment(
                &deployment_id,
                crate::events::DeploymentEvent::Finished(deployment::Status::Succeeded),
            );
        });

        let mut saw_status = false;
        let mut saw_output = false;
        let mut saw_terminal = false;
        while let Some(event) = stream.next().await {
            match event.expect("event").event {
                Some(deployment_event::Event::StatusChange(_)) => saw_status = true,
                Some(deployment_event::Event::OutputLine(_)) => saw_output = true,
                Some(deployment_event::Event::TerminalState(_)) => {
                    saw_terminal = true;
                    break;
                }
                None => {}
            }
        }

        assert!(saw_status);
        assert!(saw_output);
        assert!(saw_terminal, "the stream must end with a verdict");
    }
}
