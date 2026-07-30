//! The container the tests deploy into, and the helpers they all share.
//!
//! Every test in this binary starts its own [`Fixture`]: one container, torn
//! down when it is dropped. The container name and the mapped SSH port are
//! fixed, which is why the suite must run single-threaded.

use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

use nudo_proto::{HealthCheck, deployment, health_check};
use nudo_server::crypto::SecretKey;
use nudo_server::deploy::Engine;
use nudo_server::events::Bus;
use nudo_server::ssh::SshTarget;
use nudo_server::store::Store;

/// The container the test deploys into.
pub(crate) const CONTAINER: &str = "nudo-e2e-target";

/// The SSH port mapped onto the host.
pub(crate) const SSH_PORT: u16 = 22022;

/// A systemd-enabled image. Debian with systemd as PID 1 is the closest thing to
/// the hosts this tool actually targets.
pub(crate) const IMAGE: &str = "debian:bookworm";

/// A target under test, torn down when dropped.
pub(crate) struct Fixture {
    pub(crate) private_key: String,
}

impl Fixture {
    /// Starts the container, installs sshd and a key, and waits for both.
    pub(crate) fn start() -> anyhow::Result<Self> {
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
            // `systemctl is-system-running` exits *non-zero* for every state
            // that is not "running" — including "degraded", which is the normal
            // state in a container where some units cannot start. `run` turns a
            // non-zero exit into an `Err`, so reading this through
            // `exec_in_container` discarded the answer and spun until the
            // timeout. The state is on stdout either way, so ask for it in a way
            // that always exits zero and read the word.
            //
            // The earlier `--wait` form did not have this problem only because
            // it blocked until "running"; that made each poll hang for most of
            // the budget while apt was still installing, which is the flake this
            // loop replaced.
            let state = exec_in_container(&[
                "bash",
                "-c",
                "systemctl is-system-running 2>/dev/null || true",
            ])
            .unwrap_or_default();

            let state = state.trim();
            state == "running" || state == "degraded"
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

    pub(crate) fn stop_quietly() {
        let _ = Command::new("docker")
            .args(["rm", "-f", CONTAINER])
            .output();
    }

    /// SSH details with nothing pinned, so a connection trusts on first use.
    pub(crate) fn ssh_target(&self) -> SshTarget {
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
    pub(crate) fn host_key(&self) -> anyhow::Result<String> {
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
    pub(crate) fn seed_repository(
        &self,
        owner: &str,
        name: &str,
        version: &str,
    ) -> anyhow::Result<()> {
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
    pub(crate) fn commit_to_repository(
        &self,
        name: &str,
        version: &str,
        healthy: bool,
    ) -> anyhow::Result<()> {
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
    pub(crate) fn path_exists(&self, path: &str) -> bool {
        exec_in_container(&["test", "-e", path]).is_ok()
    }

    /// Replaces the container's host key with a freshly generated one and
    /// restarts sshd, as rebuilding the machine would.
    pub(crate) fn regenerate_host_key(&self) -> anyhow::Result<()> {
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
pub(crate) fn run(program: &str, args: &[&str]) -> anyhow::Result<String> {
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
pub(crate) fn exec_in_container(args: &[&str]) -> anyhow::Result<String> {
    let mut full = vec!["exec", CONTAINER];
    full.extend_from_slice(args);
    run("docker", &full)
}

/// Polls until a condition holds or the deadline passes.
pub(crate) fn wait_for(
    what: &str,
    timeout: Duration,
    mut ready: impl FnMut() -> bool,
) -> anyhow::Result<()> {
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
pub(crate) fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', r"'\''"))
}

/// Builds an engine wired to a temporary database.
pub(crate) async fn engine(secret_key: SecretKey) -> (Engine, tempfile::TempDir) {
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
pub(crate) async fn await_deployment(engine: &Engine, deployment_id: &str) -> deployment::Status {
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
pub(crate) async fn dump_logs(engine: &Engine, deployment_id: &str, label: &str) {
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
pub(crate) fn make_artifact(version: &str, healthy: bool) -> Vec<u8> {
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
pub(crate) const READY_FILE: &str = "/run/nudo-e2e-ready";

/// The health check that reads it. A command check, which is one of the three
/// kinds the product supports.
pub(crate) fn ready_check() -> HealthCheck {
    HealthCheck {
        kind: Some(health_check::Kind::Command(format!("test -s {READY_FILE}"))),
        timeout_seconds: 5,
        retries: 6,
        initial_delay_seconds: 2,
    }
}

/// Polls an async condition until it holds or the deadline passes.
pub(crate) async fn wait_for_async<F, Fut>(timeout: Duration, mut ready: F) -> anyhow::Result<()>
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
