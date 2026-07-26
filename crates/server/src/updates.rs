//! Release checking: is a newer version of nudo available?
//!
//! A manifest is fetched from a URL on a schedule, compared against the running
//! version, and the answer cached. The dashboard shows a banner when there is
//! something newer.
//!
//! Ported from Coolify's update check, with its two good ideas kept and one
//! deliberately dropped.
//!
//! Kept: the anti-rollback ladder — a manifest that has gone backwards, or that
//! advertises something older than what is already running, must never talk an
//! instance into "upgrading" downwards. And self-healing — the stored flag is
//! ANDed with a live comparison, so a stale `true` cannot leave a banner up
//! forever.
//!
//! Dropped: Coolify's one-click auto-update downloads `upgrade.sh` from a CDN
//! and executes it as root on the host. A control plane holding every target's
//! SSH keys should not run code fetched from a URL, so nudo tells you a release
//! exists and how to apply it, and leaves applying it to you or to your package
//! manager. See CHANGES.md.

use std::time::Duration;

use serde::{Deserialize, Serialize};

/// Where release information is published.
///
/// Deliberately a plain file in the repository rather than a bespoke service: it
/// is served by GitHub, it is diffable, and publishing a release is a commit.
pub const DEFAULT_MANIFEST_URL: &str =
    "https://raw.githubusercontent.com/loa212/nudo/main/releases.json";

/// The version this binary was built as.
pub fn current_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

/// How long a fetched manifest is trusted before being refetched.
const CACHE_TTL: Duration = Duration::from_secs(60 * 60);

/// A published release.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct Release {
    /// The version, without a leading `v`.
    pub version: String,
    /// ISO-8601 date, for display.
    #[serde(default)]
    pub published_at: String,
    /// Markdown release notes, shown in the dashboard's "What's new".
    #[serde(default)]
    pub notes: String,
    /// Where to read more.
    #[serde(default)]
    pub url: String,
    /// Set when a release needs manual steps before or after upgrading. Shown
    /// prominently, because the whole point of a changelog is that someone reads
    /// the one entry that matters.
    #[serde(default)]
    pub breaking: bool,
}

/// The published manifest.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Manifest {
    /// Newest first. Only the first is used for the update check; the rest are
    /// the changelog.
    #[serde(default)]
    pub releases: Vec<Release>,
}

impl Manifest {
    /// The newest release, which is what "latest" means.
    ///
    /// Ordering is by semver rather than by position, so a manifest listing its
    /// releases in the wrong order still yields the right answer.
    pub fn latest(&self) -> Option<&Release> {
        self.releases.iter().max_by(|a, b| {
            compare_versions(&a.version, &b.version).unwrap_or(std::cmp::Ordering::Equal)
        })
    }
}

/// What the dashboard needs to render the update banner.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpdateStatus {
    pub current: String,
    pub latest: String,
    /// True only when `latest` really is newer, checked live rather than
    /// trusted from storage.
    pub available: bool,
    /// Whether that release needs manual steps.
    pub breaking: bool,
    pub url: String,
}

impl UpdateStatus {
    /// The status when nothing is known — a fresh instance, or checking
    /// disabled.
    pub fn unknown() -> Self {
        Self {
            current: current_version().to_string(),
            latest: current_version().to_string(),
            available: false,
            breaking: false,
            url: String::new(),
        }
    }
}

/// Compares two dotted version strings.
///
/// Returns `None` for anything unparseable, which callers treat as "not newer" —
/// a malformed manifest must never be read as an upgrade.
pub fn compare_versions(left: &str, right: &str) -> Option<std::cmp::Ordering> {
    let parse = |raw: &str| -> Option<Vec<u64>> {
        let trimmed = raw.trim().trim_start_matches('v');
        // Only the release part: `1.2.0-rc.1` compares as `1.2.0` for ordering,
        // and the pre-release suffix is handled below.
        let core = trimmed.split(['-', '+']).next()?;
        let parts: Vec<u64> = core
            .split('.')
            .map(|part| part.parse::<u64>().ok())
            .collect::<Option<_>>()?;
        if parts.is_empty() { None } else { Some(parts) }
    };

    let (left_parts, right_parts) = (parse(left)?, parse(right)?);

    // Compare component-wise, treating a missing component as zero so `1.2` and
    // `1.2.0` are equal.
    let width = left_parts.len().max(right_parts.len());
    for index in 0..width {
        let a = left_parts.get(index).copied().unwrap_or(0);
        let b = right_parts.get(index).copied().unwrap_or(0);
        if a != b {
            return Some(a.cmp(&b));
        }
    }

    // Same release numbers: a pre-release is older than the final release it
    // leads to, so `1.2.0-rc.1` < `1.2.0`.
    let pre = |raw: &str| raw.trim().trim_start_matches('v').contains('-');
    Some(match (pre(left), pre(right)) {
        (true, false) => std::cmp::Ordering::Less,
        (false, true) => std::cmp::Ordering::Greater,
        _ => std::cmp::Ordering::Equal,
    })
}

