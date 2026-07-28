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
        if let Some(existing) = authorized.replayed(&self.context.store, "Deploy").await? {
            let deployment = self
                .context
                .store
                .get_deployment(&existing)
                .await
                .map_err(internal)?
                .ok_or_else(|| Status::internal("the recorded deployment is gone"))?;
            return Ok(Response::new(deployment));
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
                authorized.actor.clone(),
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

        authorized
            .record(&self.context.store, "Deploy", &deployment.id)
            .await;

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

        let chosen =
            systemd::rollback_target(&ids, &service.current_release_id, &request.release_id)
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
                    let _ = engine
                        .store
                        .set_deployment_error(&deployment_id, &message)
                        .await;
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
mod tests;
