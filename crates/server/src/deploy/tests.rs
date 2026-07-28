use super::*;
use crate::events::DeploymentEvent;
use crate::store::TargetInput;
use nudo_proto::{Release, Service, deployment};

async fn engine() -> Engine {
    Engine {
        store: Store::open_in_memory().await.expect("store"),
        bus: Bus::default(),
        secret_key: SecretKey::generate(),
        config: Arc::new(crate::Config::default()),
    }
}

async fn service_on_target(engine: &Engine, latency_critical: bool) -> (String, String) {
    let target = engine
        .store
        .create_target(&TargetInput {
            name: "box".to_string(),
            host: "10.0.0.1".to_string(),
            latency_critical,
            ..Default::default()
        })
        .await
        .expect("target");
    let service = engine
        .store
        .create_service(&Service {
            target_id: target.id.clone(),
            name: "bot".to_string(),
            ..Default::default()
        })
        .await
        .expect("service");
    (target.id, service.id)
}

#[tokio::test]
async fn a_queued_deployment_records_the_release_it_would_roll_back_to() {
    let engine = engine().await;
    let (_, service_id) = service_on_target(&engine, false).await;

    // Pretend a release is already live.
    let existing = engine
        .store
        .create_release(&Release {
            service_id: service_id.clone(),
            path: "/opt/bot/releases/r1".to_string(),
            ..Default::default()
        })
        .await
        .expect("release");
    engine
        .store
        .set_current_release(&service_id, &existing.id)
        .await
        .expect("set current");

    let deployment = engine
        .start_deploy(
            &service_id,
            nudo_proto::Actor::human("usr_1", "alice"),
            DeployOptions::default(),
        )
        .await
        .expect("start");

    // Captured at queue time, so a failure knows where to go back to even
    // if the service row changes meanwhile.
    assert_eq!(deployment.previous_release_id, existing.id);
}

#[tokio::test]
async fn the_first_deployment_of_a_service_has_nothing_to_roll_back_to() {
    let engine = engine().await;
    let (_, service_id) = service_on_target(&engine, false).await;

    let deployment = engine
        .start_deploy(
            &service_id,
            nudo_proto::Actor::human("usr_1", "alice"),
            DeployOptions::default(),
        )
        .await
        .expect("start");
    assert!(deployment.previous_release_id.is_empty());
}

#[tokio::test]
async fn deploying_an_unknown_service_fails_before_a_row_is_created() {
    let engine = engine().await;
    assert!(
        engine
            .start_deploy(
                "svc_missing",
                nudo_proto::Actor::human("u", "u"),
                DeployOptions::default()
            )
            .await
            .is_err()
    );
    assert!(
        engine
            .store
            .list_deployments("", 50, 0)
            .await
            .expect("list")
            .is_empty()
    );
}

#[tokio::test]
async fn a_service_with_no_artifact_source_fails_with_actionable_advice() {
    let engine = engine().await;
    let (_, service_id) = service_on_target(&engine, false).await;
    let service = engine
        .store
        .get_service(&service_id)
        .await
        .expect("get")
        .expect("some");

    let error = engine
        .obtain_artifact("dep_x", &service, &DeployOptions::default())
        .await
        .expect_err("must fail");
    let message = error.to_string();
    // The message has to tell an operator what to actually do.
    assert!(message.contains("no artifact"), "got: {message}");
    assert!(message.contains("nudo deploy --artifact"), "got: {message}");
}

#[tokio::test]
async fn an_uploaded_artifact_is_read_from_disk() {
    let engine = engine().await;
    let (_, service_id) = service_on_target(&engine, false).await;
    let service = engine
        .store
        .get_service(&service_id)
        .await
        .expect("get")
        .expect("some");

    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("bot");
    tokio::fs::write(&path, b"ELF binary bytes")
        .await
        .expect("write");

    let artifact = engine
        .obtain_artifact(
            "dep_x",
            &service,
            &DeployOptions {
                uploaded_artifact: Some(path),
                ..Default::default()
            },
        )
        .await
        .expect("obtain");
    assert_eq!(artifact.bytes, b"ELF binary bytes");
}

