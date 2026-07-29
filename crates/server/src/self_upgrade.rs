//! Self-upgrade: download a verified release and become it.
//!
//! The sequence is the deploy engine's staged-swap idiom pointed at this
//! process: download, verify against the digest the release workflow committed
//! to the manifest, stage into a fresh release directory, snapshot the
//! database, swap the `current` symlink, and exec() the new binary through it.
//! exec never returns on success; if it fails — a truncated binary, the wrong
//! architecture — the old process is still running, swaps the symlink back,
//! and reports the failure with zero downtime.
//!
//! What happens after exec is bookkept in a journal file (`nudo-bootguard`):
//! the new process confirms itself on boot (`reconcile_on_boot`), and
//! `nudo-boot-guard` — run by systemd as `ExecStartPre=` — reverts the symlink
//! if a swapped release fails to confirm within a few starts.
//!
//! Security properties, deliberate and test-pinned elsewhere:
//! - The download URL is constructed here from the verified version, never
//!   taken from the manifest or the request.
//! - The digest comes from the manifest, which reaches this process by a
//!   different write path (a commit to the repository) than the artifact
//!   (an asset on a GitHub Release).
//! - The anti-rollback ladder applies: nothing at or below the running
//!   version is ever executed.
//! - Nothing is piped to a shell; the tarball is unpacked and exec'd by this
//!   code.
//! - Two switches must both be on: `--allow-self-upgrade` from whoever
//!   installs, and the dashboard toggle from whoever operates.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::{Context as _, bail};
use nudo_bootguard::{Journal, JournalState};

use crate::store::Store;
use crate::updates::{self, Manifest};

/// Where release artifacts are downloaded from, unless overridden.
pub const DEFAULT_DOWNLOAD_BASE: &str = "https://github.com/Loa212/nudo/releases/download";

/// A hard cap on bytes read from the network, counted as they arrive. The
/// content-length header is advisory; this is not.
const MAX_ARTIFACT_BYTES: u64 = 200 * 1024 * 1024;

/// The whole download must finish inside this.
const DOWNLOAD_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10 * 60);

/// How many self-releases to keep on disk, beyond whatever is live or named by
/// the journal. Mirrors the deploy engine's retention: enough to roll back,
/// not enough to fill the disk.
const KEEP_RELEASES: usize = 3;

/// The binaries a release tarball is expected to carry. Anything else in the
/// archive that is not a documentation file is skipped.
const BINARIES: &[&str] = &[
    "nudo-server",
    "nudo-web",
    "nudo",
    "nudo-mcp",
    "nudo-all-in-one",
    "nudo-boot-guard",
];

/// Non-binary files worth keeping from the tarball.
const DOCS: &[&str] = &[
    "README.md",
    "CHANGES.md",
    "CHANGELOG.md",
    "NOTICE",
    "LICENSE",
    "nudo.service",
];

/// The binary exec'd after the swap and named by the unit's `ExecStart`.
const MAIN_BINARY: &str = "nudo-all-in-one";

/// How this instance stands with respect to upgrading itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Eligibility {
    /// Running in a container; upgrading is `docker pull` and recreate.
    Container,
    /// A binary install predating the self-release layout. Self-upgrade needs
    /// the one-time migration the dashboard describes.
    BinaryLegacy,
    /// Running from `<self_dir>/releases/<v>` via the `current` symlink;
    /// self-upgrade can operate.
    Managed { self_dir: PathBuf },
}