/// Whether `candidate` is newer than `current`.
pub fn is_newer(candidate: &str, current: &str) -> bool {
    compare_versions(candidate, current) == Some(std::cmp::Ordering::Greater)
}

/// Picks the version to advertise, refusing to go backwards.
///
/// Given what the manifest says, what was last recorded, and what is running,
/// returns the newest of the three. Ported from Coolify: a manifest that has
/// regressed — a bad publish, a stale mirror, a hostile response — must not be
/// able to advertise a downgrade as an update.
pub fn best_known_version(from_manifest: &str, recorded: &str, running: &str) -> String {
    let mut best = running.to_string();
    for candidate in [recorded, from_manifest] {
        if is_newer(candidate, &best) {
            best = candidate.to_string();
        }
    }
    best
}

/// Fetches the manifest.
pub async fn fetch_manifest(url: &str) -> anyhow::Result<Manifest> {
    use anyhow::Context as _;

    let client = reqwest::Client::builder()
        .user_agent(concat!("nudo/", env!("CARGO_PKG_VERSION")))
        // Short: this runs on a timer and nothing waits on it.
        .timeout(Duration::from_secs(15))
        .build()?;

    let response = client
        .get(url)
        .send()
        .await
        .with_context(|| format!("fetching the release manifest from {url}"))?;

    if !response.status().is_success() {
        anyhow::bail!("{url} returned HTTP {}", response.status());
    }

    // Bounded: this is parsed into memory, and a manifest is a few kilobytes.
    let body = response.text().await.context("reading the manifest")?;
    if body.len() > 1024 * 1024 {
        anyhow::bail!(
            "the release manifest is implausibly large ({} bytes)",
            body.len()
        );
    }

    serde_json::from_str(&body).context("parsing the release manifest")
}

/// The update checker's shared state.
#[derive(Clone)]
pub struct UpdateChecker {
    store: crate::store::Store,
    url: String,
    enabled: bool,
}

impl UpdateChecker {
    pub fn new(store: crate::store::Store, url: String, enabled: bool) -> Self {
        Self {
            store,
            url,
            enabled,
        }
    }

    /// Checks once, recording the result.
    ///
    /// Two switches have to be on: the config flag this was built with, and the
    /// operator's own choice in the dashboard. Consulted on every check rather
    /// than at startup, so unticking the box stops the next tick instead of the
    /// next restart.
    pub async fn check_now(&self) -> anyhow::Result<UpdateStatus> {
        if !self.enabled || !self.store.release_check_enabled().await.unwrap_or(true) {
            return Ok(UpdateStatus::unknown());
        }

        let manifest = fetch_manifest(&self.url).await?;
        let latest = manifest
            .latest()
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("the release manifest lists no releases"))?;

        let recorded = self.store.recorded_latest_version().await?;
        let running = current_version();

        // Never advertise something older than what is already known or running.
        let best = best_known_version(&latest.version, &recorded, running);

        // The notes belong to whichever release actually won.
        let advertised = manifest
            .releases
            .iter()
            .find(|release| release.version == best)
            .cloned()
            .unwrap_or(latest);

        self.store
            .record_latest_version(&best, &serde_json::to_string(&manifest)?)
            .await?;

