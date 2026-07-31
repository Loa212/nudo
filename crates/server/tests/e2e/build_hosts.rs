//! Building on a build host: readiness checks, a build that reaches the target,
//! a failed build that still cleans up, and host-key pinning.
//!
//! The same container serves as both a target and a build host here. That is a
//! configuration nudo does not endorse — the whole point of the feature is that
//! they are different roles on different machines — but for the test it is
//! exactly right: one fixture, and any confusion between the two roles shows up
//! as a wrong path or a wrong unit rather than being hidden by them being on
//! separate hosts.

use std::time::Duration;

use nudo_proto::{ArtifactSource, Service, SystemdUnit, artifact_source, deployment};
use nudo_server::crypto::SecretKey;
use nudo_server::deploy::{DeployOptions, Engine};
use nudo_server::ssh::SshSession;
use nudo_server::store::TargetInput;

use crate::fixture::*;

// ---------------------------------------------------------------------------
// Build hosts
// ---------------------------------------------------------------------------
//
// The same container serves as both a target and a build host here. That is a
// configuration nudo does not endorse — the whole point of the feature is that
// they are different roles on different machines — but for the test it is
// exactly right: one fixture, and any confusion between the two roles shows up
// as a wrong path or a wrong unit rather than being hidden by them being on
// separate hosts.

/// Registers the fixture as a build host, with its key in the secret store.
async fn register_build_host(
    engine: &Engine,
    secret_key: &SecretKey,
    fixture: &Fixture,
    latency_critical: bool,
) -> nudo_proto::BuildHost {
    let key_secret = engine
        .store
        .put_secret(
            secret_key,
            "E2E_BUILD_HOST_KEY",
            &fixture.private_key,
            "",
            "",
            false,
        )
        .await
        .expect("store the build host's key");

    engine
        .store
        .create_build_host(&nudo_server::store::BuildHostInput {
            name: "e2e-builder".to_string(),
            host: "127.0.0.1".to_string(),
            port: SSH_PORT as u32,
            user: "root".to_string(),
            ssh_key_id: key_secret.id,
            workspace_root: "/var/lib/nudo/builds".to_string(),
            latency_critical,
            labels: Default::default(),
        })
        .await
        .expect("create the build host")
}

#[tokio::test]
async fn a_build_host_passes_its_own_readiness_checks() {
    // The build-host check is deliberately not the target check: it wants a
    // writable workspace and git, and does not ask for sudo or systemd. This
    // asserts that against a real host rather than against the shape of the
    // code.
    let fixture = Fixture::start().expect("start the container");
    let secret_key = SecretKey::generate();
    let (engine, _dir) = engine(secret_key.clone()).await;

    let build_host = register_build_host(&engine, &secret_key, &fixture, false).await;

    let (ok, checks, warnings) = nudo_server::probe::check_build_host(
        engine.connect(&build_host).await,
        &build_host.workspace_root,
        build_host.latency_critical,
    )
    .await;

    for check in &checks {
        eprintln!(
            "check {:<12} {} {}",
            check.name,
            if check.ok { "ok" } else { "FAIL" },
            check.detail
        );
    }
    assert!(ok, "the build-host probe failed against a real host");

    let names: Vec<&str> = checks.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(names, ["host_key", "ssh", "workspace", "git"]);
    assert!(
        warnings.is_empty(),
        "a host that is not latency-critical has nothing to warn about: {warnings:?}"
    );

    // The workspace check must have actually created the root, since the first
    // build depends on it existing.
    assert!(
        fixture.path_exists(&build_host.workspace_root),
        "the workspace check should create the root it probes"
    );
}

#[tokio::test]
async fn a_latency_critical_build_host_warns_but_still_passes() {
    // The decision this feature turns on: allowed, not refused. A warning must
    // not make the check fail, or a CI step gating on readiness would break on
    // a host working exactly as configured.
    let fixture = Fixture::start().expect("start the container");
    let secret_key = SecretKey::generate();
    let (engine, _dir) = engine(secret_key.clone()).await;

    let build_host = register_build_host(&engine, &secret_key, &fixture, true).await;

    let (ok, _checks, warnings) = nudo_server::probe::check_build_host(
        engine.connect(&build_host).await,
        &build_host.workspace_root,
        build_host.latency_critical,
    )
    .await;

    assert!(ok, "a latency-critical build host is still usable");
    assert_eq!(warnings.len(), 1, "got: {warnings:?}");
    assert!(
        warnings[0].contains("latency-critical"),
        "got: {}",
        warnings[0]
    );
}