#[tokio::test]
async fn an_artifact_url_with_a_non_http_scheme_is_refused() {
    // `artifact_url` is client-supplied, so a `file://` URL would have the
    // control plane read its own filesystem and ship the result to a target.
    let engine = engine().await;
    for hostile in [
        "file:///etc/shadow",
        "FILE:///etc/passwd",
        "ftp://example.com/bot",
        "gopher://example.com/bot",
    ] {
        let error = engine
            .download_artifact("dep_x", hostile)
            .await
            .expect_err("must be refused");
        assert!(
            error.to_string().contains("http or https"),
            "{hostile}: {error}"
        );
    }
}

#[tokio::test]
async fn a_missing_uploaded_artifact_fails_rather_than_deploying_nothing() {
    let engine = engine().await;
    let (_, service_id) = service_on_target(&engine, false).await;
    let service = engine
        .store
        .get_service(&service_id)
        .await
        .expect("get")
        .expect("some");

    assert!(
        engine
            .obtain_artifact(
                "dep_x",
                &service,
                &DeployOptions {
                    uploaded_artifact: Some("/nonexistent/path/to/bot".into()),
                    ..Default::default()
                }
            )
            .await
            .is_err()
    );
}

#[tokio::test]
async fn an_ssh_target_cannot_be_assembled_without_a_key() {
    // A target with no key must produce an actionable error rather than an
    // authentication failure that looks like a network problem.
    let engine = engine().await;
    let (target_id, _) = service_on_target(&engine, false).await;
    let target = engine
        .store
        .get_target(&target_id)
        .await
        .expect("get")
        .expect("some");

    let error = engine.ssh_target_for(&target).await.expect_err("must fail");
    assert!(error.to_string().contains("no SSH key"), "got: {error}");
}

#[tokio::test]
async fn an_ssh_target_reads_its_key_from_the_secret_store() {
    let engine = engine().await;
    let (target_id, _) = service_on_target(&engine, false).await;

    let key_secret = engine
        .store
        .put_secret(
            &engine.secret_key,
            "SSH_KEY",
            "-----BEGIN OPENSSH PRIVATE KEY-----\nmaterial\n",
            "",
            "",
            false,
        )
        .await
        .expect("secret");
    let target = engine
        .store
        .update_target(
            &target_id,
            &nudo_proto::Target {
                ssh_key_id: key_secret.id,
                ..Default::default()
            },
            &["ssh_key_id".to_string()],
        )
        .await
        .expect("update");

    let ssh = engine.ssh_target_for(&target).await.expect("assemble");
    assert_eq!(ssh.host, "10.0.0.1");
    assert_eq!(ssh.port, 22);
    assert_eq!(ssh.user, "root");
    assert!(ssh.private_key.contains("OPENSSH PRIVATE KEY"));
}

#[tokio::test]
async fn a_target_whose_key_secret_was_deleted_reports_that_specifically() {
    let engine = engine().await;
    let (target_id, _) = service_on_target(&engine, false).await;
    let target = engine
        .store
        .update_target(
            &target_id,
            &nudo_proto::Target {
                ssh_key_id: "sec_deleted".to_string(),
                ..Default::default()
            },
            &["ssh_key_id".to_string()],
        )
        .await
        .expect("update");

    let error = engine.ssh_target_for(&target).await.expect_err("must fail");
    assert!(error.to_string().contains("does not exist"), "got: {error}");
}

#[tokio::test]
async fn activating_a_release_that_belongs_to_another_service_is_refused() {
    let engine = engine().await;
    let (target_id, service_id) = service_on_target(&engine, false).await;
    let other = engine
        .store
        .create_service(&Service {
            target_id,
            name: "other".to_string(),
            ..Default::default()
        })
        .await
        .expect("service");

    let release = engine
        .store
        .create_release(&Release {
            service_id: other.id,
            path: "/opt/other/releases/r1".to_string(),
            ..Default::default()
        })
        .await
        .expect("release");

    let error = engine
        .activate_release(&service_id, &release.id)
        .await
        .expect_err("must refuse");
    assert!(
        error.to_string().contains("does not belong"),
        "got: {error}"
    );
}

#[tokio::test]
async fn activating_an_unknown_release_is_refused() {
    let engine = engine().await;
    let (_, service_id) = service_on_target(&engine, false).await;
    assert!(
        engine
            .activate_release(&service_id, "rel_missing")
            .await
            .is_err()
    );
}

