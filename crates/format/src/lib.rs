//! Canonical human-readable presentation semantics for nudo clients.
//!
//! The CLI, dashboard, MCP tools, and server all present the same protocol
//! values. Keeping their vocabulary here prevents a unit, artifact, timestamp,
//! or journal priority from meaning something different depending on which
//! client an operator happens to use.

use nudo_proto::{Secret, Service, UnitStatus, artifact_source};

/// A relative time for a protocol timestamp.
pub fn ago(timestamp: Option<&nudo_proto::Timestamp>) -> String {
    let Some(when) = timestamp.and_then(nudo_proto::from_timestamp) else {
        return "-".to_string();
    };
    ago_at(when)
}

/// A relative time for a chrono timestamp.
pub fn ago_at(when: chrono::DateTime<chrono::Utc>) -> String {
    let seconds = chrono::Utc::now().signed_duration_since(when).num_seconds();

    // A peer whose clock is slightly ahead should still read as current.
    if seconds < 60 {
        return "just now".to_string();
    }
    if seconds < 3_600 {
        return format!("{}m ago", seconds / 60);
    }
    if seconds < 86_400 {
        return format!("{}h ago", seconds / 3_600);
    }
    format!("{}d ago", seconds / 86_400)
}

/// How long an operation took, or `running` while it has no finish time.
pub fn duration(
    started: Option<&nudo_proto::Timestamp>,
    finished: Option<&nudo_proto::Timestamp>,
) -> String {
    let Some(started) = started.and_then(nudo_proto::from_timestamp) else {
        return "-".to_string();
    };
    let Some(finished) = finished.and_then(nudo_proto::from_timestamp) else {
        return "running".to_string();
    };

    let seconds = finished.signed_duration_since(started).num_seconds().max(0);
    if seconds < 60 {
        format!("{seconds}s")
    } else {
        format!("{}m{}s", seconds / 60, seconds % 60)
    }
}

