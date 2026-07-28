use super::*;

/// Prefixes human output during a dry run, so it is never mistaken for a real
/// effect.
pub(super) fn dry_run_prefix(cli: &Cli) -> &'static str {
    if cli.dry_run {
        "dry run: would have "
    } else {
        ""
    }
}

/// A short badge for a unit's state.
pub fn format_status_badge(status: &UnitStatus) -> &'static str {
    match status.active_state.as_str() {
        "active" => "[ok]",
        "activating" | "deactivating" => "[..]",
        "failed" => "[!!]",
        "inactive" => "[--]",
        _ => "[??]",
    }
}

/// Human-readable label for a unit's state.
///
/// Duplicated from the server's `units::status_label` rather than shared,
/// because the CLI is a pure gRPC client and does not depend on the server
/// crate.
pub fn units_label(status: &UnitStatus) -> &'static str {
    nudo_format::unit_state_label(status)
}

// ---------------------------------------------------------------------------
// JSON shapes
// ---------------------------------------------------------------------------
//
// The generated proto types do not derive Serialize, and deriving it on them
// would leak wire details (i32 enums, nested oneofs) into scripts. These
// wrappers are the CLI's stable JSON contract.

#[derive(serde::Serialize)]
pub(super) struct JsonTargets {
    targets: Vec<JsonTarget>,
}

#[derive(serde::Serialize)]
pub(super) struct JsonTarget {
    id: String,
    name: String,
    host: String,
    port: u32,
    user: String,
    status: String,
    latency_critical: bool,
    labels: std::collections::BTreeMap<String, String>,
}

impl From<&Vec<Target>> for JsonTargets {
    fn from(targets: &Vec<Target>) -> Self {
        Self {
            targets: targets
                .iter()
                .map(|t| JsonTarget {
                    id: t.id.clone(),
                    name: t.name.clone(),
                    host: t.host.clone(),
                    port: t.port,
                    user: t.user.clone(),
                    status: target::Status::try_from(t.status)
                        .unwrap_or(target::Status::Unknown)
                        .as_str()
                        .to_string(),
                    latency_critical: t.latency_critical,
                    labels: t
                        .labels
                        .iter()
                        .map(|(k, v)| (k.clone(), v.clone()))
                        .collect(),
                })
                .collect(),
        }
    }
}

#[derive(serde::Serialize)]
pub(super) struct JsonServices {
    services: Vec<JsonService>,
}

#[derive(serde::Serialize)]
pub(super) struct JsonService {
    id: String,
    name: String,
    target_id: String,
    source: String,
    release_root: String,
    keep_releases: u32,
    current_release_id: String,
    secret_count: usize,
}

impl From<&Vec<Service>> for JsonServices {
    fn from(services: &Vec<Service>) -> Self {
        Self {
            services: services
                .iter()
                .map(|s| JsonService {
                    id: s.id.clone(),
                    name: s.name.clone(),
                    target_id: s.target_id.clone(),
                    source: format::artifact_summary(s),
                    release_root: s.release_root.clone(),
                    keep_releases: s.keep_releases,
                    current_release_id: s.current_release_id.clone(),
                    secret_count: s.secret_ids.len(),
                })
                .collect(),
        }
    }
}

#[derive(serde::Serialize)]
pub(super) struct JsonDeployments {
    deployments: Vec<JsonDeployment>,
}

#[derive(serde::Serialize)]
pub(super) struct JsonDeployment {
    id: String,
    service_id: String,
    release_id: String,
    status: String,
    actor: String,
    error: String,
}

impl From<&Vec<Deployment>> for JsonDeployments {
    fn from(deployments: &Vec<Deployment>) -> Self {
        Self {
            deployments: deployments
                .iter()
                .map(|d| JsonDeployment {
                    id: d.id.clone(),
                    service_id: d.service_id.clone(),
                    release_id: d.release_id.clone(),
                    status: deployment::Status::try_from(d.status)
                        .unwrap_or(deployment::Status::Unspecified)
                        .as_str()
                        .to_string(),
                    actor: d
                        .actor
                        .as_ref()
                        .map(|a| a.label.clone())
                        .unwrap_or_default(),
                    error: d.error.clone(),
                })
                .collect(),
        }
    }
}

#[derive(serde::Serialize)]
pub(super) struct JsonReleases {
    releases: Vec<JsonRelease>,
}

#[derive(serde::Serialize)]
pub(super) struct JsonRelease {
    id: String,
    git_sha: String,
    git_ref: String,
    artifact_digest: String,
    artifact_bytes: u64,
    path: String,
}

impl From<&Vec<Release>> for JsonReleases {
    fn from(releases: &Vec<Release>) -> Self {
        Self {
            releases: releases
                .iter()
                .map(|r| JsonRelease {
                    id: r.id.clone(),
                    git_sha: r.git_sha.clone(),
                    git_ref: r.git_ref.clone(),
                    artifact_digest: r.artifact_digest.clone(),
                    artifact_bytes: r.artifact_bytes,
                    path: r.path.clone(),
                })
                .collect(),
        }
    }
}

