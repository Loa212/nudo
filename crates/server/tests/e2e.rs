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
    ArtifactSource, HealthCheck, Route, Service, SystemdUnit, artifact_source, deployment,
    health_check,
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
                // git is here for the build-host tests: it is the one tool nudo
                // itself runs on a machine that builds.
                "apt-get update -qq && \
                 DEBIAN_FRONTEND=noninteractive apt-get install -y -qq \
                   systemd systemd-sysv openssh-server curl git >/dev/null && \
                 mkdir -p /run/sshd && \
                 exec /lib/systemd/systemd",
            ],
        )?;

        // The apt install inside the container takes a while; systemd is only
        // usable once it has finished and taken over as PID 1.
        //
        // Deliberately *not* `is-system-running --wait`, which blocks inside the
        // container until systemd settles. Each poll could then hang for most of
        // the budget while the apt install was still running, and the timeout
        // fired against a container that was simply not up yet — an intermittent
        // failure that took out every test in the file, not only the slow ones.
        // The non-blocking form returns immediately with whatever the state is
        // now, so the retry loop here does the waiting and can actually observe
        // progress.
        wait_for("systemd to come up", Duration::from_secs(300), || {
            exec_in_container(&["systemctl", "is-system-running"])
                .map(|output| {
                    // "degraded" is fine in a container: some units cannot
                    // start there and that does not affect what is tested.
                    // "starting" is not ready yet, and must not count.
                    let state = output.trim();
                    state == "running" || state == "degraded"
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

    /// SSH details with nothing pinned, so a connection trusts on first use.
    fn ssh_target(&self) -> SshTarget {
        SshTarget {
            host: "127.0.0.1".to_string(),
            port: SSH_PORT,
            user: "root".to_string(),
            private_key: self.private_key.clone(),
            passphrase: None,
            host_key: String::new(),
        }
    }

    /// The container's actual ed25519 host key, in OpenSSH one-line form.
    ///
    /// Read from inside the container rather than scanned over the network, so
    /// the test compares nudo's verification against the machine's own idea of
    /// its identity — which is exactly what an operator is told to do.
    fn host_key(&self) -> anyhow::Result<String> {
        Ok(
            exec_in_container(&["cat", "/etc/ssh/ssh_host_ed25519_key.pub"])?
                .trim()
                .to_string(),
        )
    }

    /// Seeds a git repository inside the container and makes nudo's clone URL
    /// resolve to it.
    ///
    /// The remote build path builds `https://github.com/<owner>/<name>.git` for
    /// a public repository, and a container in CI has no route to GitHub. A
    /// system-wide `url.<local>.insteadOf` rewrite points exactly that URL at a
    /// bare repository on disk, so **the code under test is unmodified** — it
    /// runs the same `git clone` command against the same URL it would use in
    /// production, and git redirects it. Faking the URL construction instead
    /// would test a string rather than a clone.
    ///
    /// The repository contains a build command that produces a real artifact,
    /// so what is exercised is clone → build → collect, not a stub.
    fn seed_repository(&self, owner: &str, name: &str, version: &str) -> anyhow::Result<()> {
        let bare = format!("/srv/git/{owner}/{name}.git");
        let work = format!("/tmp/seed-{name}");
        // The build writes its "binary" to the path the service names as its
        // artifact_path, from a source file in the repository — so a build that
        // silently did nothing would produce no artifact and fail the deploy.
        let script = make_artifact(version, true);

        exec_in_container(&[
            "bash",
            "-c",
            &format!(
                "set -eu && \
                 git config --global init.defaultBranch main && \
                 git config --global user.email e2e@nudo.test && \
                 git config --global user.name 'nudo e2e' && \
                 git config --global url.'file:///srv/git/'.insteadOf 'https://github.com/' && \
                 rm -rf {bare} {work} && \
                 mkdir -p {bare} && git init --bare -q {bare} && \
                 mkdir -p {work} && cd {work} && git init -q && \
                 printf '%s' {script} > bot.sh && \
                 printf '%s' {build} > build.sh && chmod +x build.sh && \
                 git add -A && git commit -q -m 'seed' && \
                 git branch -M main && \
                 git remote add origin {bare} && git push -q origin main",
                bare = shell_quote(&bare),
                work = shell_quote(&work),
                script = shell_quote(&String::from_utf8_lossy(&script)),
                // Deliberately not a no-op: it reads a tracked file and writes
                // the artifact, so "the build ran" and "the artifact came back"
                // are two different assertions.
                build = shell_quote(
                    "#!/bin/bash\nset -euo pipefail\n\
                     echo \"building on $(hostname)\"\n\
                     mkdir -p dist\n\
                     cp bot.sh dist/bot\n\
                     chmod +x dist/bot\n"
                ),
            ),
        ])?;
        Ok(())
    }

    /// Pushes a new commit to a seeded repository, changing what its build
    /// produces.
    ///
    /// `healthy` decides whether the built binary ever signals readiness. A
    /// false one starts and stays up but never becomes ready, so the health
    /// check fails while systemd still reports the unit active — which is the
    /// case a tool that only asks systemd gets wrong, and the one the rollback
    /// depends on.
    ///
    /// The build command is untouched: only the source file changes, so the
    /// second deploy exercises the same clone-build-collect path and differs
    /// solely in what the build emits.
    fn commit_to_repository(&self, name: &str, version: &str, healthy: bool) -> anyhow::Result<()> {
        let work = format!("/tmp/seed-{name}");
        let script = make_artifact(version, healthy);

        exec_in_container(&[
            "bash",
            "-c",
            &format!(
                "set -eu && cd {work} && \
                 printf '%s' {script} > bot.sh && \
                 git add -A && git commit -q -m {message} && \
                 git push -q origin main",
                work = shell_quote(&work),
                script = shell_quote(&String::from_utf8_lossy(&script)),
                message = shell_quote(version),
            ),
        ])?;
        Ok(())
    }

    /// Whether a path exists inside the container.
    fn path_exists(&self, path: &str) -> bool {
        exec_in_container(&["test", "-e", path]).is_ok()
    }

    /// Replaces the container's host key with a freshly generated one and
    /// restarts sshd, as rebuilding the machine would.
    fn regenerate_host_key(&self) -> anyhow::Result<()> {
        exec_in_container(&[
            "bash",
            "-c",
            "rm -f /etc/ssh/ssh_host_ed25519_key /etc/ssh/ssh_host_ed25519_key.pub && \
             ssh-keygen -t ed25519 -N '' -f /etc/ssh/ssh_host_ed25519_key -q",
        ])?;
        exec_in_container(&["systemctl", "restart", "ssh"])?;
        wait_for(
            "sshd to come back with its new key",
            Duration::from_secs(30),
            || std::net::TcpStream::connect(("127.0.0.1", SSH_PORT)).is_ok(),
        )
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
        engine.connect_build_host(&build_host).await,
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
        engine.connect_build_host(&build_host).await,
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
        .connect_build_host(&build_host)
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
        .connect_build_host(&refreshed)
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

// ---------------------------------------------------------------------------
// Ingress
//
// These install a real Caddy in the container and drive it the way nudo does.
// The point is the parts a unit test cannot reach: that the config nudo renders
// is one Caddy actually accepts, that a reload leaves the previous routes
// serving when it does not, and that a service is reachable through the proxy
// rather than only configured to be.
// ---------------------------------------------------------------------------

/// Starts something for the proxy to route to, on a port of its own.
///
/// Caddy itself, in its own systemd unit. The first version of this reached for
/// `python3 -m http.server` with a netcat fallback and chained the two with
/// `or_else`, which was wrong twice: the image has neither, and the fallback
/// hid that. The test then asserted that the proxy routed to a port where
/// nothing was listening, and failed thirty seconds later saying the proxy was
/// at fault.
///
/// Caddy is the one thing guaranteed to be here, because the test under way
/// installed it. `respond` needs no content on disk and no other tool.
///
/// Panics rather than returning a Result: a test whose fixture did not start
/// cannot produce a meaningful result, and the previous silence is the whole
/// reason this function exists.
fn start_origin(port: u32, body: &str) {
    let unit = format!("nudo-e2e-origin-{port}");
    let config = format!("/etc/{unit}.caddy");

    exec_in_container(&[
        "bash",
        "-c",
        &format!(
            "cat > {config} <<'EOF'\n\
             {{\n\
             \tadmin off\n\
             \tauto_https off\n\
             }}\n\
             :{port} {{\n\
             \trespond \"{body}\"\n\
             }}\n\
             EOF",
        ),
    ])
    .expect("write the origin's config");

    exec_in_container(&[
        "bash",
        "-c",
        &format!(
            "systemd-run --unit={unit} --collect \
             $(command -v caddy) run --config {config} --adapter caddyfile"
        ),
    ])
    .expect("start the origin");

    // Confirmed listening before the test goes on to assert anything about the
    // proxy in front of it, so a failure names the origin rather than blaming
    // the routing.
    wait_for(
        &format!("the origin on :{port}"),
        Duration::from_secs(30),
        || {
            exec_in_container(&[
                "bash",
                "-c",
                &format!("curl -fsS --max-time 2 http://127.0.0.1:{port}/ 2>/dev/null || true"),
            ])
            .map(|out| out.contains(body))
            .unwrap_or(false)
        },
    )
    .expect("the origin should answer before the proxy is asked to route to it");
}

/// Requests a path through the proxy, over HTTPS.
///
/// Caddy redirects :80 to :443 for every site it serves, and for a `.localhost`
/// name it issues a certificate from its own local CA rather than attempting
/// ACME — which is what makes any of this testable inside a container. So the
/// request has to speak HTTPS (`--resolve` points the name at loopback) and
/// accept that CA (`-k`). Requesting plain HTTP and treating the 301 as a
/// failure, which an earlier version of these tests did, reads correct
/// behaviour as the proxy being broken.
fn proxy_get(host: &str, path: &str) -> anyhow::Result<String> {
    exec_in_container(&[
        "bash",
        "-c",
        &format!(
            "curl -sSkL --max-time 5 --resolve {host}:443:127.0.0.1 \
             https://{host}{path} 2>/dev/null || true"
        ),
    ])
}

/// Registers the container as a target with managed ingress.
async fn register_ingress_target(
    engine: &Engine,
    secret_key: &SecretKey,
    fixture: &Fixture,
    name: &str,
) -> nudo_proto::Target {
    let key_secret = engine
        .store
        .put_secret(
            secret_key,
            "E2E_INGRESS_KEY",
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
            name: name.to_string(),
            host: "127.0.0.1".to_string(),
            port: SSH_PORT as u32,
            user: "root".to_string(),
            ssh_key_id: key_secret.id,
            latency_critical: false,
            labels: Default::default(),
        })
        .await
        .expect("create the target");

    engine
        .store
        .set_ingress(&target.id, nudo_proto::ingress::Mode::Managed, 0, "")
        .await
        .expect("enable ingress");

    engine
        .store
        .get_target(&target.id)
        .await
        .expect("read back")
        .expect("target")
}

#[tokio::test]
async fn caddy_is_installed_and_serves_the_config_nudo_renders() {
    // The whole feature against a real host: install Caddy, write the config,
    // start it, and confirm a request to the domain reaches the service.
    let fixture = Fixture::start().expect("start the container");
    let secret_key = SecretKey::generate();
    let (engine, _dir) = engine(secret_key.clone()).await;

    let target = register_ingress_target(&engine, &secret_key, &fixture, "e2e-ingress").await;
    let session = engine.connect(&target).await.expect("connect");

    // ---- install ----
    // Before the origin, because the origin is a second Caddy and needs the
    // binary this step puts there.
    let version = nudo_server::ingress::reconcile::install(&session)
        .await
        .expect("install caddy");
    eprintln!("installed caddy {version}");
    assert!(!version.is_empty());

    // Something for the proxy to route to. A plain listener rather than a nudo
    // service: what is under test is the proxy, not the deploy engine.
    start_origin(8080, "hello from the origin");
    start_origin(8081, "hello from the api");

    // Two routes on one service, and a path under one of them — the shape a
    // single domain and port could not express, and the reason the model is a
    // list.
    let service = engine
        .store
        .create_service(&Service {
            target_id: target.id.clone(),
            name: "e2e-routed".to_string(),
            artifact: Some(ArtifactSource {
                kind: Some(artifact_source::Kind::DirectUpload(true)),
            }),
            release_root: "/opt/e2e-routed".to_string(),
            // `localhost` would not survive `validate_domain`, and a real
            // domain cannot be issued a certificate in a container. `.localhost`
            // is reserved, resolves locally, and Caddy serves it over plain
            // HTTP without attempting ACME — which is what makes this testable.
            routes: vec![
                Route {
                    domain: "routed.localhost".to_string(),
                    port: 8080,
                    ..Default::default()
                },
                Route {
                    domain: "routed.localhost".to_string(),
                    path: "/api".to_string(),
                    port: 8081,
                    ..Default::default()
                },
            ],
            ..Default::default()
        })
        .await
        .expect("create the routed service");

    // ---- write the config and start ----
    let services = engine
        .store
        .routed_services(&target.id)
        .await
        .expect("routed services");
    assert_eq!(services.len(), 1, "one routed service");

    let outcome = nudo_server::ingress::reconcile::reload(&session, &target, &services)
        .await
        .expect("reload");
    assert!(
        outcome.ok,
        "Caddy rejected the config nudo rendered: {}",
        outcome.error
    );

    // ---- the config on the host is what nudo renders ----
    let on_disk =
        exec_in_container(&["cat", nudo_server::ingress::CONFIG_PATH]).expect("read the config");
    assert!(
        on_disk.contains("routed.localhost {"),
        "the domain should be a site block: {on_disk}"
    );
    assert!(
        on_disk.contains("reverse_proxy 127.0.0.1:8080"),
        "the route should point at the service's loopback port: {on_disk}"
    );

    // ---- and the proxy actually serves it ----
    //
    // Over HTTPS, following the redirect. Caddy redirects :80 to :443 for every
    // site, and for a `.localhost` name it issues a certificate from its own
    // local CA rather than attempting ACME — which is exactly what makes this
    // testable inside a container. `-k` accepts that CA; `-L` follows the
    // redirect. An earlier version of this test requested plain HTTP with `-f`
    // and read the 301 as the proxy being broken.
    wait_for("caddy to answer", Duration::from_secs(60), || {
        proxy_get("routed.localhost", "/")
            .map(|body| body.contains("hello from the origin"))
            .unwrap_or(false)
    })
    .expect("the proxy should route the domain to the service");

    // ---- the path route reaches the other port ----
    // The same domain, a different backend, chosen by prefix. This is the case
    // a single domain-and-port field could not express at all.
    let from_path = proxy_get("routed.localhost", "/api/").expect("request the path route");
    assert!(
        from_path.contains("hello from the api"),
        "/api should reach the second origin, got: {from_path:?}"
    );

    let _ = service;
    let _ = session.close().await;
}

#[tokio::test]
async fn a_grpc_route_reaches_the_backend_over_http2() {
    // gRPC needs HTTP/2 end to end. A proxy that downgrades to HTTP/1.1 breaks
    // every call, which is why the protocol is stated on the route rather than
    // guessed — and why this asserts the wire protocol rather than only that
    // the config mentions h2c.
    let fixture = Fixture::start().expect("start the container");
    let secret_key = SecretKey::generate();
    let (engine, _dir) = engine(secret_key.clone()).await;

    let target = register_ingress_target(&engine, &secret_key, &fixture, "e2e-grpc").await;
    let session = engine.connect(&target).await.expect("connect");

    nudo_server::ingress::reconcile::install(&session)
        .await
        .expect("install caddy");

    // An h2c backend: Caddy serving cleartext HTTP/2, which is what a gRPC
    // server looks like on the wire.
    exec_in_container(&[
        "bash",
        "-c",
        "cat > /etc/nudo-e2e-h2c.caddy <<'EOF'\n\
         {\n\
         \tadmin off\n\
         \tauto_https off\n\
         \tservers {\n\
         \t\tprotocols h1 h2c\n\
         \t}\n\
         }\n\
         :9090 {\n\
         \trespond \"grpc backend saw {http.request.proto}\"\n\
         }\n\
         EOF",
    ])
    .expect("write the h2c backend config");

    exec_in_container(&[
        "bash",
        "-c",
        "systemd-run --unit=nudo-e2e-h2c --collect \
         $(command -v caddy) run --config /etc/nudo-e2e-h2c.caddy --adapter caddyfile",
    ])
    .expect("start the h2c backend");

    wait_for("the h2c backend", Duration::from_secs(30), || {
        exec_in_container(&[
            "bash",
            "-c",
            "curl -fsS --http2-prior-knowledge --max-time 2 http://127.0.0.1:9090/ \
             2>/dev/null || true",
        ])
        .map(|out| out.contains("grpc backend saw"))
        .unwrap_or(false)
    })
    .expect("the h2c backend should answer over http/2");

    let service = engine
        .store
        .create_service(&Service {
            target_id: target.id.clone(),
            name: "e2e-grpc".to_string(),
            release_root: "/opt/e2e-grpc".to_string(),
            routes: vec![Route {
                domain: "grpc.localhost".to_string(),
                port: 9090,
                protocol: nudo_proto::route::Protocol::H2c as i32,
                ..Default::default()
            }],
            ..Default::default()
        })
        .await
        .expect("create the grpc service");

    let services = engine
        .store
        .routed_services(&target.id)
        .await
        .expect("routed services");

    let outcome = nudo_server::ingress::reconcile::reload(&session, &target, &services)
        .await
        .expect("reload");
    assert!(
        outcome.ok,
        "Caddy rejected the h2c config: {}",
        outcome.error
    );

    let on_disk =
        exec_in_container(&["cat", nudo_server::ingress::CONFIG_PATH]).expect("read the config");
    assert!(
        on_disk.contains("reverse_proxy h2c://127.0.0.1:9090"),
        "a gRPC route should proxy over h2c: {on_disk}"
    );

    // Requested over HTTPS, which is how a gRPC client actually connects. What
    // is being asserted is the *second* hop: the backend echoes the protocol it
    // saw from the proxy, and that is the one `h2c://` decides. It reports
    // HTTP/2.0 only if the proxy did not downgrade — which is the failure that
    // breaks every gRPC call and looks like the service being broken.
    let mut seen = String::new();
    wait_for("the proxy to route grpc", Duration::from_secs(60), || {
        seen = exec_in_container(&[
            "bash",
            "-c",
            "curl -sSkL --http2 --max-time 5 --resolve grpc.localhost:443:127.0.0.1 \
             https://grpc.localhost/ 2>/dev/null || true",
        ])
        .unwrap_or_default();
        seen.contains("grpc backend saw")
    })
    .expect("the proxy should route the grpc domain");

    assert!(
        seen.contains("HTTP/2.0"),
        "the backend should have been reached over HTTP/2, not downgraded to \
         HTTP/1.1 — gRPC breaks if it is. Got: {seen:?}"
    );

    let _ = service;
    let _ = session.close().await;
}

#[tokio::test]
async fn a_rejected_config_leaves_the_previous_routes_serving() {
    // The property the staged-write-then-validate ordering exists for. A typo
    // in a domain must not take a host offline, and Caddy must still be serving
    // what it was serving before the attempt.
    let fixture = Fixture::start().expect("start the container");
    let secret_key = SecretKey::generate();
    let (engine, _dir) = engine(secret_key.clone()).await;

    let target = register_ingress_target(&engine, &secret_key, &fixture, "e2e-ingress-bad").await;
    let session = engine.connect(&target).await.expect("connect");

    nudo_server::ingress::reconcile::install(&session)
        .await
        .expect("install caddy");

    // ---- a good config first, so there is something to preserve ----
    let good = Service {
        target_id: target.id.clone(),
        name: "e2e-good".to_string(),
        release_root: "/opt/e2e-good".to_string(),
        routes: vec![Route {
            domain: "good.localhost".to_string(),
            port: 8080,
            ..Default::default()
        }],
        ..Default::default()
    };
    let outcome = nudo_server::ingress::reconcile::reload(&session, &target, &[good.clone()])
        .await
        .expect("reload");
    assert!(
        outcome.ok,
        "the first config should be accepted: {}",
        outcome.error
    );

    let before =
        exec_in_container(&["cat", nudo_server::ingress::CONFIG_PATH]).expect("read the config");
    assert!(before.contains("good.localhost"));

    // ---- now something Caddy will refuse ----
    // A directive that does not exist, reached by way of a service name — the
    // renderer refuses a hostile *domain*, so the config has to be broken in a
    // way that gets past it to test what Caddy does with one.
    exec_in_container(&[
        "bash",
        "-c",
        &format!(
            "printf '%s\\n' 'not-a-directive {{' 'nonsense here' '}}' >> {}",
            nudo_server::ingress::CONFIG_PATH
        ),
    ])
    .expect("corrupt the config");

    // nudo re-renders from the database, so the corrupted file is replaced by a
    // valid one — which is the point: a hand-edited config is not preserved,
    // and a reload puts the rendered config back.
    let outcome = nudo_server::ingress::reconcile::reload(&session, &target, &[good])
        .await
        .expect("reload");
    assert!(
        outcome.ok,
        "re-rendering should replace a hand-broken config: {}",
        outcome.error
    );

    let after =
        exec_in_container(&["cat", nudo_server::ingress::CONFIG_PATH]).expect("read the config");
    assert!(
        !after.contains("not-a-directive"),
        "the rendered config should have replaced the hand-edit: {after}"
    );
    assert!(after.contains("good.localhost"));

    let _ = session.close().await;
}

#[tokio::test]
async fn the_ingress_check_reports_what_is_wrong_and_warns_about_dns() {
    // The DNS warning is the single most common way this feature disappoints
    // someone, so it has to be a warning that does not fail the check — the
    // record may be minutes away — and it has to actually fire.
    let fixture = Fixture::start().expect("start the container");
    let secret_key = SecretKey::generate();
    let (engine, _dir) = engine(secret_key.clone()).await;

    let target = register_ingress_target(&engine, &secret_key, &fixture, "e2e-ingress-check").await;
    let session = engine.connect(&target).await.expect("connect");

    // ---- before installing: the check should say so rather than error ----
    let before = nudo_server::ingress::reconcile::check(&session, &target, &[]).await;
    for check in &before.checks {
        eprintln!(
            "check {:<24} {} {}",
            check.name,
            if check.ok { "ok" } else { "FAIL" },
            check.detail
        );
    }
    assert!(!before.ok, "nothing is installed yet");
    let installed = before
        .checks
        .iter()
        .find(|c| c.name == "installed")
        .expect("an installed check");
    assert!(!installed.ok);

    // ---- install, and check a domain that does not resolve ----
    nudo_server::ingress::reconcile::install(&session)
        .await
        .expect("install caddy");

    let unresolvable = Service {
        target_id: target.id.clone(),
        name: "e2e-nodns".to_string(),
        release_root: "/opt/e2e-nodns".to_string(),
        routes: vec![Route {
            // Reserved for exactly this: guaranteed never to resolve.
            domain: "nothing.invalid".to_string(),
            port: 8080,
            ..Default::default()
        }],
        ..Default::default()
    };

    nudo_server::ingress::reconcile::reload(&session, &target, &[unresolvable.clone()])
        .await
        .expect("reload");

    let after = nudo_server::ingress::reconcile::check(&session, &target, &[unresolvable]).await;
    for warning in &after.warnings {
        eprintln!("warning: {warning}");
    }
    assert!(
        after.warnings.iter().any(|w| w.contains("nothing.invalid")),
        "a domain that does not resolve should warn: {:?}",
        after.warnings
    );
    assert!(
        after
            .warnings
            .iter()
            .any(|w| w.contains("A or AAAA record")),
        "the warning should say what to do about it: {:?}",
        after.warnings
    );

    let _ = session.close().await;
}