/// A byte count in the largest binary unit that keeps it readable.
pub fn bytes(count: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    if count == 0 {
        return "0 B".to_string();
    }

    let mut value = count as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }

    if unit == 0 {
        format!("{count} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

/// Shortens a value for a single-line table cell.
pub fn truncate(value: &str, limit: usize) -> String {
    let single_line = value.replace('\n', " ").trim().to_string();
    if single_line.is_empty() {
        return "-".to_string();
    }
    if single_line.chars().count() <= limit {
        return single_line;
    }
    let kept: String = single_line.chars().take(limit.saturating_sub(1)).collect();
    format!("{kept}…")
}

/// A compact description of where a service artifact comes from.
pub fn artifact_summary(service: &Service) -> String {
    match service.artifact.as_ref().and_then(|a| a.kind.as_ref()) {
        Some(artifact_source::Kind::Url(url)) if !url.is_empty() => "url".to_string(),
        Some(artifact_source::Kind::Git(git)) => git_summary(git),
        _ => "upload".to_string(),
    }
}

/// A complete one-line artifact description for an agent or diagnostic view.
pub fn artifact_description(service: &Service) -> String {
    match service.artifact.as_ref().and_then(|a| a.kind.as_ref()) {
        Some(artifact_source::Kind::Url(url)) if !url.is_empty() => format!("url:{url}"),
        Some(artifact_source::Kind::Git(git)) => git_summary(git),
        _ => "upload".to_string(),
    }
}

fn git_summary(git: &nudo_proto::GitSource) -> String {
    if git.branch.is_empty() {
        format!("git:{}", git.repo)
    } else {
        format!("git:{}@{}", git.repo, git.branch)
    }
}

/// Describes a secret's effective scope without exposing its value.
pub fn scope_label(secret: &Secret) -> String {
    match (
        secret.scope_target_id.is_empty(),
        secret.scope_service_id.is_empty(),
    ) {
        (true, true) => "global".to_string(),
        (false, true) => format!("target {}", secret.scope_target_id),
        (true, false) | (false, false) => format!("service {}", secret.scope_service_id),
    }
}

/// Collapses systemd's active/sub-state pair into one operator-facing phrase.
pub fn unit_state_label(status: &UnitStatus) -> &'static str {
    match (status.active_state.as_str(), status.sub_state.as_str()) {
        ("active", "running") => "running",
        ("active", "exited") => "exited cleanly",
        ("active", _) => "active",
        ("activating", _) => "starting",
        ("deactivating", _) => "stopping",
        ("failed", _) => "failed",
        ("inactive", _) => "stopped",
        ("unknown", _) => "unreachable",
        _ => "unknown",
    }
}

/// Maps a journald numeric priority to its conventional level name.
pub fn journal_priority_label(priority: &str) -> &'static str {
    match priority.trim() {
        "0" => "emerg",
        "1" => "alert",
        "2" => "crit",
        "3" => "err",
        "4" => "warning",
        "5" => "notice",
        "6" => "info",
        "7" => "debug",
        _ => "info",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relative_times_cover_missing_past_and_future_values() {
        let now = chrono::Utc::now();
        let at = |seconds: i64| nudo_proto::to_timestamp(now - chrono::Duration::seconds(seconds));

        assert_eq!(ago(Some(&at(5))), "just now");
        assert_eq!(ago(Some(&at(59))), "just now");
        assert_eq!(ago(Some(&at(120))), "2m ago");
        assert_eq!(ago(Some(&at(7_200))), "2h ago");
        assert_eq!(ago(Some(&at(172_800))), "2d ago");
        assert_eq!(ago(None), "-");
        assert_eq!(
            ago(Some(&nudo_proto::to_timestamp(
                now + chrono::Duration::hours(1)
            ))),
            "just now",
            "clock skew must not render a negative duration"
        );
    }

    #[test]
    fn durations_distinguish_missing_running_and_finished_operations() {
        let start = chrono::Utc::now();
        let started = nudo_proto::to_timestamp(start);
        let short = nudo_proto::to_timestamp(start + chrono::Duration::seconds(42));
        let long = nudo_proto::to_timestamp(start + chrono::Duration::seconds(95));

        assert_eq!(duration(Some(&started), Some(&short)), "42s");
        assert_eq!(duration(Some(&started), Some(&long)), "1m35s");
        assert_eq!(duration(Some(&started), None), "running");
        assert_eq!(duration(None, None), "-");
    }

    #[test]
    fn byte_counts_use_the_largest_readable_binary_unit() {
        assert_eq!(bytes(0), "0 B");
        assert_eq!(bytes(512), "512 B");
        assert_eq!(bytes(2_048), "2.0 KiB");
        assert_eq!(bytes(5 * 1_024 * 1_024), "5.0 MiB");
        assert_eq!(bytes(3 * 1_024 * 1_024 * 1_024), "3.0 GiB");
    }

    #[test]
    fn truncation_produces_one_bounded_nonempty_table_cell() {
        let rendered = truncate("line one\nline two\nline three", 20);
        assert!(!rendered.contains('\n'));
        assert!(rendered.chars().count() <= 20);
        assert!(rendered.ends_with('…'));
        assert_eq!(truncate("short", 20), "short");
        assert_eq!(truncate("", 20), "-");
        assert_eq!(truncate("   ", 20), "-");
    }

    #[test]
    fn secret_scope_uses_the_narrowest_available_boundary() {
        let scoped = |target: &str, service: &str| {
            scope_label(&Secret {
                scope_target_id: target.to_string(),
                scope_service_id: service.to_string(),
                ..Default::default()
            })
        };

        assert_eq!(scoped("", ""), "global");
        assert_eq!(scoped("tgt_1", ""), "target tgt_1");
        assert_eq!(scoped("", "svc_1"), "service svc_1");
        assert_eq!(scoped("tgt_1", "svc_1"), "service svc_1");
    }

    #[test]
    fn unit_states_have_one_vocabulary() {
        let status = |active: &str, sub: &str| UnitStatus {
            active_state: active.to_string(),
            sub_state: sub.to_string(),
            ..Default::default()
        };

        assert_eq!(unit_state_label(&status("active", "running")), "running");
        assert_eq!(
            unit_state_label(&status("active", "exited")),
            "exited cleanly"
        );
        assert_eq!(unit_state_label(&status("failed", "failed")), "failed");
        assert_eq!(unit_state_label(&status("unknown", "")), "unreachable");
    }

    #[test]
    fn artifact_views_cover_every_source_kind() {
        let with = |kind: artifact_source::Kind| Service {
            artifact: Some(nudo_proto::ArtifactSource { kind: Some(kind) }),
            ..Service::default()
        };

        let url = with(artifact_source::Kind::Url(
            "https://example.test/bot".to_string(),
        ));
        assert_eq!(artifact_summary(&url), "url");
        assert_eq!(artifact_description(&url), "url:https://example.test/bot");

        let git = with(artifact_source::Kind::Git(nudo_proto::GitSource {
            repo: "owner/repo".to_string(),
            branch: "main".to_string(),
            ..Default::default()
        }));
        assert_eq!(artifact_summary(&git), "git:owner/repo@main");
        assert_eq!(artifact_description(&git), "git:owner/repo@main");

        let branchless = with(artifact_source::Kind::Git(nudo_proto::GitSource {
            repo: "owner/repo".to_string(),
            ..Default::default()
        }));
        assert_eq!(artifact_summary(&branchless), "git:owner/repo");

        assert_eq!(
            artifact_summary(&with(artifact_source::Kind::DirectUpload(true))),
            "upload"
        );
        assert_eq!(
            artifact_summary(&with(artifact_source::Kind::Url(String::new()))),
            "upload"
        );
        assert_eq!(artifact_summary(&Service::default()), "upload");
    }
}