/// Works out whether this process can upgrade itself.
///
/// `Managed` requires all of: not a container, a configured self directory,
/// the running executable actually living under `<self_dir>/releases/` (a
/// configured directory the process does not run from would make the swap a
/// no-op), and — outside the test feature — x86_64 Linux, because that is the
/// only target release artifacts exist for. An aarch64 operator gets a clear
/// refusal instead of a download that cannot run.
pub fn eligibility(config: &crate::Config) -> Eligibility {
    if updates::InstallKind::detect() == updates::InstallKind::Container {
        return Eligibility::Container;
    }
    let Some(self_dir) = &config.self_dir else {
        return Eligibility::BinaryLegacy;
    };

    #[cfg(not(feature = "self-upgrade-test"))]
    if !(cfg!(target_os = "linux") && cfg!(target_arch = "x86_64")) {
        return Eligibility::BinaryLegacy;
    }

    // Canonicalised on both sides: the executable path may still contain the
    // `current` symlink (macOS reports the path as invoked; Linux's
    // /proc/self/exe resolves it), and the configured directory may sit
    // behind symlinks of its own.
    let running_from_layout = std::env::current_exe()
        .and_then(|exe| exe.canonicalize())
        .ok()
        .zip(self_dir.join("releases").canonicalize().ok())
        .is_some_and(|(exe, releases)| exe.starts_with(&releases));
    if !running_from_layout {
        return Eligibility::BinaryLegacy;
    }

    Eligibility::Managed {
        self_dir: self_dir.clone(),
    }
}

/// What the dashboard needs to render the self-upgrade card.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StatusView {
    /// One of the journal states, a live phase (`downloading`, `verifying`,
    /// `staging`), or `idle`.
    pub state: String,
    pub from_version: String,
    pub to_version: String,
    pub error: String,
    /// RFC 3339, empty when nothing has happened yet.
    pub updated_at: String,
    pub allowed_by_config: bool,
    pub enabled_in_settings: bool,
    pub eligible: bool,
}

/// What an in-flight upgrade is doing right now. Kept in memory only: the
/// journal starts at `staged`, because everything before that point leaves
/// nothing behind that a restart would need to know about.
#[derive(Debug, Clone)]
struct LiveProgress {
    phase: &'static str,
    to_version: String,
}

/// The engine. One per process, shared between the gRPC service and boot
/// reconciliation.
#[derive(Clone)]
pub struct SelfUpgrader {
    store: Store,
    config: Arc<crate::Config>,
    live: Arc<Mutex<Option<LiveProgress>>>,
}

impl SelfUpgrader {
    pub fn new(store: Store, config: Arc<crate::Config>) -> Self {
        Self {
            store,
            config,
            live: Arc::new(Mutex::new(None)),
        }
    }

    /// The current status: gates, plus whichever of the live phase or the
    /// journal is most recent.
    pub async fn status(&self) -> StatusView {
        let eligibility = eligibility(&self.config);
        let mut view = StatusView {
            state: "idle".to_string(),
            allowed_by_config: self.config.allow_self_upgrade,
            enabled_in_settings: self.store.self_upgrade_enabled().await.unwrap_or(false),
            eligible: matches!(eligibility, Eligibility::Managed { .. }),
            ..StatusView::default()
        };

        if let Eligibility::Managed { self_dir } = &eligibility
            && let Ok(Some(journal)) = Journal::load(self_dir)
        {
            view.state = journal.state.as_str().to_string();
            view.from_version = journal.from_version;
            view.to_version = journal.to_version;
            view.error = journal.error;
            view.updated_at = chrono::DateTime::from_timestamp(journal.updated_at as i64, 0)
                .map(|dt| dt.to_rfc3339())
                .unwrap_or_default();
        }

        // A live phase outranks whatever the journal last recorded.
        if let Some(live) = self.live.lock().expect("live lock").clone() {
            view.state = live.phase.to_string();
            view.from_version = updates::current_version().to_string();
            view.to_version = live.to_version;
            view.error = String::new();
        }

        view
    }

