//! Output formatting. Kept separate so it can be tested without a server.

use nudo_proto::{
    BuildHost, Deployment, Release, Secret, Service, Target, UnitStatus, build_host, deployment,
    target,
};

pub use nudo_format::{ago, artifact_summary, bytes, duration, scope_label, truncate};

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

pub fn build_hosts_table(hosts: &[BuildHost]) -> String {
    let rows: Vec<Vec<String>> = hosts
        .iter()
        .map(|h| {
            vec![
                h.id.clone(),
                h.name.clone(),
                format!("{}@{}:{}", h.user, h.host, h.port),
                h.workspace_root.clone(),
                build_host::Status::try_from(h.status)
                    .unwrap_or(build_host::Status::Unknown)
                    .as_str()
                    .to_string(),
                // Shown for the same reason it is on a target: it changes how
                // every other command against this host behaves, and here it
                // also means a build will contend with whatever else runs.
                if h.latency_critical {
                    "yes".to_string()
                } else {
                    "-".to_string()
                },
                ago(h.last_seen_at.as_ref()),
            ]
        })
        .collect();

    table(
        &[
            "id",
            "name",
            "address",
            "workspace",
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

/// Renders a unit's live state as a single line.
pub fn unit_status_line(status: &UnitStatus) -> String {
    let mut line = format!(
        "{}  {}",
        crate::format_status_badge(status),
        nudo_format::unit_state_label(status)
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
    fn a_build_host_listing_shows_its_workspace_and_contention_flag() {
        let rendered = build_hosts_table(&[
            BuildHost {
                id: "bh_1".to_string(),
                name: "spare-box".to_string(),
                host: "10.0.0.9".to_string(),
                port: 22,
                user: "build".to_string(),
                workspace_root: "/mnt/fast/builds".to_string(),
                latency_critical: true,
                status: build_host::Status::Reachable as i32,
                ..Default::default()
            },
            BuildHost {
                id: "bh_2".to_string(),
                name: "ci-runner".to_string(),
                host: "10.0.0.10".to_string(),
                port: 2222,
                user: "ci".to_string(),
                workspace_root: "/var/lib/nudo/builds".to_string(),
                ..Default::default()
            },
        ]);

        assert!(rendered.contains("WORKSPACE"));
        assert!(rendered.contains("/mnt/fast/builds"));
        assert!(rendered.contains("build@10.0.0.9:22"));
        assert!(rendered.contains("ci@10.0.0.10:2222"));
        assert!(rendered.contains("reachable"));

        // The contention flag has to be readable at a glance, for the same
        // reason it is on a target listing.
        let hot = rendered
            .lines()
            .find(|l| l.contains("spare-box"))
            .expect("row");
        assert!(hot.contains("yes"));
        let plain = rendered
            .lines()
            .find(|l| l.contains("ci-runner"))
            .expect("row");
        assert!(!plain.contains("yes"));
    }

    #[test]
    fn an_empty_build_host_listing_renders_nothing_so_the_caller_can_say_none() {
        assert!(build_hosts_table(&[]).trim().is_empty());
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
