//! End-to-end deployment test against a real SSH target running systemd.
//!
//! This is the test that actually proves the product works: a systemd-enabled
//! container is started, an SSH key is installed into it, and then a real binary
//! is deployed through the real engine — upload, unit file, symlink swap,
//! daemon-reload, restart, health check. Then the health check is made to fail
//! and the automatic rollback is verified to have put the previous release back.
//!
//! Behind the `e2e` feature because it needs Docker. Run it with:
//!
//! ```sh
//! cargo test -p nudo-server --features e2e --test e2e -- --test-threads=1 --nocapture
//! ```
//!
//! A `--test-threads=1` is required: the fixture binds a host port and installs
//! into a shared container name.

#![cfg(feature = "e2e")]

use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

use nudo_proto::{
    ArtifactSource, HealthCheck, Service, SystemdUnit, artifact_source, deployment, health_check,
};
use nudo_server::crypto::SecretKey;
use nudo_server::deploy::{DeployOptions, Engine};
use nudo_server::events::Bus;
use nudo_server::ssh::{SshSession, SshTarget};
use nudo_server::store::{Store, TargetInput};

/// The container the test deploys into.
const CONTAINER: &str = "nudo-e2e-target";

/// The SSH port mapped onto the host.
const SSH_PORT: u16 = 22022;

/// A systemd-enabled image. Debian with systemd as PID 1 is the closest thing to
/// the hosts this tool actually targets.
const IMAGE: &str = "debian:bookworm";

/// A target under test, torn down when dropped.
struct Fixture {
    private_key: String,
}

impl Fixture {
    /// Starts the container, installs sshd and a key, and waits for both.
    fn start() -> anyhow::Result<Self> {
        Self::stop_quietly();

        // systemd needs to be PID 1 for `systemctl` to work at all, which is the
        // whole point of testing against a container rather than a mock.
        run(
            "docker",
            &[
                "run",
                "-d",
                "--name",
                CONTAINER,
                "--privileged",
                "--cgroupns=host",
                "-v",
                "/sys/fs/cgroup:/sys/fs/cgroup:rw",
                "-p",
                &format!("{SSH_PORT}:22"),
                IMAGE,
                "/bin/bash",
                "-c",
                "apt-get update -qq && \
                 DEBIAN_FRONTEND=noninteractive apt-get install -y -qq \
                   systemd systemd-sysv openssh-server curl >/dev/null && \
                 mkdir -p /run/sshd && \
                 exec /lib/systemd/systemd",
            ],
        )?;

        // The apt install inside the container takes a while; systemd is only
        // usable once it has finished and taken over as PID 1.
        wait_for("systemd to come up", Duration::from_secs(240), || {
            exec_in_container(&["systemctl", "is-system-running", "--wait"])
                .map(|output| {
                    // "degraded" is fine in a container: some units cannot
                    // start there and that does not affect what is tested.
                    output.contains("running") || output.contains("degraded")
                })
                .unwrap_or(false)
        })?;

        // A throwaway key pair, installed as root's authorized key.
        let key_dir = tempfile::tempdir()?;
        let key_path = key_dir.path().join("id_ed25519");
        run(
            "ssh-keygen",
            &[
                "-t",
                "ed25519",
                "-N",
                "",
                "-f",
                key_path.to_str().expect("path"),
                "-q",
            ],
        )?;

        let private_key = std::fs::read_to_string(&key_path)?;
        let public_key = std::fs::read_to_string(key_path.with_extension("pub"))?;

        exec_in_container(&["mkdir", "-p", "/root/.ssh"])?;
        exec_in_container(&[
            "bash",
            "-c",
            &format!(
                "printf '%s' {} > /root/.ssh/authorized_keys && chmod 600 /root/.ssh/authorized_keys",
                shell_quote(public_key.trim())
            ),
        ])?;
        exec_in_container(&["systemctl", "enable", "--now", "ssh"])?;

        wait_for(
            "sshd to accept connections",
            Duration::from_secs(60),
            || std::net::TcpStream::connect(("127.0.0.1", SSH_PORT)).is_ok(),
        )?;

        Ok(Self { private_key })
    }

    fn stop_quietly() {
        let _ = Command::new("docker")
            .args(["rm", "-f", CONTAINER])
            .output();
    }

    fn ssh_target(&self) -> SshTarget {
        SshTarget {
            host: "127.0.0.1".to_string(),
            port: SSH_PORT,
            user: "root".to_string(),
            private_key: self.private_key.clone(),
            passphrase: None,
        }
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        Self::stop_quietly();
    }
}