        Ok(UpdateStatus {
            current: running.to_string(),
            latest: best.clone(),
            available: is_newer(&best, running),
            breaking: advertised.breaking,
            url: advertised.url,
        })
    }

    /// The last known status, without a network call.
    ///
    /// The stored version is compared live rather than trusting a stored
    /// boolean, so an instance that has since been upgraded stops showing a
    /// banner without needing another check to clear it.
    pub async fn cached_status(&self) -> UpdateStatus {
        // Someone who turns the check off wants the banner gone, not frozen at
        // whatever the last check happened to find. The recorded row is left
        // alone so turning it back on restores what was known without waiting
        // for the next tick.
        if !self.store.release_check_enabled().await.unwrap_or(true) {
            return UpdateStatus::unknown();
        }

        let running = current_version();
        let recorded = self
            .store
            .recorded_latest_version()
            .await
            .unwrap_or_default();

        if recorded.is_empty() {
            return UpdateStatus::unknown();
        }

        let manifest: Manifest = self
            .store
            .recorded_manifest()
            .await
            .ok()
            .flatten()
            .and_then(|raw| serde_json::from_str(&raw).ok())
            .unwrap_or_default();

        let release = manifest
            .releases
            .iter()
            .find(|candidate| candidate.version == recorded);

        UpdateStatus {
            current: running.to_string(),
            latest: recorded.clone(),
            available: is_newer(&recorded, running),
            breaking: release.map(|r| r.breaking).unwrap_or(false),
            url: release.map(|r| r.url.clone()).unwrap_or_default(),
        }
    }

    /// The changelog, newest first.
    pub async fn changelog(&self) -> Vec<Release> {
        if !self.store.release_check_enabled().await.unwrap_or(true) {
            return Vec::new();
        }

        let manifest: Manifest = self
            .store
            .recorded_manifest()
            .await
            .ok()
            .flatten()
            .and_then(|raw| serde_json::from_str(&raw).ok())
            .unwrap_or_default();

        let mut releases = manifest.releases;
        releases.sort_by(|a, b| {
            compare_versions(&b.version, &a.version).unwrap_or(std::cmp::Ordering::Equal)
        });
        releases
    }
}

