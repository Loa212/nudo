//! Output formatting. Kept separate so it can be tested without a server.

use nudo_proto::{Deployment, Release, Secret, Service, Target, UnitStatus, deployment, target};

/// How to render results.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum Output {
    /// Aligned columns for a human.
    Table,
    /// One JSON object per invocation, for scripts and CI.
    Json,
}

/// Renders rows as aligned columns.
///
/// Column widths are computed from the content so a long service name does not
/// break the alignment of everything after it.
pub fn table(headers: &[&str], rows: &[Vec<String>]) -> String {
    if rows.is_empty() {
        return String::new();
    }

    let mut widths: Vec<usize> = headers.iter().map(|h| h.chars().count()).collect();
    for row in rows {
        for (index, cell) in row.iter().enumerate() {
            if index < widths.len() {
                widths[index] = widths[index].max(cell.chars().count());
            }
        }
    }

    let mut out = String::new();

    for (index, header) in headers.iter().enumerate() {
        if index > 0 {
            out.push_str("  ");
        }
        out.push_str(&pad(
            &header.to_uppercase(),
            widths[index],
            index + 1 == headers.len(),
        ));
    }
    out.push('\n');

    for row in rows {
        for (index, cell) in row.iter().enumerate() {
            if index > 0 {
                out.push_str("  ");
            }
            out.push_str(&pad(cell, widths[index], index + 1 == row.len()));
        }
        out.push('\n');
    }

    out
}

/// Pads a cell to a width. The last column is not padded, so lines have no
/// trailing whitespace.
fn pad(value: &str, width: usize, last: bool) -> String {
    if last {
        return value.to_string();
    }
    let len = value.chars().count();
    format!("{value}{}", " ".repeat(width.saturating_sub(len)))
}

/// A relative time, for list output. Absolute timestamps are the wrong default
/// when the question is usually "how long ago".
pub fn ago(timestamp: Option<&nudo_proto::Timestamp>) -> String {
    let Some(when) = timestamp.and_then(nudo_proto::from_timestamp) else {
        return "-".to_string();
    };

    let elapsed = chrono::Utc::now().signed_duration_since(when);
    let seconds = elapsed.num_seconds();

    // A negative value means the other side's clock is ahead; showing "in 3s"
    // would be confusing, so clamp to "just now".
    if seconds < 60 {
        return "just now".to_string();
    }
    if seconds < 3600 {
        return format!("{}m ago", seconds / 60);
    }
    if seconds < 86_400 {
        return format!("{}h ago", seconds / 3600);
    }
    format!("{}d ago", seconds / 86_400)
}

/// A duration between two timestamps, for deployment history.
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

/// Renders a byte count in the largest unit that keeps it readable.
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

pub fn targets_table(targets: &[Target]) -> String {
    let rows: Vec<Vec<String>> = targets
        .iter()
        .map(|t| {
            vec![
                t.id.clone(),
                t.name.clone(),
                format!("{}@{}:{}", t.user, t.host, t.port),
                target::Status::try_from(t.status)
                    .unwrap_or(target::Status::Unknown)
                    .as_str()
                    .to_string(),
                // Marked prominently: this is the flag that changes how every
                // other command behaves against this host.
                if t.latency_critical {
                    "yes".to_string()
                } else {
                    "-".to_string()
                },
                ago(t.last_seen_at.as_ref()),
            ]
        })
        .collect();

    table(
        &[
            "id",
            "name",
            "address",
            "status",
            "latency-critical",
            "last seen",
        ],
        &rows,
    )
}

pub fn services_table(services: &[Service]) -> String {
    let rows: Vec<Vec<String>> = services
        .iter()
        .map(|s| {
            vec![
                s.id.clone(),
                s.name.clone(),
                s.target_id.clone(),
                artifact_summary(s),
                if s.current_release_id.is_empty() {
                    "never deployed".to_string()
                } else {
                    s.current_release_id.clone()
                },
            ]
        })
        .collect();

    table(
        &["id", "name", "target", "source", "current release"],
        &rows,
    )
}