#[tokio::test]
async fn a_cancel_request_stops_the_deploy_at_the_next_checkpoint() {
    let engine = engine().await;
    let (_, service_id) = service_on_target(&engine, false).await;

    let deployment = engine
        .store
        .create_deployment(&NewDeployment {
            service_id: service_id.clone(),
            actor: nudo_proto::Actor::human("u", "u"),
            previous_release_id: String::new(),
            git_ref: String::new(),
            trigger: DeployTrigger::Manual,
        })
        .await
        .expect("create");

    engine
        .store
        .request_cancel(&deployment.id)
        .await
        .expect("cancel");
    assert!(engine.check_cancelled(&deployment.id).await.is_err());
}

#[tokio::test]
async fn output_is_persisted_and_broadcast_together() {
    let engine = engine().await;
    let (_, service_id) = service_on_target(&engine, false).await;
    let deployment = engine
        .store
        .create_deployment(&NewDeployment {
            service_id,
            actor: nudo_proto::Actor::human("u", "u"),
            previous_release_id: String::new(),
            git_ref: String::new(),
            trigger: DeployTrigger::Manual,
        })
        .await
        .expect("create");

    let mut watcher = engine.bus.watch_deployment(&deployment.id);
    engine.emit(&deployment.id, "compiling bot v2", false).await;

    // Reaches the live watcher...
    assert!(matches!(
        watcher.recv().await.expect("event"),
        DeploymentEvent::Output { line, .. } if line == "compiling bot v2"
    ));
    // ...and survives for a view opened later.
    let stored = engine
        .store
        .deployment_logs(&deployment.id)
        .await
        .expect("logs");
    assert_eq!(stored.len(), 1);
    assert_eq!(stored[0].line, "compiling bot v2");
}

#[tokio::test]
async fn blank_output_lines_are_not_recorded() {
    let engine = engine().await;
    let (_, service_id) = service_on_target(&engine, false).await;
    let deployment = engine
        .store
        .create_deployment(&NewDeployment {
            service_id,
            actor: nudo_proto::Actor::human("u", "u"),
            previous_release_id: String::new(),
            git_ref: String::new(),
            trigger: DeployTrigger::Manual,
        })
        .await
        .expect("create");

    engine.emit(&deployment.id, "   ", false).await;
    engine.emit(&deployment.id, "", false).await;
    assert!(
        engine
            .store
            .deployment_logs(&deployment.id)
            .await
            .expect("logs")
            .is_empty()
    );
}

#[tokio::test]
async fn rolling_back_the_first_ever_deployment_reports_that_there_is_no_target() {
    let engine = engine().await;
    let (_, service_id) = service_on_target(&engine, false).await;
    let deployment = engine
        .store
        .create_deployment(&NewDeployment {
            service_id,
            actor: nudo_proto::Actor::human("u", "u"),
            previous_release_id: String::new(),
            git_ref: String::new(),
            trigger: DeployTrigger::Manual,
        })
        .await
        .expect("create");

    let error = engine
        .rollback_after_failure(&deployment.id)
        .await
        .expect_err("must fail");
    assert!(error.to_string().contains("first release"), "got: {error}");
}

#[tokio::test]
async fn a_terminal_status_is_broadcast_before_the_channel_closes() {
    let engine = engine().await;
    let (_, service_id) = service_on_target(&engine, false).await;
    let deployment = engine
        .store
        .create_deployment(&NewDeployment {
            service_id,
            actor: nudo_proto::Actor::human("u", "u"),
            previous_release_id: String::new(),
            git_ref: String::new(),
            trigger: DeployTrigger::Manual,
        })
        .await
        .expect("create");

    let mut watcher = engine.bus.watch_deployment(&deployment.id);
    engine
        .finish(&deployment.id, deployment::Status::Succeeded)
        .await;

    assert!(matches!(
        watcher.recv().await.expect("event"),
        DeploymentEvent::Finished(deployment::Status::Succeeded)
    ));
    // And the row reflects it.
    let stored = engine
        .store
        .get_deployment(&deployment.id)
        .await
        .expect("get")
        .expect("some");
    assert_eq!(stored.status, deployment::Status::Succeeded as i32);
    assert!(stored.finished_at.is_some());
}