#[derive(serde::Serialize)]
pub(super) struct JsonSecrets {
    secrets: Vec<JsonSecret>,
}

/// Note the absence of a value field. There is nothing to omit, because the API
/// never returns one.
#[derive(serde::Serialize)]
pub(super) struct JsonSecret {
    id: String,
    name: String,
    scope: String,
    digest: String,
}

impl From<&Vec<Secret>> for JsonSecrets {
    fn from(secrets: &Vec<Secret>) -> Self {
        Self {
            secrets: secrets
                .iter()
                .map(|s| JsonSecret {
                    id: s.id.clone(),
                    name: s.name.clone(),
                    scope: format::scope_label(s),
                    digest: s.digest.clone(),
                })
                .collect(),
        }
    }
}

#[derive(serde::Serialize)]
pub(super) struct JsonUnitStatus {
    service_id: String,
    active_state: String,
    sub_state: String,
    label: String,
    enabled: bool,
    pid: u32,
    memory_bytes: u64,
    restart_count: u32,
}

impl From<&UnitStatus> for JsonUnitStatus {
    fn from(status: &UnitStatus) -> Self {
        Self {
            service_id: status.service_id.clone(),
            active_state: status.active_state.clone(),
            sub_state: status.sub_state.clone(),
            label: units_label(status).to_string(),
            enabled: status.enabled,
            pid: status.pid,
            memory_bytes: status.memory_bytes,
            restart_count: status.restart_count,
        }
    }
}

#[derive(serde::Serialize)]
pub(super) struct JsonChecks {
    ok: bool,
    checks: Vec<JsonCheck>,
}

#[derive(serde::Serialize)]
pub(super) struct JsonCheck {
    name: String,
    ok: bool,
    detail: String,
}

impl From<&CheckTargetResponse> for JsonChecks {
    fn from(response: &CheckTargetResponse) -> Self {
        Self {
            ok: response.ok,
            checks: response
                .checks
                .iter()
                .map(|c| JsonCheck {
                    name: c.name.clone(),
                    ok: c.ok,
                    detail: c.detail.clone(),
                })
                .collect(),
        }
    }
}

#[derive(serde::Serialize)]
pub(super) struct JsonLogLine {
    at: String,
    message: String,
    priority: String,
    unit: String,
    cursor: String,
}

impl From<&LogLine> for JsonLogLine {
    fn from(line: &LogLine) -> Self {
        Self {
            at: line
                .at
                .as_ref()
                .and_then(nudo_proto::from_timestamp)
                .map(|t| t.to_rfc3339())
                .unwrap_or_default(),
            message: line.message.clone(),
            priority: line.priority.clone(),
            unit: line.unit.clone(),
            cursor: line.cursor.clone(),
        }
    }
}

#[derive(serde::Serialize)]
pub(super) struct JsonAudit {
    entries: Vec<JsonAuditEntry>,
}

#[derive(serde::Serialize)]
pub(super) struct JsonAuditEntry {
    id: String,
    at: String,
    actor_kind: String,
    actor: String,
    action: String,
    subject_id: String,
    dry_run: bool,
    summary: String,
}

impl From<&Vec<AuditEntry>> for JsonAudit {
    fn from(entries: &Vec<AuditEntry>) -> Self {
        Self {
            entries: entries
                .iter()
                .map(|e| JsonAuditEntry {
                    id: e.id.clone(),
                    at: e
                        .at
                        .as_ref()
                        .and_then(nudo_proto::from_timestamp)
                        .map(|t| t.to_rfc3339())
                        .unwrap_or_default(),
                    actor_kind: e
                        .actor
                        .as_ref()
                        .map(|a| a.kind_str().to_string())
                        .unwrap_or_default(),
                    actor: e
                        .actor
                        .as_ref()
                        .map(|a| a.label.clone())
                        .unwrap_or_default(),
                    action: e.action.clone(),
                    subject_id: e.subject_id.clone(),
                    dry_run: e.dry_run,
                    summary: e.summary.clone(),
                })
                .collect(),
        }
    }
}

#[derive(serde::Serialize)]
pub(super) struct JsonSources {
    sources: Vec<JsonSource>,
}

#[derive(serde::Serialize)]
pub(super) struct JsonSource {
    id: String,
    name: String,
    kind: String,
    account_login: String,
    installed: bool,
}

impl From<&Vec<Source>> for JsonSources {
    fn from(sources: &Vec<Source>) -> Self {
        Self {
            sources: sources
                .iter()
                .map(|s| JsonSource {
                    id: s.id.clone(),
                    name: s.name.clone(),
                    kind: source::Kind::try_from(s.kind)
                        .unwrap_or(source::Kind::Unspecified)
                        .as_str()
                        .to_string(),
                    account_login: s.account_login.clone(),
                    installed: s.installed,
                })
                .collect(),
        }
    }
}