/// A one-word description of where a service's binary comes from.
pub fn artifact_summary(service: &Service) -> String {
    use nudo_proto::artifact_source::Kind;
    match service.artifact.as_ref().and_then(|a| a.kind.as_ref()) {
        Some(Kind::Url(url)) if !url.is_empty() => "url".to_string(),
        Some(Kind::Git(git)) => {
            if git.branch.is_empty() {
                format!("git:{}", git.repo)
            } else {
                format!("git:{}@{}", git.repo, git.branch)
            }
        }
        _ => "upload".to_string(),
    }
}

pub fn deployments_table(deployments: &[Deployment]) -> String {
    let rows: Vec<Vec<String>> = deployments
        .iter()
        .map(|d| {
            vec![
                d.id.clone(),
                deployment::Status::try_from(d.status)
                    .unwrap_or(deployment::Status::Unspecified)
                    .as_str()
                    .to_string(),
                d.actor
                    .as_ref()
                    .map(|a| a.label.clone())
                    .filter(|label| !label.is_empty())
                    .unwrap_or_else(|| "-".to_string()),
                ago(d.started_at.as_ref()),
                duration(d.started_at.as_ref(), d.finished_at.as_ref()),
                // Truncated: a multi-line build error would destroy the table.
                truncate(&d.error, 48),
            ]
        })
        .collect();

    table(
        &["id", "status", "actor", "started", "duration", "error"],
        &rows,
    )
}

pub fn releases_table(releases: &[Release]) -> String {
    let rows: Vec<Vec<String>> = releases
        .iter()
        .map(|r| {
            vec![
                r.id.clone(),
                if r.git_sha.is_empty() {
                    "-".to_string()
                } else {
                    r.git_sha.chars().take(8).collect()
                },
                if r.git_ref.is_empty() {
                    "-".to_string()
                } else {
                    r.git_ref.clone()
                },
                bytes(r.artifact_bytes),
                ago(r.created_at.as_ref()),
            ]
        })
        .collect();

    table(&["id", "sha", "ref", "size", "created"], &rows)
}

pub fn secrets_table(secrets: &[Secret]) -> String {
    let rows: Vec<Vec<String>> = secrets
        .iter()
        .map(|s| {
            vec![
                s.id.clone(),
                s.name.clone(),
                scope_label(s),
                // The digest, never the value — enough to tell whether two
                // environments hold the same secret.
                s.digest.chars().take(12).collect(),
                ago(s.updated_at.as_ref()),
            ]
        })
        .collect();

    table(&["id", "name", "scope", "digest", "updated"], &rows)
}

/// Describes a secret's scope for display.
pub fn scope_label(secret: &Secret) -> String {
    match (
        secret.scope_target_id.is_empty(),
        secret.scope_service_id.is_empty(),
    ) {
        (true, true) => "global".to_string(),
        (false, true) => format!("target {}", secret.scope_target_id),
        (true, false) => format!("service {}", secret.scope_service_id),
        // The proto allows both; narrower wins for display.
        (false, false) => format!("service {}", secret.scope_service_id),
    }
}

/// Renders a unit's live state as a single line.
pub fn unit_status_line(status: &UnitStatus) -> String {
    let mut line = format!(
        "{}  {}",
        crate::format_status_badge(status),
        crate::units_label(status)
    );

    if status.pid > 0 {
        line.push_str(&format!("  pid {}", status.pid));
    }
    if status.memory_bytes > 0 {
        line.push_str(&format!("  mem {}", bytes(status.memory_bytes)));
    }
    if status.restart_count > 0 {
        line.push_str(&format!("  restarts {}", status.restart_count));
    }
    if let Some(since) = status.since.as_ref() {
        line.push_str(&format!("  since {}", ago(Some(since))));
    }

    line
}