#[tokio::test]
async fn a_service_builds_on_a_build_host_and_the_artifact_reaches_the_target() {
    // The end-to-end path this whole feature exists for: clone on a machine
    // that is not the control plane, run the build command there, bring the
    // binary back, and ship it — with the deploy ending in a unit that systemd
    // reports active.
    let fixture = Fixture::start().expect("start the container");
    let secret_key = SecretKey::generate();
    let (engine, _dir) = engine(secret_key.clone()).await;

    fixture
        .seed_repository("acme", "bot", "v1")
        .expect("seed the repository");

    let build_host = register_build_host(&engine, &secret_key, &fixture, false).await;

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
            name: "e2e-built".to_string(),
            artifact: Some(ArtifactSource {
                kind: Some(artifact_source::Kind::Git(nudo_proto::GitSource {
                    // No source_id: a public repository, which is the path that
                    // needs no credentials on the build host.
                    repo: "acme/bot".to_string(),
                    branch: "main".to_string(),
                    build_command: "./build.sh".to_string(),
                    artifact_path: "dist/bot".to_string(),
                    build_host_id: build_host.id.clone(),
                    ..Default::default()
                })),
            }),
            unit: Some(SystemdUnit {
                unit_name: "e2e-built.service".to_string(),
                description: "built on a build host".to_string(),
                restart: "always".to_string(),
                restart_sec: 1,
                ..Default::default()
            }),
            health_check: Some(ready_check()),
            release_root: "/opt/e2e-built".to_string(),
            keep_releases: 5,
            ..Default::default()
        })
        .await
        .expect("create the service");

    let deployment = engine
        .start_deploy(
            &service.id,
            nudo_proto::Actor::human("usr_e2e", "e2e test"),
            DeployOptions {
                auto_rollback_on_failure: true,
                ..Default::default()
            },
        )
        .await
        .expect("start the deploy");

    let status = await_deployment(&engine, &deployment.id).await;
    if status != deployment::Status::Succeeded {
        dump_logs(&engine, &deployment.id, "build-host deploy").await;
    }
    assert_eq!(
        status,
        deployment::Status::Succeeded,
        "a service built on a build host should deploy"
    );

    // ---- the unit is actually running the built artifact ----
    let active = exec_in_container(&["systemctl", "is-active", "e2e-built.service"])
        .unwrap_or_default()
        .trim()
        .to_string();
    assert_eq!(active, "active", "the built unit should be running");

    // ---- the build really happened on the build host ----
    let logs = engine
        .store
        .deployment_logs(&deployment.id)
        .await
        .expect("logs");
    let text: String = logs
        .iter()
        .map(|line| line.line.as_str())
        .collect::<Vec<_>>()
        .join("\n");

    assert!(
        text.contains("cloning acme/bot at main"),
        "the clone should be reported as it is locally: {text}"
    );
    assert!(
        text.contains("running: ./build.sh"),
        "the build command should be echoed: {text}"
    );
    assert!(
        text.contains("building on "),
        "the build command's own output must be streamed back: {text}"
    );
    assert!(
        text.contains("built dist/bot"),
        "the collected artifact should be reported: {text}"
    );

    // The log must not say where the build ran — that is configuration, not
    // output, and anything parsing a deployment log should not break when an
    // operator adds a build host.
    assert!(
        !text.contains("e2e-builder") && !text.to_lowercase().contains("build host"),
        "the deploy log should not reveal where the build ran: {text}"
    );

    // ---- the workspace is gone ----
    // A build host that accumulates checkouts fills up, and that failure then
    // belongs to every service built there.
    assert!(
        !fixture.path_exists(&format!("/var/lib/nudo/builds/{}", deployment.id)),
        "the build workspace should be removed after a successful build"
    );
}