    /// Starts an upgrade to `target_version` and returns once it is underway.
    ///
    /// The version is explicit rather than "latest" so the click authorises
    /// what the page showed, not whatever the manifest says by the time the
    /// task runs. The work happens in a spawned task because the caller is a
    /// gRPC handler whose response must reach the dashboard before exec()
    /// replaces the process serving it.
    pub async fn start(&self, target_version: &str) -> anyhow::Result<()> {
        if !self.config.allow_self_upgrade {
            bail!(
                "self-upgrade is not allowed by this instance's configuration (--allow-self-upgrade)"
            );
        }
        if !self.store.self_upgrade_enabled().await? {
            bail!("self-upgrade is switched off in the dashboard settings");
        }
        let Eligibility::Managed { self_dir } = eligibility(&self.config) else {
            bail!("this install cannot upgrade itself; see /upgrade for what applies here");
        };

        let running = updates::current_version();
        if !updates::is_newer(target_version, running) {
            bail!("refusing to \"upgrade\" from {running} to {target_version}: not newer");
        }

        // The recorded manifest, not a fresh fetch: what the operator saw and
        // clicked is what runs, and the update checker is the only thing that
        // talks to the manifest URL.
        let manifest: Manifest = self
            .store
            .recorded_manifest()
            .await?
            .and_then(|raw| serde_json::from_str(&raw).ok())
            .context("no recorded release manifest; wait for the update check to run")?;
        let release = manifest
            .releases
            .iter()
            .find(|release| release.version == target_version)
            .cloned()
            .with_context(|| format!("the manifest does not list {target_version}"))?;
        let latest = manifest
            .latest()
            .map(|r| r.version.clone())
            .unwrap_or_default();
        if release.version != latest {
            bail!(
                "{target_version} is not the latest release ({latest}); upgrades only go to the newest"
            );
        }

        let filename = artifact_filename(target_version);
        let expected_digest = release
            .artifacts
            .get(&filename)
            .map(|artifact| artifact.sha256.clone())
            .with_context(|| {
                format!(
                    "the manifest carries no digest for {filename} — this release \
                     predates digest publishing and cannot be verified, so it \
                     cannot be self-installed"
                )
            })?;

        let url = format!(
            "{}/v{}/{}",
            self.config.self_upgrade_download_base.trim_end_matches('/'),
            target_version,
            filename
        );
        validate_download_url(&url)?;

        // One upgrade at a time. The slot is taken before spawning so two
        // clicks race here, not in the filesystem.
        {
            let mut live = self.live.lock().expect("live lock");
            if live.is_some() {
                bail!("an upgrade is already running");
            }
            *live = Some(LiveProgress {
                phase: "downloading",
                to_version: target_version.to_string(),
            });
        }

        let upgrader = self.clone();
        let target_version = target_version.to_string();
        tokio::spawn(async move {
            if let Err(error) = upgrader
                .run(&self_dir, &target_version, &url, &expected_digest)
                .await
            {
                tracing::error!(%error, "self-upgrade failed");
                let journal = Journal {
                    state: JournalState::Failed,
                    from_version: updates::current_version().to_string(),
                    to_version: target_version.clone(),
                    previous: String::new(),
                    target: String::new(),
                    updated_at: nudo_bootguard::epoch_seconds(),
                    error: format!("{error:#}"),
                };
                if let Err(error) = journal.store(&self_dir) {
                    tracing::error!(%error, "recording the failure also failed");
                }
                *upgrader.live.lock().expect("live lock") = None;
            }
            // On success this task never gets here: run() ends in exec().
        });

        Ok(())
    }

    fn set_phase(&self, phase: &'static str, to_version: &str) {
        *self.live.lock().expect("live lock") = Some(LiveProgress {
            phase,
            to_version: to_version.to_string(),
        });
    }

