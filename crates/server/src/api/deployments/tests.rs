use super::*;
use crate::api::test_support;
use tokio_stream::StreamExt;

async fn fixture() -> (DeploymentsService, String) {
    let (context, _, service) = test_support::context_with_service().await;
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

    let first = service
        .deploy(Request::new(request()))
        .await
        .expect("first")
        .into_inner();
    let second = service
        .deploy(Request::new(request()))
        .await
        .expect("second")
        .into_inner();

    assert_eq!(first.id, second.id);
    assert_eq!(
        service
            .context
            .store
            .list_deployments("", 50, 0)
            .await
            .expect("list")
            .len(),
        1
    );
}

#[tokio::test]
async fn a_deploy_to_a_latency_critical_target_is_refused_without_the_opt_in() {
    let (service, _) = fixture().await;
    let hot = test_support::create_latency_critical_target(&service.context).await;
    let hot_service = test_support::create_service(&service.context, &hot.id, "hft").await;

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
    assert!(
        entry.summary.contains(&first),
        "the audit entry must name the release"
    );
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
    service
        .context
        .store
        .mark_release_pruned(&first)
        .await
        .expect("prune");

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
    service
        .context
        .store
        .mark_release_pruned(&first)
        .await
        .expect("prune");

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
