//! The end-to-end self-upgrade test: a real binary upgrades itself, and the
//! version is checked right after.
//!
//! Behind the `self-upgrade-test` feature, which builds `nudo-all-in-one`
//! with two seams: `current_version()` honours a `.version-override` file
//! beside the executable (so the one binary this test builds can play both
//! the old and the new version), and the managed-layout eligibility check
//! stops requiring x86_64 Linux (so this runs on developer machines).
//!
//!     cargo test -p nudo-allinone --features self-upgrade-test --test self_upgrade
//!
//! No Docker and no systemd: the swap→exec→confirm cycle is pure process
//! mechanics. What systemd adds in production — restarting a crashed process
//! and running the boot guard — is covered by the guard's own tests in
//! `crates/bootguard`.
#![cfg(feature = "self-upgrade-test")]

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use nudo_proto::self_upgrade_client::SelfUpgradeClient;
use nudo_proto::{SelfUpgradeStatus, StartSelfUpgradeRequest};

const OLD: &str = "0.0.1";
const NEW: &str = "99.0.0";

/// One self-contained upgrade world: layout, fixture release, running child.
struct Harness {
    root: tempfile::TempDir,
    self_dir: PathBuf,
    grpc_port: u16,
    child: Child,
    _artifact_server: ArtifactServer,
}

impl Harness {
    /// `new_release_binary` is what the fixture tarball ships as the main
    /// binary — the real one for the happy path, garbage for the exec-failure
    /// path.
    async fn start(new_release_binary: &[u8]) -> Harness {
        let root = tempfile::tempdir().expect("tempdir");
        // Canonicalised because the eligibility check compares
        // current_exe() — which the OS canonicalises — against this path;
        // macOS tempdirs live behind a /var -> /private/var symlink.
        let canonical = root.path().canonicalize().expect("canonicalize");
        let self_dir = canonical.join("self");

        // The "installed" release the child starts from.
        let binary = std::fs::read(env!("CARGO_BIN_EXE_nudo-all-in-one")).expect("read binary");
        let old_dir = self_dir.join("releases").join(OLD);
        std::fs::create_dir_all(&old_dir).expect("mkdir");
        write_executable(&old_dir.join("nudo-all-in-one"), &binary);
        std::fs::write(old_dir.join(".version-override"), OLD).expect("override");
        nudo_bootguard::swap_current(&self_dir, &format!("releases/{OLD}")).expect("link");

        // The fixture release, packaged the way the release workflow does.
        let tarball = build_tarball(new_release_binary);
        let digest = {
            use sha2::Digest as _;
            hex::encode(sha2::Sha256::digest(&tarball))
        };
        let artifact_server = ArtifactServer::start(tarball);

        // The recorded manifest is what start() consults, so seeding the
        // store stands in for the update check having run.
        let database = canonical.join("nudo.db");
        {
            let store = nudo_server::store::Store::open(&database)
                .await
                .expect("store");
            store.set_self_upgrade_enabled(true).await.expect("toggle");
            let manifest = format!(
                r#"{{"releases":[{{"version":"{NEW}","artifacts":{{"nudo-v{NEW}-x86_64-unknown-linux-musl.tar.gz":{{"sha256":"{digest}"}}}}}}]}}"#,
            );
            store
                .record_latest_version(NEW, &manifest)
                .await
                .expect("record");
        }

        let grpc_port = free_port();
        let web_port = free_port();
        let child = Command::new(self_dir.join("current").join("nudo-all-in-one"))
            .env("NUDO_SELF_DIR", &self_dir)
            .env(
                "NUDO_SELF_UPGRADE_DOWNLOAD_BASE",
                format!("http://127.0.0.1:{}", artifact_server.port),
            )
            .env("NUDO_DB", &database)
            .env("NUDO_DATA_DIR", canonical.join("data"))
            .env("NUDO_SECRET_KEY", "aa".repeat(32))
            .env("NUDO_GRPC_ADDR", format!("127.0.0.1:{grpc_port}"))
            .env("NUDO_WEB_ADDR", format!("127.0.0.1:{web_port}"))
            // The real manifest URL must never be fetched from a test.
            .env("NUDO_CHECK_FOR_UPDATES", "false")
            .stdout(Stdio::from(
                std::fs::File::create(canonical.join("child.log")).expect("log"),
            ))
            .stderr(Stdio::from(
                std::fs::File::create(canonical.join("child.err")).expect("log"),
            ))
            .spawn()
            .expect("spawn nudo-all-in-one");

        Harness {
            root,
            self_dir,
            grpc_port,
            child,
            _artifact_server: artifact_server,
        }
    }