    /// The upgrade proper. Only ever returns an error: success is exec().
    async fn run(
        &self,
        self_dir: &Path,
        version: &str,
        url: &str,
        expected_digest: &str,
    ) -> anyhow::Result<()> {
        let staging = self_dir.join("tmp");
        let release_dir = self_dir.join("releases").join(version);

        // A leftover directory from an earlier failed attempt is removed, the
        // same way the ingress reconciler discards a stale staged config: the
        // only trustworthy staged state is the one this run writes.
        for stale in [&staging, &release_dir] {
            if stale.exists() {
                tokio::fs::remove_dir_all(stale)
                    .await
                    .with_context(|| format!("clearing {}", stale.display()))?;
            }
        }
        tokio::fs::create_dir_all(&staging)
            .await
            .context("creating the staging directory")?;

        let tarball = staging.join(artifact_filename(version));
        self.set_phase("downloading", version);
        let actual_digest = download(url, &tarball).await?;

        self.set_phase("verifying", version);
        if !digests_match(&actual_digest, expected_digest) {
            // The file is removed before the error is reported: a tarball that
            // failed verification has no business existing on disk.
            let _ = tokio::fs::remove_file(&tarball).await;
            bail!(
                "digest mismatch for {url}: the manifest says {expected_digest}, \
                 the download hashes to {actual_digest}. Refusing it."
            );
        }

        self.set_phase("staging", version);
        let unpack_tarball = tarball.clone();
        let unpack_dest = release_dir.clone();
        tokio::task::spawn_blocking(move || unpack(&unpack_tarball, &unpack_dest))
            .await
            .context("the unpack task panicked")??;
        tokio::fs::remove_dir_all(&staging)
            .await
            .context("removing the staging directory")?;

        // The snapshot lands inside the new release directory: it lives and
        // dies with the release it was taken for, and the retention sweep
        // cannot orphan it.
        self.store
            .snapshot_database(&release_dir.join("db-pre-upgrade.sqlite"))
            .await?;

        let current = nudo_bootguard::current_target(self_dir)
            .map(|target| target.to_string_lossy().to_string())
            .context("the current symlink is missing; the layout is broken")?;
        let target = format!("releases/{version}");
        let running = updates::current_version().to_string();

        let mut journal = Journal {
            state: JournalState::Staged,
            from_version: running.clone(),
            to_version: version.to_string(),
            previous: current,
            target: target.clone(),
            updated_at: nudo_bootguard::epoch_seconds(),
            error: String::new(),
        };
        journal
            .store(self_dir)
            .context("recording the staged release")?;

        nudo_bootguard::swap_current(self_dir, &target).context("swapping the current symlink")?;
        journal.state = JournalState::Swapped;
        journal.updated_at = nudo_bootguard::epoch_seconds();
        journal.store(self_dir).context("recording the swap")?;

        self.store
            .audit(crate::store::NewAuditEntry {
                actor: nudo_proto::Actor::system("self-upgrade"),
                action: "SelfUpgrade.Exec".to_string(),
                subject_id: format!("release/{version}"),
                dry_run: false,
                summary: format!("upgrading from {running} to {version}: exec'ing the new binary"),
            })
            .await;

        // Beyond this line the process image is replaced. Anything still
        // buffered would be lost, so say what is about to happen and flush.
        tracing::info!(from = %running, to = %version, "exec'ing the new binary");
        {
            use std::io::Write as _;
            let _ = std::io::stdout().flush();
            let _ = std::io::stderr().flush();
        }

        let exec_error = {
            use std::os::unix::process::CommandExt as _;
            // Through the symlink, so the new process's /proc/self/exe — and
            // therefore its own eligibility check — resolves into the release
            // directory it actually runs from. Same argv: the unit passes
            // flags via the environment, and a manual invocation's flags stay
            // meaningful across versions.
            std::process::Command::new(
                self_dir
                    .join(nudo_bootguard::CURRENT_LINK)
                    .join(MAIN_BINARY),
            )
            .args(std::env::args_os().skip(1))
            .exec()
        };

        // Reachable only when exec failed: the old binary is still running and
        // still correct, so put the symlink back and keep serving. Handled
        // here rather than by returning an error, because the caller's error
        // arm records a generic `failed` journal — and this failure has a
        // more precise resting state that must not be overwritten.
        tracing::error!(error = %exec_error, "exec of the new binary failed; rolling back");
        nudo_bootguard::swap_current(self_dir, &journal.previous)
            .context("reverting the symlink after a failed exec")?;
        journal.state = JournalState::ExecFailed;
        journal.updated_at = nudo_bootguard::epoch_seconds();
        journal.error = format!("exec of the new binary failed: {exec_error}");
        journal
            .store(self_dir)
            .context("recording the failed exec")?;
        *self.live.lock().expect("live lock") = None;
        Ok(())
    }
}