#[tokio::test]
async fn a_failed_remote_build_reports_it_and_still_cleans_up() {
    // The half of cleanup that is easy to get wrong: a build that exits
    // non-zero must fail the deploy *and* leave nothing behind.
    let fixture = Fixture::start().expect("start the container");
    let secret_key = SecretKey::generate();
    let (engine, _dir) = engine(secret_key.clone()).await;

    fixture
        .seed_repository("acme", "bot", "v1")
        .expect("seed the repository");

    let build_host = register_build_host(&engine, &secret_key, &fixture, false).await;

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
            name: "e2e-broken-build".to_string(),
            artifact: Some(ArtifactSource {
                kind: Some(artifact_source::Kind::Git(nudo_proto::GitSource {
                    repo: "acme/bot".to_string(),
                    branch: "main".to_string(),
                    build_command: "echo 'compiler said no' >&2; exit 3".to_string(),
                    artifact_path: "dist/bot".to_string(),
                    build_host_id: build_host.id.clone(),
                    ..Default::default()
                })),
            }),
            unit: Some(SystemdUnit {
                unit_name: "e2e-broken-build.service".to_string(),
                restart: "always".to_string(),
                restart_sec: 1,
                ..Default::default()
            }),
            health_check: Some(ready_check()),
            release_root: "/opt/e2e-broken-build".to_string(),
            keep_releases: 5,
            ..Default::default()
        })
        .await
        .expect("create the service");

    let deployment = engine
        .start_deploy(
            &service.id,
            nudo_proto::Actor::human("usr_e2e", "e2e test"),
            DeployOptions {
                auto_rollback_on_failure: true,
                ..Default::default()
            },
        )
        .await
        .expect("start the deploy");

    let status = await_deployment(&engine, &deployment.id).await;
    assert_eq!(
        status,
        deployment::Status::Failed,
        "a build command exiting non-zero must fail the deploy"
    );

    let logs = engine
        .store
        .deployment_logs(&deployment.id)
        .await
        .expect("logs");
    let text: String = logs
        .iter()
        .map(|line| line.line.as_str())
        .collect::<Vec<_>>()
        .join("\n");

    // The build's own stderr has to survive the trip back, or a failing build
    // is undiagnosable from the dashboard.
    assert!(
        text.contains("compiler said no"),
        "the build's stderr should reach the deployment log: {text}"
    );
    assert!(
        text.contains("status 3"),
        "the exit status should be reported: {text}"
    );

    // Nothing was deployed.
    assert!(
        exec_in_container(&["systemctl", "is-active", "e2e-broken-build.service"])
            .unwrap_or_default()
            .trim()
            != "active",
        "a failed build must not leave a running unit"
    );

    // And the workspace is gone even though the build failed.
    assert!(
        !fixture.path_exists(&format!("/var/lib/nudo/builds/{}", deployment.id)),
        "the build workspace should be removed after a failed build too"
    );
}

#[tokio::test]
async fn a_bad_remote_build_rolls_back_to_the_previously_built_release() {
    // The full cycle, with both releases produced by a build on a build host
    // rather than uploaded: build v1, ship it, then build a v2 that starts but
    // never becomes ready, and assert the rollback put the *built* v1 back and
    // that it is serving again.
    //
    // The activation path is shared with an uploaded artifact, so what is new
    // here is that the release being rolled back *to* came off a build host —
    // its directory, its recorded release id and the symlink all have to line
    // up for the rollback to find it.
    let fixture = Fixture::start().expect("start the container");
    let secret_key = SecretKey::generate();
    let (engine, _dir) = engine(secret_key.clone()).await;

    fixture
        .seed_repository("acme", "rollback", "v1")
        .expect("seed the repository");

    let build_host = register_build_host(&engine, &secret_key, &fixture, false).await;

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
            name: "e2e-build-rollback".to_string(),
            artifact: Some(ArtifactSource {
                kind: Some(artifact_source::Kind::Git(nudo_proto::GitSource {
                    repo: "acme/rollback".to_string(),
                    branch: "main".to_string(),
                    build_command: "./build.sh".to_string(),
                    artifact_path: "dist/bot".to_string(),
                    build_host_id: build_host.id.clone(),
                    ..Default::default()
                })),
            }),
            unit: Some(SystemdUnit {
                unit_name: "e2e-build-rollback.service".to_string(),
                description: "rollback between two remote builds".to_string(),
                restart: "always".to_string(),
                restart_sec: 1,
                ..Default::default()
            }),
            health_check: Some(ready_check()),
            release_root: "/opt/e2e-build-rollback".to_string(),
            keep_releases: 5,
            ..Default::default()
        })
        .await
        .expect("create the service");

    // ---- build and deploy v1 ----
    let first = engine
        .start_deploy(
            &service.id,
            nudo_proto::Actor::human("usr_e2e", "e2e test"),
            DeployOptions {
                auto_rollback_on_failure: true,
                ..Default::default()
            },
        )
        .await
        .expect("queue the first deploy");

    let status = await_deployment(&engine, &first.id).await;
    if status != deployment::Status::Succeeded {
        dump_logs(&engine, &first.id, "first remote build").await;
    }
    assert_eq!(
        status,
        deployment::Status::Succeeded,
        "the first remote build should deploy"
    );

    let good_release = engine
        .store
        .get_service(&service.id)
        .await
        .expect("read")
        .expect("service")
        .current_release_id;
    assert!(
        !good_release.is_empty(),
        "the first build should have produced a live release"
    );

    // ---- push a v2 whose build produces a binary that never becomes ready ----
    fixture
        .commit_to_repository("rollback", "v2", false)
        .expect("commit the bad version");

    let second = engine
        .start_deploy(
            &service.id,
            nudo_proto::Actor::human("usr_e2e", "e2e test"),
            DeployOptions {
                auto_rollback_on_failure: true,
                ..Default::default()
            },
        )
        .await
        .expect("queue the bad deploy");

    let status = await_deployment(&engine, &second.id).await;
    dump_logs(&engine, &second.id, "bad remote build").await;

    // The build itself succeeded — it is the *service* that never becomes
    // ready. A tool that only asked systemd would have called this a success,
    // since the unit is active throughout.
    assert_eq!(
        status,
        deployment::Status::RolledBack,
        "a built release that starts but does not serve must roll back"
    );

    // ---- the previously built release is live again and serving ----
    let session = SshSession::connect(&fixture.ssh_target())
        .await
        .expect("connect");

    let link = session
        .exec("readlink -f /opt/e2e-build-rollback/current")
        .await
        .expect("read the symlink");
    assert!(
        link.trimmed().contains(&good_release),
        "current should be back at {good_release}, got {}",
        link.trimmed()
    );

    let active = session
        .exec("systemctl is-active e2e-build-rollback.service")
        .await
        .expect("query the unit");
    assert_eq!(
        active.trimmed(),
        "active",
        "the rolled-back unit should be running"
    );

    // Healthy again, which is the property that actually matters: the rollback
    // has to restore a *working* service, not merely an active unit. v1 is what
    // the first build produced, so this also proves the rolled-back binary is
    // the one that build emitted rather than whatever was there before.
    wait_for_async(Duration::from_secs(30), || async {
        session
            .exec(&format!("cat {READY_FILE} 2>/dev/null"))
            .await
            .map(|result| result.trimmed() == "v1")
            .unwrap_or(false)
    })
    .await
    .expect("the rolled-back build should be healthy again");

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

    // Both builds cleaned up after themselves, the rolled-back one included.
    for deployment_id in [&first.id, &second.id] {
        assert!(
            !fixture.path_exists(&format!("/var/lib/nudo/builds/{deployment_id}")),
            "the workspace for {deployment_id} should have been removed"
        );
    }

    let _ = session.close().await;
}

