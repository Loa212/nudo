//! Deploying a binary: the unit becomes active, a failed health check rolls
//! back, secrets arrive as a locked-down environment file, and a host key is
//! pinned on first use.

use std::time::Duration;

use nudo_proto::{
    ArtifactSource, HealthCheck, Service, SystemdUnit, artifact_source, deployment, health_check,
};
use nudo_server::crypto::SecretKey;
use nudo_server::deploy::DeployOptions;
use nudo_server::ssh::SshSession;
use nudo_server::store::TargetInput;

use crate::fixture::*;

#[tokio::test(flavor = "multi_thread")]
async fn a_binary_deploys_and_the_unit_becomes_active() {
    let fixture = Fixture::start().expect("start the target container");
    let secret_key = SecretKey::generate();
    let (engine, _dir) = engine(secret_key.clone()).await;

    // ---- register the target, with its key in the secret store ----
    let key_secret = engine
        .store
        .put_secret(
            &secret_key,
            "E2E_SSH_KEY",
            &fixture.private_key,
            "",
            "",
            false,
        )
        .await
        .expect("store the key");

    let target = engine
        .store
        .create_target(&TargetInput {
            name: "e2e-target".to_string(),
            host: "127.0.0.1".to_string(),
            port: SSH_PORT as u32,
            user: "root".to_string(),
            ssh_key_id: key_secret.id,
            latency_critical: false,
            labels: Default::default(),
        })
        .await
        .expect("create the target");

    // ---- the readiness probe should pass against a real host ----
    let (ok, checks) =
        nudo_server::probe::check_target(engine.connect(&target).await, "/opt").await;
    for check in &checks {
        eprintln!(
            "check {:<12} {} {}",
            check.name,
            if check.ok { "ok" } else { "FAIL" },
            check.detail
        );
    }
    assert!(ok, "the readiness probe failed against a real systemd host");

    // ---- a service with a real HTTP health check ----
    let service = engine
        .store
        .create_service(&Service {
            target_id: target.id.clone(),
            name: "e2e-bot".to_string(),
            artifact: Some(ArtifactSource {
                kind: Some(artifact_source::Kind::DirectUpload(true)),
            }),
            unit: Some(SystemdUnit {
                unit_name: "e2e-bot.service".to_string(),
                description: "end-to-end test service".to_string(),
                restart: "always".to_string(),
                restart_sec: 1,
                ..Default::default()
            }),
            health_check: Some(ready_check()),
            release_root: "/opt/e2e-bot".to_string(),
            keep_releases: 5,
            ..Default::default()
        })
        .await
        .expect("create the service");

    // ---- deploy v1 ----
    let artifact = tempfile::NamedTempFile::new().expect("tempfile");
    std::fs::write(artifact.path(), make_artifact("v1", true)).expect("write the artifact");

    let first = engine
        .start_deploy(
            &service.id,
            nudo_proto::Actor::human("usr_e2e", "e2e test"),
            DeployOptions {
                uploaded_artifact: Some(artifact.path().to_path_buf()),
                auto_rollback_on_failure: true,
                ..Default::default()
            },
        )
        .await
        .expect("queue the first deploy");

    let status = await_deployment(&engine, &first.id).await;
    if status != deployment::Status::Succeeded {
        dump_logs(&engine, &first.id, "first deploy").await;
    }
    assert_eq!(
        status,
        deployment::Status::Succeeded,
        "the first deploy should succeed"
    );

    // ---- the unit really is active on the target ----
    let session = SshSession::connect(&fixture.ssh_target())
        .await
        .expect("connect");

    let active = session
        .exec("systemctl is-active e2e-bot.service")
        .await
        .expect("query the unit");
    assert_eq!(active.trimmed(), "active", "the unit should be active");

    // Enabled, so it survives a reboot.
    let enabled = session
        .exec("systemctl is-enabled e2e-bot.service")
        .await
        .expect("query the unit");
    assert_eq!(enabled.trimmed(), "enabled", "the unit should be enabled");

    // The symlink points at the release, and the binary is there and executable.
    let link = session
        .exec("readlink -f /opt/e2e-bot/current")
        .await
        .expect("read the symlink");
    let first_release = engine
        .store
        .get_service(&service.id)
        .await
        .expect("read")
        .expect("service")
        .current_release_id;
    assert!(
        link.trimmed().contains(&first_release),
        "current should point at {first_release}, got {}",
        link.trimmed()
    );

    let executable = session
        .exec("test -x /opt/e2e-bot/current/bin && echo yes")
        .await
        .expect("check the binary");
    assert_eq!(
        executable.trimmed(),
        "yes",
        "the deployed binary should be executable"
    );

    // And the running process is v1's, not a leftover.
    let served = session
        .exec(&format!("cat {READY_FILE}"))
        .await
        .expect("read the readiness file");
    assert_eq!(served.trimmed(), "v1", "v1 should be the version running");

    // ---- the unit file the engine wrote matches what RenderUnit previews ----
    let on_disk = session
        .exec("cat /etc/systemd/system/e2e-bot.service")
        .await
        .expect("read the unit");
    let expected = nudo_server::systemd::render_unit(
        &engine
            .store
            .get_service(&service.id)
            .await
            .expect("read")
            .expect("service"),
    );
    assert_eq!(
        on_disk.stdout, expected,
        "the unit on the target should be exactly what RenderUnit returns"
    );

    let _ = session.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_failed_health_check_rolls_back_to_the_previous_release() {
    let fixture = Fixture::start().expect("start the target container");
    let secret_key = SecretKey::generate();
    let (engine, _dir) = engine(secret_key.clone()).await;

    let key_secret = engine
        .store
        .put_secret(
            &secret_key,
            "E2E_SSH_KEY",
            &fixture.private_key,
            "",
            "",
            false,
        )
        .await
        .expect("store the key");

    let target = engine
        .store
        .create_target(&TargetInput {
            name: "e2e-target".to_string(),
            host: "127.0.0.1".to_string(),
            port: SSH_PORT as u32,
            user: "root".to_string(),
            ssh_key_id: key_secret.id,
            latency_critical: false,
            labels: Default::default(),
        })
        .await
        .expect("create the target");

    let service = engine
        .store
        .create_service(&Service {
            target_id: target.id.clone(),
            name: "e2e-rollback".to_string(),
            artifact: Some(ArtifactSource {
                kind: Some(artifact_source::Kind::DirectUpload(true)),
            }),
            unit: Some(SystemdUnit {
                unit_name: "e2e-rollback.service".to_string(),
                restart: "always".to_string(),
                restart_sec: 1,
                ..Default::default()
            }),
            health_check: Some(HealthCheck {
                // Few retries: the point is to fail, and quickly.
                retries: 2,
                ..ready_check()
            }),
            release_root: "/opt/e2e-rollback".to_string(),
            keep_releases: 5,
            ..Default::default()
        })
        .await
        .expect("create the service");

    // ---- a good release first, so there is something to roll back to ----
    let good = tempfile::NamedTempFile::new().expect("tempfile");
    std::fs::write(good.path(), make_artifact("v1", true)).expect("write");

    let first = engine
        .start_deploy(
            &service.id,
            nudo_proto::Actor::human("usr_e2e", "e2e test"),
            DeployOptions {
                uploaded_artifact: Some(good.path().to_path_buf()),
                auto_rollback_on_failure: true,
                ..Default::default()
            },
        )
        .await
        .expect("queue the good deploy");

    let status = await_deployment(&engine, &first.id).await;
    if status != deployment::Status::Succeeded {
        dump_logs(&engine, &first.id, "good deploy").await;
    }
    assert_eq!(status, deployment::Status::Succeeded);

    let good_release = engine
        .store
        .get_service(&service.id)
        .await
        .expect("read")
        .expect("service")
        .current_release_id;
    assert!(!good_release.is_empty());

    // ---- then a release that starts but never serves ----
    let bad = tempfile::NamedTempFile::new().expect("tempfile");
    std::fs::write(bad.path(), make_artifact("v2-broken", false)).expect("write");

    let second = engine
        .start_deploy(
            &service.id,
            nudo_proto::Actor::human("usr_e2e", "e2e test"),
            DeployOptions {
                uploaded_artifact: Some(bad.path().to_path_buf()),
                auto_rollback_on_failure: true,
                ..Default::default()
            },
        )
        .await
        .expect("queue the bad deploy");

    let status = await_deployment(&engine, &second.id).await;
    dump_logs(&engine, &second.id, "bad deploy").await;

    // The deploy must be reported as rolled back, not as succeeded — the unit
    // was active the whole time, so a tool that only checks systemd would have
    // called this a success.
    assert_eq!(
        status,
        deployment::Status::RolledBack,
        "a release that starts but does not serve must roll back"
    );

    // ---- the previous release is live again and serving ----
    let session = SshSession::connect(&fixture.ssh_target())
        .await
        .expect("connect");

    let link = session
        .exec("readlink -f /opt/e2e-rollback/current")
        .await
        .expect("read the symlink");
    assert!(
        link.trimmed().contains(&good_release),
        "current should be back at {good_release}, got {}",
        link.trimmed()
    );

    let active = session
        .exec("systemctl is-active e2e-rollback.service")
        .await
        .expect("query the unit");
    assert_eq!(
        active.trimmed(),
        "active",
        "the rolled-back unit should be running"
    );

    // Healthy again, which is the property that actually matters: the rollback
    // has to restore a *working* service, not merely an active unit.
    wait_for_async(Duration::from_secs(30), || async {
        session
            .exec(&format!("cat {READY_FILE} 2>/dev/null"))
            .await
            .map(|result| result.trimmed() == "v1")
            .unwrap_or(false)
    })
    .await
    .expect("the rolled-back release should be healthy again");

    // The service row agrees with the target.
    let reloaded = engine
        .store
        .get_service(&service.id)
        .await
        .expect("read")
        .expect("service");
    assert_eq!(
        reloaded.current_release_id, good_release,
        "the recorded current release should match what is on the target"
    );

    let _ = session.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn secrets_reach_the_target_as_a_locked_down_environment_file() {
    let fixture = Fixture::start().expect("start the target container");
    let secret_key = SecretKey::generate();
    let (engine, _dir) = engine(secret_key.clone()).await;

    let key_secret = engine
        .store
        .put_secret(
            &secret_key,
            "E2E_SSH_KEY",
            &fixture.private_key,
            "",
            "",
            false,
        )
        .await
        .expect("store the key");

    let target = engine
        .store
        .create_target(&TargetInput {
            name: "e2e-target".to_string(),
            host: "127.0.0.1".to_string(),
            port: SSH_PORT as u32,
            user: "root".to_string(),
            ssh_key_id: key_secret.id,
            latency_critical: false,
            labels: Default::default(),
        })
        .await
        .expect("create the target");

    // A value with characters that would break a naive environment file.
    let hostile_value = "p@ss\"word with spaces and $DOLLAR";
    let app_secret = engine
        .store
        .put_secret(&secret_key, "APP_TOKEN", hostile_value, "", "", false)
        .await
        .expect("store the secret");

    let service = engine
        .store
        .create_service(&Service {
            target_id: target.id.clone(),
            name: "e2e-secrets".to_string(),
            artifact: Some(ArtifactSource {
                kind: Some(artifact_source::Kind::DirectUpload(true)),
            }),
            unit: Some(SystemdUnit {
                unit_name: "e2e-secrets.service".to_string(),
                restart: "no".to_string(),
                ..Default::default()
            }),
            health_check: Some(HealthCheck {
                // The unit exits immediately, so trust systemd rather than
                // polling a port.
                kind: Some(health_check::Kind::SystemdActive(true)),
                retries: 1,
                initial_delay_seconds: 1,
                timeout_seconds: 5,
            }),
            release_root: "/opt/e2e-secrets".to_string(),
            secret_ids: vec![app_secret.id],
            ..Default::default()
        })
        .await
        .expect("create the service");

    // A binary that writes its environment where the test can read it.
    let artifact = tempfile::NamedTempFile::new().expect("tempfile");
    std::fs::write(
        artifact.path(),
        b"#!/bin/bash\nprintenv APP_TOKEN > /tmp/seen-token\nsleep 30\n",
    )
    .expect("write");

    let deployment = engine
        .start_deploy(
            &service.id,
            nudo_proto::Actor::human("usr_e2e", "e2e test"),
            DeployOptions {
                uploaded_artifact: Some(artifact.path().to_path_buf()),
                // `Restart=no` plus an immediate exit means systemd may report
                // the unit inactive before the check runs; the point of this
                // test is the environment file, not the health check.
                skip_health_check: true,
                ..Default::default()
            },
        )
        .await
        .expect("queue the deploy");

    let status = await_deployment(&engine, &deployment.id).await;
    if status != deployment::Status::Succeeded {
        dump_logs(&engine, &deployment.id, "secrets deploy").await;
    }
    assert_eq!(status, deployment::Status::Succeeded);

    let session = SshSession::connect(&fixture.ssh_target())
        .await
        .expect("connect");

    // The service saw the exact value, escaping and all.
    wait_for_async(Duration::from_secs(20), || async {
        session
            .exec("cat /tmp/seen-token 2>/dev/null")
            .await
            .map(|result| result.trimmed() == hostile_value)
            .unwrap_or(false)
    })
    .await
    .unwrap_or_else(|_| {
        panic!("the service did not receive APP_TOKEN intact");
    });

    // The environment file is not world-readable — a secret written 0644 would
    // be readable by every account on the box.
    let mode = session
        .exec("stat -c '%a' /opt/e2e-secrets/env")
        .await
        .expect("stat");
    assert_eq!(mode.trimmed(), "600", "the environment file must be 0600");

    // And the unit references it rather than inlining the value, so the secret
    // does not appear in `systemctl cat`.
    let unit = session
        .exec("cat /etc/systemd/system/e2e-secrets.service")
        .await
        .expect("read the unit");
    assert!(
        unit.stdout
            .contains("EnvironmentFile=-/opt/e2e-secrets/env")
    );
    assert!(
        !unit.stdout.contains(hostile_value),
        "the secret value must not appear in the unit file"
    );

    let _ = session.close().await;
}

/// The whole host-key lifecycle against a real sshd: pin, verify, refuse a
/// change, review it, accept it, connect again.
///
/// This is the test that proves the feature does what it claims. The unit tests
/// exercise the decision rule; only a real server actually presenting a real
/// host key — and then a different one — shows that the refusal happens where it
/// has to, which is before authentication.
#[tokio::test(flavor = "multi_thread")]
async fn a_host_key_is_pinned_on_first_use_and_a_change_is_refused_until_accepted() {
    let fixture = Fixture::start().expect("start the target container");
    let secret_key = SecretKey::generate();
    let (engine, _dir) = engine(secret_key.clone()).await;

    let key_secret = engine
        .store
        .put_secret(
            &secret_key,
            "E2E_SSH_KEY",
            &fixture.private_key,
            "",
            "",
            false,
        )
        .await
        .expect("store the key");

    let target = engine
        .store
        .create_target(&TargetInput {
            name: "e2e-host-key".to_string(),
            host: "127.0.0.1".to_string(),
            port: SSH_PORT as u32,
            user: "root".to_string(),
            ssh_key_id: key_secret.id,
            latency_critical: false,
            labels: Default::default(),
        })
        .await
        .expect("create the target");

    // ---- nothing pinned: the first connection records what the host presents ----
    let reload = |id: String| {
        let engine = engine.clone();
        async move {
            engine
                .store
                .get_target(&id)
                .await
                .expect("read the target")
                .expect("the target exists")
        }
    };

    assert!(
        reload(target.id.clone()).await.host_key.is_none(),
        "a target that has never connected must not have a pinned key"
    );

    let session = engine.connect(&target).await.expect("first connection");
    let _ = session.close().await;

    let pinned = reload(target.id.clone())
        .await
        .host_key
        .expect("the first connection must pin a key");
    assert!(!pinned.key.is_empty());
    assert!(pinned.fingerprint.starts_with("SHA256:"));
    assert!(pinned.pending_key.is_empty());

    // What was pinned must be the machine's own key, not merely something
    // self-consistent: this is checked against the container's key file rather
    // than against anything nudo produced.
    let actual = fixture.host_key().expect("read the container's host key");
    let actual_key_only = actual
        .split_whitespace()
        .take(2)
        .collect::<Vec<_>>()
        .join(" ");
    assert_eq!(
        pinned.key, actual_key_only,
        "the pinned key must be the host's actual key"
    );

    // ---- pinned: a second connection verifies rather than re-pins ----
    // Re-read first, as every caller does: what is pinned is carried on the
    // target, so connecting with a stale copy would be first use all over again.
    let target = reload(target.id.clone()).await;
    let session = engine.connect(&target).await.expect("second connection");
    let _ = session.close().await;
    let unchanged = reload(target.id.clone()).await.host_key.expect("host key");
    assert_eq!(unchanged.key, pinned.key);
    assert_eq!(
        unchanged.pinned_at, pinned.pinned_at,
        "an unchanged key must not be re-pinned on every connection"
    );

    // ---- the host key changes, as a rebuilt machine's would ----
    fixture
        .regenerate_host_key()
        .expect("regenerate the container's host key");

    let error = engine
        .connect(&target)
        .await
        .expect_err("a changed host key must refuse the connection");
    assert!(
        error.is::<nudo_server::ssh::HostKeyChanged>(),
        "expected a host-key refusal, got: {error:#}"
    );
    let message = format!("{error:#}");
    assert!(message.contains(&pinned.fingerprint), "got: {message}");

    // The change is held for review rather than only reported, and the pinned
    // key is untouched — it is still what every connection is checked against.
    let refused = reload(target.id.clone()).await.host_key.expect("host key");
    assert_eq!(refused.key, pinned.key, "the pinned key must survive");
    assert!(!refused.pending_key.is_empty());
    assert_ne!(refused.pending_fingerprint, pinned.fingerprint);

    // The pending key must be the host's new key, so what an operator reviews
    // is what the machine actually presented.
    let new_actual = fixture.host_key().expect("read the new host key");
    let new_key_only = new_actual
        .split_whitespace()
        .take(2)
        .collect::<Vec<_>>()
        .join(" ");
    assert_eq!(refused.pending_key, new_key_only);

    // ---- every operation stays refused, not just deploys ----
    // A mismatch may mean this is not the host at all, so reading from it is no
    // safer than writing to it.
    let (ok, checks) =
        nudo_server::probe::check_target(engine.connect(&target).await, "/opt").await;
    assert!(
        !ok,
        "the preflight check must fail while the key is unreviewed"
    );
    let host_key_check = checks
        .iter()
        .find(|c| c.name == "host_key")
        .expect("a host_key check");
    assert!(!host_key_check.ok);
    let ssh_check = checks
        .iter()
        .find(|c| c.name == "ssh")
        .expect("an ssh check");
    assert!(
        ssh_check.detail.contains("host key was refused"),
        "ssh must not be blamed for a deliberate refusal, got: {}",
        ssh_check.detail
    );

    // ---- accepting the reviewed key restores service ----
    engine
        .store
        .pin_host_key(
            nudo_server::store::SshHost::Target,
            &target.id,
            &refused.pending_key,
            &refused.pending_fingerprint,
        )
        .await
        .expect("accept the reviewed key");

    let target = reload(target.id.clone()).await;
    let session = engine
        .connect(&target)
        .await
        .expect("connecting must work again once the new key is accepted");
    let whoami = session.exec("id -un").await.expect("run a command");
    assert_eq!(whoami.trimmed(), "root");
    let _ = session.close().await;

    let accepted = reload(target.id.clone()).await.host_key.expect("host key");
    assert_eq!(accepted.key, new_key_only);
    assert!(
        accepted.pending_key.is_empty(),
        "accepting must clear the review"
    );
}