/// Starts the periodic check.
///
/// Runs once shortly after boot and then on the interval. Failures are logged
/// and retried on the next tick — an instance that cannot reach the manifest
/// should keep working, not complain.
pub fn spawn(checker: UpdateChecker, interval: Duration) {
    if !checker.enabled {
        tracing::info!("release checking is disabled");
        return;
    }

    tokio::spawn(async move {
        // Not immediately: a fresh instance has more urgent things to do than
        // an HTTP request, and staggering avoids every instance in a fleet
        // checking at the same instant after a coordinated restart.
        tokio::time::sleep(Duration::from_secs(30)).await;

        let mut ticker = tokio::time::interval(interval.max(CACHE_TTL));
        loop {
            match checker.check_now().await {
                Ok(status) if status.available => {
                    tracing::info!(
                        current = %status.current,
                        latest = %status.latest,
                        "a newer release of nudo is available"
                    );
                }
                Ok(_) => tracing::debug!("nudo is up to date"),
                Err(error) => tracing::debug!(%error, "the release check failed"),
            }
            ticker.tick().await;
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cmp::Ordering;

    #[test]
    fn versions_compare_numerically_not_lexically() {
        // The bug every string comparison has: "10" < "9" as text.
        assert_eq!(compare_versions("0.10.0", "0.9.0"), Some(Ordering::Greater));
        assert_eq!(
            compare_versions("1.0.0", "0.99.99"),
            Some(Ordering::Greater)
        );
        assert_eq!(compare_versions("2.0.0", "10.0.0"), Some(Ordering::Less));
    }

    #[test]
    fn a_leading_v_is_ignored() {
        assert_eq!(compare_versions("v1.2.3", "1.2.3"), Some(Ordering::Equal));
        assert_eq!(compare_versions("v1.3.0", "1.2.9"), Some(Ordering::Greater));
    }

    #[test]
    fn missing_components_count_as_zero() {
        assert_eq!(compare_versions("1.2", "1.2.0"), Some(Ordering::Equal));
        assert_eq!(compare_versions("1.2.1", "1.2"), Some(Ordering::Greater));
        assert_eq!(compare_versions("1", "1.0.0"), Some(Ordering::Equal));
    }

    #[test]
    fn a_prerelease_is_older_than_the_release_it_leads_to() {
        assert_eq!(
            compare_versions("1.2.0-rc.1", "1.2.0"),
            Some(Ordering::Less)
        );
        assert_eq!(
            compare_versions("1.2.0", "1.2.0-rc.1"),
            Some(Ordering::Greater)
        );
        // But still newer than the previous release.
        assert!(is_newer("1.2.0-rc.1", "1.1.9"));
    }

    #[test]
    fn an_unparseable_version_is_never_newer() {
        // A malformed manifest must not be readable as an upgrade.
        assert!(compare_versions("not-a-version", "1.0.0").is_none());
        assert!(!is_newer("not-a-version", "1.0.0"));
        assert!(!is_newer("", "1.0.0"));
        assert!(!is_newer("latest", "1.0.0"));
    }

    #[test]
    fn only_a_strictly_greater_version_counts_as_newer() {
        assert!(is_newer("1.1.0", "1.0.0"));
        assert!(!is_newer("1.0.0", "1.0.0"));
        assert!(!is_newer("0.9.0", "1.0.0"));
    }

    #[test]
    fn the_anti_rollback_ladder_never_advertises_a_downgrade() {
        // A manifest that regressed — a bad publish, a stale mirror, a hostile
        // response — must not talk an instance into "upgrading" backwards.
        assert_eq!(best_known_version("0.9.0", "1.0.0", "1.0.0"), "1.0.0");
        assert_eq!(best_known_version("0.1.0", "0.1.0", "0.5.0"), "0.5.0");

        // A genuine upgrade still comes through.
        assert_eq!(best_known_version("1.1.0", "1.0.0", "1.0.0"), "1.1.0");

        // And a manifest that has caught up with a newer recorded value is fine.
        assert_eq!(best_known_version("1.2.0", "1.1.0", "1.0.0"), "1.2.0");
    }

    #[test]
    fn the_running_version_is_the_floor() {
        // Someone who upgraded past the manifest is not offered a downgrade.
        assert_eq!(best_known_version("1.0.0", "1.0.0", "2.0.0"), "2.0.0");
    }

    #[test]
    fn the_latest_release_is_chosen_by_version_not_by_position() {
        // A manifest listing its releases out of order still yields the right
        // answer.
        let manifest = Manifest {
            releases: vec![
                Release {
                    version: "0.9.0".to_string(),
                    ..Default::default()
                },
                Release {
                    version: "1.2.0".to_string(),
                    ..Default::default()
                },
                Release {
                    version: "1.0.0".to_string(),
                    ..Default::default()
                },
            ],
        };
        assert_eq!(manifest.latest().map(|r| r.version.as_str()), Some("1.2.0"));
    }

    #[test]
    fn an_empty_manifest_has_no_latest_release() {
        assert!(Manifest::default().latest().is_none());
    }

    #[test]
    fn a_manifest_round_trips_through_json() {
        let json = r#"{
            "releases": [
                {
                    "version": "1.2.0",
                    "published_at": "2026-07-26",
                    "notes": "Added a thing",
                    "url": "https://github.com/loa212/nudo/releases/tag/v1.2.0",
                    "breaking": true
                }
            ]
        }"#;

        let manifest: Manifest = serde_json::from_str(json).expect("parse");
        let release = manifest.latest().expect("a release");
        assert_eq!(release.version, "1.2.0");
        assert!(release.breaking);
        assert!(release.notes.contains("Added a thing"));
    }

    #[test]
    fn a_manifest_missing_optional_fields_still_parses() {
        // Only `version` is required, so a minimal manifest is valid.
        let manifest: Manifest =
            serde_json::from_str(r#"{"releases":[{"version":"1.0.0"}]}"#).expect("parse");
        let release = manifest.latest().expect("a release");
        assert!(release.notes.is_empty());
        assert!(!release.breaking);
    }

    #[test]
    fn an_unknown_status_reports_no_update() {
        let status = UpdateStatus::unknown();
        assert!(!status.available);
        assert_eq!(status.current, status.latest);
        assert_eq!(status.current, current_version());
    }

    #[tokio::test]
    async fn a_disabled_checker_reports_nothing_without_a_network_call() {
        let store = crate::store::Store::open_in_memory().await.expect("store");
        let checker = UpdateChecker::new(store, "http://127.0.0.1:1".to_string(), false);
        let status = checker.check_now().await.expect("no network call is made");
        assert!(!status.available);
    }

    #[tokio::test]
    async fn an_unreachable_manifest_is_an_error_rather_than_a_false_update() {
        let store = crate::store::Store::open_in_memory().await.expect("store");
        let checker = UpdateChecker::new(store, "http://127.0.0.1:1".to_string(), true);
        assert!(checker.check_now().await.is_err());
    }

    #[tokio::test]
    async fn a_cached_status_self_heals_once_the_instance_catches_up() {
        // Coolify's flag can get stuck true; ANDing it with a live comparison
        // means an instance that has been upgraded stops showing the banner
        // without waiting for another check.
        let store = crate::store::Store::open_in_memory().await.expect("store");
        let checker = UpdateChecker::new(store.clone(), String::new(), true);

        // Something newer than us: the banner should show.
        store
            .record_latest_version("999.0.0", r#"{"releases":[{"version":"999.0.0"}]}"#)
            .await
            .expect("record");
        assert!(checker.cached_status().await.available);

        // Something older than us — as if we had upgraded past it.
        store
            .record_latest_version("0.0.1", r#"{"releases":[{"version":"0.0.1"}]}"#)
            .await
            .expect("record");
        assert!(!checker.cached_status().await.available);
    }

    #[tokio::test]
    async fn the_changelog_is_returned_newest_first() {
        let store = crate::store::Store::open_in_memory().await.expect("store");
        store
            .record_latest_version(
                "1.2.0",
                r#"{"releases":[
                    {"version":"1.0.0"},
                    {"version":"1.2.0"},
                    {"version":"1.1.0"}
                ]}"#,
            )
            .await
            .expect("record");

        let checker = UpdateChecker::new(store, String::new(), true);
        let versions: Vec<String> = checker
            .changelog()
            .await
            .into_iter()
            .map(|release| release.version)
            .collect();
        assert_eq!(versions, vec!["1.2.0", "1.1.0", "1.0.0"]);
    }

    #[tokio::test]
    async fn a_fresh_instance_has_no_recorded_release() {
        let store = crate::store::Store::open_in_memory().await.expect("store");
        let checker = UpdateChecker::new(store, String::new(), true);

        let status = checker.cached_status().await;
        assert!(!status.available);
        assert!(checker.changelog().await.is_empty());
    }
    #[test]
    fn the_manifest_committed_to_this_repository_parses() {
        // releases.json is what every running instance fetches. A typo in it
        // would leave every dashboard silently showing no releases, and nothing
        // else in this repository would notice.
        let raw = include_str!("../../../releases.json");
        let manifest: Manifest = serde_json::from_str(raw).expect("releases.json is valid");

        assert!(
            !manifest.releases.is_empty(),
            "the manifest lists no releases"
        );

        for release in &manifest.releases {
            assert!(
                compare_versions(&release.version, "0.0.0").is_some(),
                "{} is not a version this can sort, so the banner would never \
                 fire for it",
                release.version
            );
            assert!(
                release.url.starts_with("https://"),
                "{} has no https url",
                release.version
            );
        }
    }

    #[test]
    fn the_manifest_lists_the_version_this_binary_reports() {
        // If Cargo.toml is bumped without adding a manifest entry, the release
        // check would tell everyone the newest release is older than what they
        // are running. `best_known_version` already refuses to go backwards, but
        // the real fix is to notice at build time.
        let raw = include_str!("../../../releases.json");
        let manifest: Manifest = serde_json::from_str(raw).expect("releases.json is valid");

        let running = current_version();
        assert!(
            manifest
                .releases
                .iter()
                .any(|release| release.version.trim_start_matches('v') == running),
            "this binary reports {running}, which releases.json does not list — \
             add an entry (scripts/add-release.py) or the release check will \
             report a version older than the one running"
        );
    }

    #[tokio::test]
    async fn turning_the_check_off_clears_the_banner_rather_than_freezing_it() {
        // The bug this pins: the switch stopped future checks but left the last
        // result on the dashboard, so unticking the box appeared to do nothing.
        let store = crate::store::Store::open_in_memory().await.expect("store");
        store
            .record_latest_version("99.0.0", r#"{"releases":[{"version":"99.0.0"}]}"#)
            .await
            .expect("record");

        let checker = UpdateChecker::new(store.clone(), String::new(), true);
        assert!(
            checker.cached_status().await.available,
            "the banner is shown"
        );

        store
            .set_release_check_enabled(false)
            .await
            .expect("turn it off");
        assert!(
            !checker.cached_status().await.available,
            "the banner survived the switch being turned off"
        );
        assert!(
            checker.changelog().await.is_empty(),
            "the changelog survived the switch being turned off"
        );

        // Turning it back on restores what was known, without waiting for a
        // check to run again.
        store.set_release_check_enabled(true).await.expect("on");
        assert!(checker.cached_status().await.available);
        assert_eq!(checker.changelog().await.len(), 1);
    }
}