#[tokio::test]
async fn a_build_host_pins_its_host_key_like_a_target_does() {
    // A build host is handed repository credentials, so connecting to the wrong
    // machine matters at least as much here as for a deploy target. This is the
    // target host-key test aimed at the other noun.
    let fixture = Fixture::start().expect("start the container");
    let secret_key = SecretKey::generate();
    let (engine, _dir) = engine(secret_key.clone()).await;

    let build_host = register_build_host(&engine, &secret_key, &fixture, false).await;
    assert!(
        build_host.host_key.is_none(),
        "nothing is pinned before the first connection"
    );

    // First connection pins whatever the host presents.
    engine
        .connect(&build_host)
        .await
        .expect("connect")
        .close()
        .await
        .ok();

    let pinned = engine
        .store
        .get_build_host(&build_host.id)
        .await
        .expect("read")
        .expect("exists")
        .host_key
        .expect("a key is pinned after the first connection");
    let expected = fixture.host_key().expect("read the container's host key");
    assert_eq!(
        pinned.fingerprint,
        nudo_server::ssh::fingerprint_of(&expected),
        "the pinned key should be the machine's own"
    );

    // Rebuild the machine's identity; the next connection must refuse.
    fixture
        .regenerate_host_key()
        .expect("regenerate the host key");

    let refreshed = engine
        .store
        .get_build_host(&build_host.id)
        .await
        .expect("read")
        .expect("exists");
    let error = engine
        .connect(&refreshed)
        .await
        .expect_err("a changed host key must be refused");
    assert!(
        error
            .downcast_ref::<nudo_server::ssh::HostKeyChanged>()
            .is_some(),
        "got: {error:#}"
    );

    // The presented key is held for review rather than applied.
    let pending = engine
        .store
        .get_build_host(&build_host.id)
        .await
        .expect("read")
        .expect("exists")
        .host_key
        .expect("host key");
    assert_eq!(
        pending.fingerprint, pinned.fingerprint,
        "the pinned key must not be overwritten by a refused connection"
    );
    assert!(
        !pending.pending_key.is_empty(),
        "the presented key should be recorded for review"
    );
}