/// Finishes an upgrade after the process restarts, and tidies old releases.
///
/// Runs at boot, after the store is open — meaning the database opened and the
/// migrations applied, which is the definition of "the new version works"
/// worth having here. Confirming clears the boot-attempt counter the guard
/// counts against, and only then is the stable guard copy refreshed: the
/// guard that protects the *next* upgrade always comes from a release that
/// proved it could boot.
pub async fn reconcile_on_boot(config: &crate::Config, store: &Store) {
    let Eligibility::Managed { self_dir } = eligibility(config) else {
        return;
    };
    reconcile_in(&self_dir, store).await;
}

/// The reconciliation itself, on an explicit directory. Split from
/// `reconcile_on_boot` so tests can exercise it without relocating the test
/// executable into a managed layout.
async fn reconcile_in(self_dir: &Path, store: &Store) {
    let journal = match Journal::load(self_dir) {
        Ok(Some(journal)) => journal,
        Ok(None) => return,
        Err(error) => {
            tracing::warn!(%error, "the self-upgrade journal is unreadable");
            return;
        }
    };

    if journal.state != JournalState::Swapped {
        // Resting states stay for the dashboard; nothing to reconcile.
        return;
    }

    let running = updates::current_version();
    if journal.to_version == running {
        let mut confirmed = journal.clone();
        confirmed.state = JournalState::Confirmed;
        confirmed.updated_at = nudo_bootguard::epoch_seconds();
        if let Err(error) = confirmed.store(self_dir) {
            tracing::error!(%error, "could not record the confirmed upgrade");
            return;
        }
        let _ = nudo_bootguard::clear_attempts(self_dir);
        refresh_guard_copy(self_dir);
        store
            .audit(crate::store::NewAuditEntry {
                actor: nudo_proto::Actor::system("self-upgrade"),
                action: "SelfUpgrade.Confirmed".to_string(),
                subject_id: format!("release/{}", journal.to_version),
                dry_run: false,
                summary: format!(
                    "upgraded from {} to {} and confirmed the new version",
                    journal.from_version, journal.to_version
                ),
            })
            .await;
        tracing::info!(
            from = %journal.from_version,
            to = %journal.to_version,
            "self-upgrade confirmed"
        );
        prune_releases(self_dir, &journal);
    } else {
        // A swapped release should boot as the version it promised. Running as
        // anything else means the swap went somewhere wrong — most likely the
        // guard reverted us and this is the old binary coming back up before
        // the guard updated the journal, which it does first; so in practice:
        // a genuinely wrong binary.
        let mut failed = journal.clone();
        failed.state = JournalState::Failed;
        failed.updated_at = nudo_bootguard::epoch_seconds();
        failed.error = format!(
            "after the swap to {}, the process came up as {} — the release did \
             not become what it claimed",
            journal.to_version, running
        );
        if let Err(error) = failed.store(self_dir) {
            tracing::error!(%error, "could not record the version mismatch");
        }
        tracing::error!(
            expected = %journal.to_version,
            running = %running,
            "a swapped release booted as the wrong version"
        );
    }
}

/// Copies the confirmed release's guard to the stable path `ExecStartPre`
/// runs. Best-effort: a release without the guard (during rollout of this
/// very feature) keeps the existing copy.
fn refresh_guard_copy(self_dir: &Path) {
    let source = self_dir
        .join(nudo_bootguard::CURRENT_LINK)
        .join("nudo-boot-guard");
    if !source.exists() {
        tracing::debug!("the current release ships no boot guard; keeping the existing copy");
        return;
    }
    let stable = self_dir.join("nudo-boot-guard");
    let temp = self_dir.join("nudo-boot-guard.tmp");
    let result = std::fs::copy(&source, &temp).and_then(|_| {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(&temp, std::fs::Permissions::from_mode(0o755))?;
        }
        std::fs::rename(&temp, &stable)
    });
    if let Err(error) = result {
        tracing::warn!(%error, "could not refresh the stable boot-guard copy");
    }
}