/// Runs a command, failing with its output.
fn run(program: &str, args: &[&str]) -> anyhow::Result<String> {
    let output = Command::new(program).args(args).output()?;
    if !output.status.success() {
        anyhow::bail!(
            "{program} {args:?} failed: {}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Runs a command inside the fixture container.
fn exec_in_container(args: &[&str]) -> anyhow::Result<String> {
    let mut full = vec!["exec", CONTAINER];
    full.extend_from_slice(args);
    run("docker", &full)
}

/// Polls until a condition holds or the deadline passes.
fn wait_for(what: &str, timeout: Duration, mut ready: impl FnMut() -> bool) -> anyhow::Result<()> {
    let deadline = std::time::Instant::now() + timeout;
    while std::time::Instant::now() < deadline {
        if ready() {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    anyhow::bail!("timed out after {timeout:?} waiting for {what}")
}

/// Quotes a value for a shell command line.
fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', r"'\''"))
}

/// Builds an engine wired to a temporary database.
async fn engine(secret_key: SecretKey) -> (Engine, tempfile::TempDir) {
    let dir = tempfile::tempdir().expect("tempdir");
    let store = Store::open(&dir.path().join("nudo.db"))
        .await
        .expect("store");

    let config = nudo_server::Config {
        data_dir: dir.path().to_path_buf(),
        ..nudo_server::Config::default()
    };

    (
        Engine {
            store,
            bus: Bus::default(),
            secret_key,
            config: Arc::new(config),
        },
        dir,
    )
}

/// Waits for a deployment to reach a terminal state and returns it.
async fn await_deployment(engine: &Engine, deployment_id: &str) -> deployment::Status {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(180);
    loop {
        let record = engine
            .store
            .get_deployment(deployment_id)
            .await
            .expect("read the deployment")
            .expect("the deployment exists");

        let status =
            deployment::Status::try_from(record.status).unwrap_or(deployment::Status::Unspecified);
        if status.is_terminal() {
            return status;
        }

        if tokio::time::Instant::now() > deadline {
            // Print the output so a CI failure is diagnosable rather than just
            // "timed out".
            for line in engine
                .store
                .deployment_logs(deployment_id)
                .await
                .unwrap_or_default()
            {
                eprintln!("  {} {}", if line.stderr { "!" } else { " " }, line.line);
            }
            panic!("the deployment did not finish within 180s");
        }

        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

/// Prints a deployment's output, for diagnosing a failure.
async fn dump_logs(engine: &Engine, deployment_id: &str, label: &str) {
    eprintln!("--- {label} ---");
    for line in engine
        .store
        .deployment_logs(deployment_id)
        .await
        .unwrap_or_default()
    {
        eprintln!("  {} {}", if line.stderr { "!" } else { " " }, line.line);
    }
}

/// A tiny "binary" whose readiness the health check can observe.
///
/// A shell script rather than a compiled binary keeps the test self-contained:
/// what is under test is the deploy mechanics, not a compiler. Readiness is
/// signalled by writing a file the health-check command reads, rather than by
/// binding a socket — a listener would need a tool (`nc`, python) that a minimal
/// image does not have, and the mechanism being tested is identical either way.
///
/// When `healthy` is false the process starts and stays up but never becomes
/// ready, so the health check fails while systemd reports the unit active. That
/// is the case a tool which only asks systemd gets wrong, and the one the
/// rollback test depends on.
fn make_artifact(version: &str, healthy: bool) -> Vec<u8> {
    let body = if healthy {
        format!(
            r#"#!/bin/bash
set -euo pipefail
echo "bot {version} starting"
printf '%s' "{version}" > {ready_file}
trap 'rm -f {ready_file}' EXIT
while true; do sleep 3600; done
"#,
            version = version,
            ready_file = READY_FILE
        )
    } else {
        format!(
            r#"#!/bin/bash
set -euo pipefail
echo "bot {version} starting but will never become ready"
rm -f {ready_file}
while true; do sleep 3600; done
"#,
            version = version,
            ready_file = READY_FILE
        )
    };

    body.into_bytes()
}

/// Where a deployed test service signals readiness.
const READY_FILE: &str = "/run/nudo-e2e-ready";

/// The health check that reads it. A command check, which is one of the three
/// kinds the product supports.
fn ready_check() -> HealthCheck {
    HealthCheck {
        kind: Some(health_check::Kind::Command(format!("test -s {READY_FILE}"))),
        timeout_seconds: 5,
        retries: 6,
        initial_delay_seconds: 2,
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_binary_deploys_and_the_unit_becomes_active() {
    let fixture = Fixture::start().expect("start the target container");
    let secret_key = SecretKey::generate();
    let (engine, _dir) = engine(secret_key.clone()).await;

    // ---- register the target, with its key in the secret store ----
    let key_secret = engine
        .store
        .put_secret(&secret_key, "E2E_SSH_KEY", &fixture.private_key, "", "")
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
    let (ok, checks) = nudo_server::probe::check_target(&fixture.ssh_target(), "/opt").await;
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
        .put_secret(&secret_key, "E2E_SSH_KEY", &fixture.private_key, "", "")
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
        .put_secret(&secret_key, "E2E_SSH_KEY", &fixture.private_key, "", "")
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
        .put_secret(&secret_key, "APP_TOKEN", hostile_value, "", "")
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
        b"#!/bin/bash\nprintenv APP_TOKEN > /tmp/seen-token\nsleep 30\n".to_vec(),
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

/// Polls an async condition until it holds or the deadline passes.
async fn wait_for_async<F, Fut>(timeout: Duration, mut ready: F) -> anyhow::Result<()>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    let deadline = tokio::time::Instant::now() + timeout;
    while tokio::time::Instant::now() < deadline {
        if ready().await {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    anyhow::bail!("timed out after {timeout:?}")
}