    async fn client(&self) -> Option<SelfUpgradeClient<tonic::transport::Channel>> {
        SelfUpgradeClient::connect(format!("http://127.0.0.1:{}", self.grpc_port))
            .await
            .ok()
    }

    /// Polls GetStatus until `done` says so, riding through the exec window
    /// where the endpoint is briefly unreachable.
    async fn wait_for_status(
        &self,
        what: &str,
        done: impl Fn(&SelfUpgradeStatus) -> bool,
    ) -> SelfUpgradeStatus {
        let deadline = Instant::now() + Duration::from_secs(120);
        loop {
            if let Some(mut client) = self.client().await
                && let Ok(response) = client.get_status(()).await
            {
                let status = response.into_inner();
                if done(&status) {
                    return status;
                }
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for {what}; child log:\n{}",
                self.debug_logs()
            );
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
    }

    fn debug_logs(&self) -> String {
        let canonical = self.root.path().canonicalize().unwrap_or_default();
        let read = |name: &str| std::fs::read_to_string(canonical.join(name)).unwrap_or_default();
        format!(
            "--- stdout ---\n{}\n--- stderr ---\n{}",
            read("child.log"),
            read("child.err")
        )
    }

    fn journal(&self) -> nudo_bootguard::Journal {
        nudo_bootguard::Journal::load(&self.self_dir)
            .expect("journal readable")
            .expect("journal present")
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn write_executable(path: &Path, bytes: &[u8]) {
    std::fs::write(path, bytes).expect("write binary");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    }
}

/// Packages a fixture release tarball with the workflow's layout and name.
fn build_tarball(main_binary: &[u8]) -> Vec<u8> {
    let top = format!("nudo-v{NEW}-x86_64-unknown-linux-musl");
    let encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
    let mut builder = tar::Builder::new(encoder);
    for (name, contents) in [
        ("nudo-all-in-one", main_binary),
        (".version-override", NEW.as_bytes()),
        ("README.md", b"fixture".as_slice()),
    ] {
        let mut header = tar::Header::new_gnu();
        header.set_size(contents.len() as u64);
        header.set_mode(if name == "nudo-all-in-one" {
            0o755
        } else {
            0o644
        });
        header.set_cksum();
        builder
            .append_data(&mut header, format!("{top}/{name}"), contents)
            .expect("append");
    }
    builder
        .into_inner()
        .expect("finish tar")
        .finish()
        .expect("finish gzip")
}

/// A one-shot loopback HTTP server for the artifact, in the spirit of the
/// CLI's deploy upload: a std thread, because the child's download must not
/// depend on this test's tokio runtime staying unblocked.
struct ArtifactServer {
    port: u16,
}

impl ArtifactServer {
    fn start(tarball: Vec<u8>) -> ArtifactServer {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().expect("addr").port();
        std::thread::spawn(move || {
            // Serves every request the same artifact; the path has already
            // been constructed and verified by the engine under test.
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                let tarball = tarball.clone();
                std::thread::spawn(move || {
                    use std::io::Read as _;
                    // Read (and discard) the request head.
                    let mut buffer = [0u8; 4096];
                    let _ = stream.read(&mut buffer);
                    let _ = write!(
                        stream,
                        "HTTP/1.1 200 OK\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                        tarball.len()
                    );
                    let _ = stream.write_all(&tarball);
                });
            }
        });
        ArtifactServer { port }
    }
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .expect("bind")
        .local_addr()
        .expect("addr")
        .port()
}

#[tokio::test]
async fn an_instance_upgrades_itself_and_comes_back_as_the_new_version() {
    // The version override file is part of the fixture release, so the same
    // binary reports NEW once it runs from the new release directory.
    let binary = std::fs::read(env!("CARGO_BIN_EXE_nudo-all-in-one")).expect("read binary");
    let harness = Harness::start(&binary).await;

    // The gates the dashboard would show: all open.
    let status = harness
        .wait_for_status("the instance to come up", |status| status.state == "idle")
        .await;
    assert!(status.enabled_in_settings, "{}", harness.debug_logs());
    assert!(status.eligible, "the child must detect the managed layout");

    let pid_before_upgrade = harness.child.id();

    harness
        .client()
        .await
        .expect("client")
        .start(StartSelfUpgradeRequest {
            target_version: NEW.to_string(),
            mutation: None,
        })
        .await
        .expect("start accepted");

    // Download → verify → stage → swap → exec → boot → confirm. The poll
    // rides through the restart exactly the way the dashboard does.
    let status = harness
        .wait_for_status("the upgrade to confirm", |status| {
            status.state == "confirmed"
        })
        .await;
    assert_eq!(status.from_version, OLD);
    assert_eq!(status.to_version, NEW);

    // The version, checked right after — three ways.
    // 1. The journal only reaches `confirmed` when the process that booted
    //    reports exactly the promised version.
    assert_eq!(
        harness.journal().state,
        nudo_bootguard::JournalState::Confirmed
    );
    // 2. The process is the same one — exec, not a respawn: nothing exited.
    let mut harness = harness;
    assert!(
        harness.child.try_wait().expect("probe").is_none(),
        "the original process must still be running (exec replaces in place)"
    );
    assert_eq!(harness.child.id(), pid_before_upgrade);
    // 3. `--version` of what `current` points at.
    let output = Command::new(harness.self_dir.join("current").join("nudo-all-in-one"))
        .arg("--version")
        .output()
        .expect("--version");
    let version = String::from_utf8_lossy(&output.stdout);
    assert!(
        version.contains(NEW),
        "`current/nudo-all-in-one --version` reports {version:?}, expected {NEW}"
    );

    // The safety net exists: the pre-swap database snapshot is in the new
    // release directory.
    assert!(
        harness
            .self_dir
            .join("releases")
            .join(NEW)
            .join("db-pre-upgrade.sqlite")
            .exists(),
        "the database snapshot is missing"
    );
}

#[tokio::test]
async fn a_release_that_cannot_exec_is_rolled_back_with_the_old_version_still_serving() {
    // The tarball verifies and stages fine; the "binary" inside is garbage,
    // so exec fails — the worst failure mode, and the one that must cost
    // nothing: the old process never stopped running.
    let harness = Harness::start(b"#!not a real executable\n").await;

    harness
        .wait_for_status("the instance to come up", |status| status.state == "idle")
        .await;

    harness
        .client()
        .await
        .expect("client")
        .start(StartSelfUpgradeRequest {
            target_version: NEW.to_string(),
            mutation: None,
        })
        .await
        .expect("start accepted");

    let status = harness
        .wait_for_status("the exec failure to be recorded", |status| {
            status.state == "exec-failed"
        })
        .await;
    assert!(status.error.contains("exec"), "{}", status.error);

    // The old process is still the one answering — that is the zero-downtime
    // property, proven by the RPC above having answered at all — and the
    // symlink points back at the old release.
    assert_eq!(
        nudo_bootguard::current_target(&harness.self_dir),
        Some(PathBuf::from(format!("releases/{OLD}"))),
        "current was not swapped back after the failed exec"
    );
}