/// Removes old self-releases, keeping the newest `KEEP_RELEASES` and never
/// anything the symlink or the journal still names.
fn prune_releases(self_dir: &Path, journal: &Journal) {
    let releases_dir = self_dir.join("releases");
    let mut keep: Vec<String> = vec![journal.previous.clone(), journal.target.clone()]
        .into_iter()
        .filter_map(|rel| rel.strip_prefix("releases/").map(str::to_string))
        .collect();
    if let Some(current) = nudo_bootguard::current_target(self_dir)
        && let Ok(stripped) = current.strip_prefix("releases")
    {
        keep.push(stripped.to_string_lossy().to_string());
    }

    let Ok(entries) = std::fs::read_dir(&releases_dir) else {
        return;
    };
    let mut versions: Vec<String> = entries
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.path().is_dir())
        .map(|entry| entry.file_name().to_string_lossy().to_string())
        .collect();
    versions.sort_by(|a, b| updates::compare_versions(b, a).unwrap_or(std::cmp::Ordering::Equal));

    for doomed in versions.iter().skip(KEEP_RELEASES) {
        if keep.iter().any(|kept| kept == doomed) {
            continue;
        }
        let path = releases_dir.join(doomed);
        match std::fs::remove_dir_all(&path) {
            Ok(()) => tracing::info!(release = %doomed, "pruned an old self-release"),
            Err(error) => tracing::warn!(%error, release = %doomed, "could not prune"),
        }
    }
}

/// The tarball name for a version, matching what the release workflow
/// publishes. Always the musl artifact: fully static, so the swap can never
/// fail on the host's libc — the failure class exec() cannot recover from
/// gracefully is the one we refuse to ship.
fn artifact_filename(version: &str) -> String {
    format!("nudo-v{version}-x86_64-unknown-linux-musl.tar.gz")
}

/// Refuses download bases that are neither https nor loopback.
///
/// Loopback http is allowed so the end-to-end test (and an operator's local
/// mirror) can serve artifacts without a certificate; anything remote must be
/// https. The URL is already constructed by us — this guards the configured
/// base, not a caller-supplied value.
fn validate_download_url(url: &str) -> anyhow::Result<()> {
    let parsed = reqwest::Url::parse(url).with_context(|| format!("not a URL: {url}"))?;
    match parsed.scheme() {
        "https" => Ok(()),
        "http" => {
            let loopback = matches!(
                parsed.host_str(),
                Some("localhost") | Some("127.0.0.1") | Some("[::1]") | Some("::1")
            );
            if loopback {
                Ok(())
            } else {
                bail!("refusing to download a release over plain http from {url}")
            }
        }
        scheme => bail!("refusing the {scheme} scheme for a release download"),
    }
}

/// Downloads `url` to `dest`, returning the sha256 of what was written.
///
/// Streamed: bytes are hashed and written as they arrive, and the byte count
/// is enforced as it grows. The deploy engine's artifact download checks
/// `content_length` and then buffers the body whole; for something that will
/// be exec'd as this process, neither is good enough.
async fn download(url: &str, dest: &Path) -> anyhow::Result<String> {
    use sha2::Digest as _;
    use tokio::io::AsyncWriteExt as _;

    let client = reqwest::Client::builder()
        .user_agent(concat!("nudo/", env!("CARGO_PKG_VERSION")))
        // Release assets redirect to object storage; bounded, not disabled.
        .redirect(reqwest::redirect::Policy::limited(5))
        .timeout(DOWNLOAD_TIMEOUT)
        .build()?;

    let response = client
        .get(url)
        .send()
        .await
        .with_context(|| format!("downloading {url}"))?;
    if !response.status().is_success() {
        bail!("{url} returned HTTP {}", response.status());
    }

    let mut file = tokio::fs::File::create(dest)
        .await
        .with_context(|| format!("creating {}", dest.display()))?;
    let mut hasher = sha2::Sha256::new();
    let mut total: u64 = 0;
    let mut response = response;
    while let Some(chunk) = response.chunk().await.context("reading the download")? {
        total += chunk.len() as u64;
        if total > MAX_ARTIFACT_BYTES {
            bail!(
                "{url} exceeded {} bytes; refusing to keep reading",
                MAX_ARTIFACT_BYTES
            );
        }
        hasher.update(&chunk);
        file.write_all(&chunk)
            .await
            .context("writing the download")?;
    }
    if total == 0 {
        bail!("{url} returned an empty body");
    }
    file.sync_all().await.context("flushing the download")?;
    Ok(hex::encode(hasher.finalize()))
}