/// Shortens a string for a table cell, collapsing newlines.
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_table_aligns_columns_to_their_widest_cell() {
        let rendered = table(
            &["id", "name"],
            &[
                vec!["tgt_1".to_string(), "short".to_string()],
                vec![
                    "tgt_longer_id".to_string(),
                    "a much longer name".to_string(),
                ],
            ],
        );

        let lines: Vec<&str> = rendered.lines().collect();
        assert_eq!(lines.len(), 3, "a header and two rows");
        assert!(lines[0].starts_with("ID"));
        // The name column starts at the same offset on both rows.
        let first = lines[1].find("short").expect("cell");
        let second = lines[2].find("a much longer").expect("cell");
        assert_eq!(first, second);
    }

    #[test]
    fn table_lines_have_no_trailing_whitespace() {
        let rendered = table(&["a", "b"], &[vec!["1".to_string(), "2".to_string()]]);
        for line in rendered.lines() {
            assert_eq!(line, line.trim_end(), "trailing whitespace in {line:?}");
        }
    }

    #[test]
    fn an_empty_table_renders_nothing_rather_than_a_lone_header() {
        // A bare header row reads as though something was found.
        assert!(table(&["id", "name"], &[]).is_empty());
    }

    #[test]
    fn relative_times_read_naturally_at_each_scale() {
        let now = chrono::Utc::now();
        let at = |seconds: i64| nudo_proto::to_timestamp(now - chrono::Duration::seconds(seconds));

        assert_eq!(ago(Some(&at(5))), "just now");
        assert_eq!(ago(Some(&at(59))), "just now");
        assert_eq!(ago(Some(&at(120))), "2m ago");
        assert_eq!(ago(Some(&at(7200))), "2h ago");
        assert_eq!(ago(Some(&at(172_800))), "2d ago");
    }

    #[test]
    fn a_missing_timestamp_renders_as_a_dash() {
        assert_eq!(ago(None), "-");
    }

    #[test]
    fn a_future_timestamp_reads_as_just_now_rather_than_negative() {
        // Clock skew between the CLI and the server must not print "in -3s".
        let future = nudo_proto::to_timestamp(chrono::Utc::now() + chrono::Duration::hours(1));
        assert_eq!(ago(Some(&future)), "just now");
    }

    #[test]
    fn durations_distinguish_running_from_finished() {
        let start = chrono::Utc::now();
        let started = nudo_proto::to_timestamp(start);
        let finished = nudo_proto::to_timestamp(start + chrono::Duration::seconds(95));

        assert_eq!(duration(Some(&started), Some(&finished)), "1m35s");
        assert_eq!(duration(Some(&started), None), "running");
        assert_eq!(duration(None, None), "-");
    }

    #[test]
    fn a_sub_minute_duration_is_shown_in_seconds() {
        let start = chrono::Utc::now();
        let started = nudo_proto::to_timestamp(start);
        let finished = nudo_proto::to_timestamp(start + chrono::Duration::seconds(42));
        assert_eq!(duration(Some(&started), Some(&finished)), "42s");
    }

    #[test]
    fn byte_counts_use_the_largest_readable_unit() {
        assert_eq!(bytes(0), "0 B");
        assert_eq!(bytes(512), "512 B");
        assert_eq!(bytes(2048), "2.0 KiB");
        assert_eq!(bytes(5 * 1024 * 1024), "5.0 MiB");
        assert_eq!(bytes(3 * 1024 * 1024 * 1024), "3.0 GiB");
    }

    #[test]
    fn long_and_multi_line_errors_are_reduced_to_one_cell() {
        // A build error containing newlines would otherwise destroy the table.
        let multi = "line one\nline two\nline three";
        let rendered = truncate(multi, 20);
        assert!(!rendered.contains('\n'));
        assert!(rendered.chars().count() <= 20);
        assert!(rendered.ends_with('…'));

        assert_eq!(truncate("short", 20), "short");
        assert_eq!(truncate("", 20), "-");
        assert_eq!(truncate("   ", 20), "-");
    }

    #[test]
    fn a_targets_latency_critical_flag_is_visible_in_the_listing() {
        // This is the flag that changes how every other command behaves.
        let rendered = targets_table(&[
            Target {
                id: "tgt_1".to_string(),
                name: "hot-box".to_string(),
                host: "10.0.0.1".to_string(),
                port: 22,
                user: "root".to_string(),
                latency_critical: true,
                status: target::Status::Reachable as i32,
                ..Default::default()
            },
            Target {
                id: "tgt_2".to_string(),
                name: "normal".to_string(),
                host: "10.0.0.2".to_string(),
                port: 2222,
                user: "deploy".to_string(),
                latency_critical: false,
                ..Default::default()
            },
        ]);

        assert!(rendered.contains("LATENCY-CRITICAL"));
        assert!(rendered.contains("root@10.0.0.1:22"));
        assert!(rendered.contains("deploy@10.0.0.2:2222"));
        assert!(rendered.contains("reachable"));

        let hot = rendered
            .lines()
            .find(|l| l.contains("hot-box"))
            .expect("row");
        assert!(hot.contains("yes"));
        let normal = rendered
            .lines()
            .find(|l| l.contains("normal"))
            .expect("row");
        assert!(!normal.contains("yes"));
    }

    #[test]
    fn a_never_deployed_service_says_so_rather_than_showing_an_empty_cell() {
        let rendered = services_table(&[Service {
            id: "svc_1".to_string(),
            name: "bot".to_string(),
            target_id: "tgt_1".to_string(),
            current_release_id: String::new(),
            ..Default::default()
        }]);
        assert!(rendered.contains("never deployed"));
    }

    #[test]
    fn each_artifact_kind_has_a_readable_summary() {
        use nudo_proto::{ArtifactSource, GitSource, artifact_source::Kind};

        let with = |kind: Kind| {
            artifact_summary(&Service {
                artifact: Some(ArtifactSource { kind: Some(kind) }),
                ..Default::default()
            })
        };

        assert_eq!(with(Kind::Url("https://x/bot".to_string())), "url");
        assert_eq!(
            with(Kind::Git(GitSource {
                repo: "owner/bot".to_string(),
                branch: "main".to_string(),
                ..Default::default()
            })),
            "git:owner/bot@main"
        );
        assert_eq!(with(Kind::DirectUpload(true)), "upload");
        // A service with no artifact set will be pushed to by the CLI.
        assert_eq!(artifact_summary(&Service::default()), "upload");
    }

    #[test]
    fn a_secrets_scope_is_described_and_the_value_never_appears() {
        let rendered = secrets_table(&[
            Secret {
                id: "sec_1".to_string(),
                name: "GLOBAL_KEY".to_string(),
                digest: "abcdef0123456789".to_string(),
                ..Default::default()
            },
            Secret {
                id: "sec_2".to_string(),
                name: "SCOPED_KEY".to_string(),
                scope_target_id: "tgt_1".to_string(),
                digest: "fedcba9876543210".to_string(),
                ..Default::default()
            },
        ]);

        assert!(rendered.contains("global"));
        assert!(rendered.contains("target tgt_1"));
        // Only a digest prefix, and no column that could hold a value.
        assert!(rendered.contains("abcdef012345"));
        assert!(!rendered.to_lowercase().contains("value"));
    }

    #[test]
    fn scope_labels_cover_every_combination() {
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
        // Both set: the narrower scope is what matters.
        assert_eq!(scoped("tgt_1", "svc_1"), "service svc_1");
    }

    #[test]
    fn deployment_history_shows_status_actor_and_timing() {
        let start = chrono::Utc::now() - chrono::Duration::minutes(5);
        let rendered = deployments_table(&[Deployment {
            id: "dep_1".to_string(),
            status: deployment::Status::Succeeded as i32,
            actor: Some(nudo_proto::Actor::human("usr_1", "alice")),
            started_at: Some(nudo_proto::to_timestamp(start)),
            finished_at: Some(nudo_proto::to_timestamp(
                start + chrono::Duration::seconds(30),
            )),
            ..Default::default()
        }]);

        assert!(rendered.contains("succeeded"));
        assert!(rendered.contains("alice"));
        assert!(rendered.contains("30s"));
        assert!(rendered.contains("5m ago"));
    }

    #[test]
    fn a_deployment_with_no_actor_renders_a_dash() {
        let rendered = deployments_table(&[Deployment {
            id: "dep_1".to_string(),
            status: deployment::Status::Queued as i32,
            actor: None,
            ..Default::default()
        }]);
        assert!(rendered.contains("queued"));
    }

    #[test]
    fn releases_show_a_short_sha_and_a_readable_size() {
        let rendered = releases_table(&[Release {
            id: "rel_1".to_string(),
            git_sha: "0123456789abcdef0123456789abcdef01234567".to_string(),
            git_ref: "main".to_string(),
            artifact_bytes: 12 * 1024 * 1024,
            ..Default::default()
        }]);

        assert!(rendered.contains("01234567"));
        // Not the whole 40-character sha.
        assert!(!rendered.contains("0123456789abcdef0123456789"));
        assert!(rendered.contains("12.0 MiB"));
        assert!(rendered.contains("main"));
    }
}