/// Compares two hex digests in constant time.
fn digests_match(actual: &str, expected: &str) -> bool {
    use subtle::ConstantTimeEq as _;
    actual.len() == expected.len()
        && actual
            .to_lowercase()
            .as_bytes()
            .ct_eq(expected.to_lowercase().as_bytes())
            .into()
}

/// Unpacks the verified tarball into the release directory.
///
/// The archive layout is `nudo-v<version>-<target>/<file>`; the top-level
/// directory is stripped. Only regular files with expected names are
/// extracted; a path that is absolute or contains `..` is treated as hostile
/// and fails the upgrade rather than being skipped.
fn unpack(tarball: &Path, release_dir: &Path) -> anyhow::Result<()> {
    let file = std::fs::File::open(tarball).context("opening the tarball")?;
    let mut archive = tar::Archive::new(flate2::read::GzDecoder::new(file));
    std::fs::create_dir_all(release_dir).context("creating the release directory")?;

    let mut extracted_main = false;
    for entry in archive.entries().context("reading the tarball")? {
        let mut entry = entry.context("reading a tarball entry")?;
        if !entry.header().entry_type().is_file() {
            continue;
        }

        let path = entry.path().context("an entry with an unreadable path")?;
        let mut components = path.components();
        // Strip `nudo-v<version>-<target>/`.
        let _top = components.next();
        let rest: Vec<_> = components.collect();

        // A verified artifact should never trip these; if it does, someone
        // built a hostile tarball that also passed the digest, and the only
        // safe response is to stop.
        if path.is_absolute()
            || path
                .components()
                .any(|c| matches!(c, std::path::Component::ParentDir))
        {
            bail!("the tarball contains a hostile path: {}", path.display());
        }
        let [std::path::Component::Normal(name)] = rest.as_slice() else {
            // Nested directories are not part of the published layout.
            continue;
        };
        let name = name.to_string_lossy().to_string();

        let is_binary = BINARIES.contains(&name.as_str());
        // The version-override seam: the e2e test's fixture release carries
        // the file `current_version()` honours under the test feature. A
        // release build neither extracts nor reads it.
        let is_test_seam = cfg!(feature = "self-upgrade-test") && name == ".version-override";
        if !is_binary && !DOCS.contains(&name.as_str()) && !is_test_seam {
            continue;
        }

        let dest = release_dir.join(&name);
        let mut out =
            std::fs::File::create(&dest).with_context(|| format!("creating {}", dest.display()))?;
        std::io::copy(&mut entry, &mut out).with_context(|| format!("extracting {name}"))?;
        out.sync_all().context("flushing an extracted file")?;
        if is_binary {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt as _;
                std::fs::set_permissions(&dest, std::fs::Permissions::from_mode(0o755))
                    .with_context(|| format!("marking {name} executable"))?;
            }
        }
        if name == MAIN_BINARY {
            extracted_main = true;
        }
    }

    if !extracted_main {
        bail!("the tarball carries no {MAIN_BINARY}; refusing a release that cannot run");
    }
    // The directory entry itself, so the extracted names survive power loss.
    std::fs::File::open(release_dir)
        .and_then(|dir| dir.sync_all())
        .context("flushing the release directory")?;
    Ok(())
}

#[cfg(test)]
mod tests;
