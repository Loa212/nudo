use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Tool parameters
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
pub struct ListTargetsParams {
    /// Optional label selector, e.g. "env=prod,role=indexer". Omit to list all.
    #[serde(default)]
    pub label_selector: Option<String>,
}

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
pub struct ListBuildHostsParams {
    /// Optional label selector, e.g. "arch=arm64,pool=ci". Omit to list all.
    #[serde(default)]
    pub label_selector: Option<String>,
}

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
pub struct ListServicesParams {
    /// Only services on this target. Omit to list every service.
    #[serde(default)]
    pub target_id: Option<String>,
}

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
pub struct ServiceParams {
    /// The service's id, as returned by list_services.
    pub service_id: String,
}

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
pub struct DeployParams {
    /// The service to deploy, as returned by list_services.
    pub service_id: String,

    /// A branch, tag or commit sha to build, overriding the service's default.
    /// Omit to use what the service is configured with.
    #[serde(default)]
    pub git_ref: Option<String>,

    /// When true (the default) nothing is changed and the plan is described
    /// instead. Set false only when you intend to deploy for real.
    #[serde(default = "default_true")]
    pub dry_run: bool,

    /// Required to deploy to a target marked latency-critical. Those hosts run
    /// workloads that cannot tolerate an unattended restart, so this must be a
    /// deliberate, human-sanctioned choice — do not set it to work around an
    /// error unless you have been told to.
    #[serde(default)]
    pub allow_latency_critical: bool,
}

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
pub struct RollbackParams {
    /// The service to roll back.
    pub service_id: String,

    /// Which release to activate. Omit to go back to the release before the
    /// current one, which is almost always what you want.
    #[serde(default)]
    pub release_id: Option<String>,

    /// When true (the default) nothing is changed and the plan is described.
    #[serde(default = "default_true")]
    pub dry_run: bool,

    /// Required for a latency-critical target. See deploy.
    #[serde(default)]
    pub allow_latency_critical: bool,
}

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
pub struct StreamLogsParams {
    /// The service whose journal to read.
    pub service_id: String,

    /// How many recent lines to return. Capped at 500.
    #[serde(default)]
    pub lines: Option<u32>,

    /// Only lines containing this text. Use it — an unfiltered read of a busy
    /// service will mostly be noise.
    #[serde(default)]
    pub grep: Option<String>,
}

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
pub struct RunCommandParams {
    /// The target to run on, as returned by list_targets.
    pub target_id: String,

    /// The program to run. Not a shell line: pass arguments separately in `args`
    /// so nothing is re-parsed by a shell.
    pub command: String,

    /// The program's arguments, one per element.
    #[serde(default)]
    pub args: Vec<String>,

    /// Seconds to allow before giving up. Defaults to 60, maximum 600.
    #[serde(default)]
    pub timeout_seconds: Option<u32>,

    /// When true (the default) the command is described rather than run.
    #[serde(default = "default_true")]
    pub dry_run: bool,

    /// Required for a latency-critical target. See deploy.
    #[serde(default)]
    pub allow_latency_critical: bool,
}

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
pub struct ListDeploymentsParams {
    /// Only deployments of this service. Omit for the most recent across all.
    #[serde(default)]
    pub service_id: Option<String>,

    /// How many to return. Defaults to 20.
    #[serde(default)]
    pub limit: Option<u32>,
}

/// serde needs a function for a non-false default.
fn default_true() -> bool {
    true
}

// ---------------------------------------------------------------------------
// Tool results
// ---------------------------------------------------------------------------
//
// Purpose-built shapes rather than the raw proto messages: an agent reading
// `status: 2` has to guess, and a wire-shaped oneof is noise in a context
// window. Every enum is a name and every id is spelled out so it can be passed
// straight to the next call.

/// The MCP specification requires a tool's output schema to have an object at
/// its root, so each listing is wrapped rather than returned as a bare array.
/// The wrapper also gives room to carry a count and a hint alongside the items.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct TargetList {
    pub count: usize,
    pub targets: Vec<TargetSummary>,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct BuildHostList {
    pub count: usize,
    pub build_hosts: Vec<BuildHostSummary>,
    /// Where a build runs when its service does not name a build host. Empty
    /// means the control plane, which is the default.
    pub default_build_host_id: String,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct ServiceList {
    pub count: usize,
    pub services: Vec<ServiceSummary>,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct DeploymentList {
    pub count: usize,
    pub deployments: Vec<DeploymentSummary>,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct TargetSummary {
    pub id: String,
    pub name: String,
    pub host: String,
    pub reachability: String,
    /// True when this host must not be mutated unattended. Every mutating tool
    /// refuses it unless `allow_latency_critical` is set.
    pub latency_critical: bool,
    /// True when this target's SSH host key has changed and is waiting for a
    /// human to review it. Every operation against the host — including
    /// read-only ones — is refused until then, so there is nothing an agent can
    /// usefully do here. Accepting a host key is deliberately not an agent
    /// action: it is a judgement about whether a machine is the one it claims
    /// to be, which needs someone who can check it out of band.
    pub host_key_change_pending: bool,
    pub labels: std::collections::BTreeMap<String, String>,
}

/// A machine that builds, which is never a machine that is deployed to.
///
/// Read-only for an agent, like targets: registering infrastructure is not an
/// agent action. This exists so an agent can explain where a build ran, or why
/// one failed, without guessing.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct BuildHostSummary {
    pub id: String,
    pub name: String,
    pub host: String,
    pub reachability: String,
    /// Where checkouts and build trees go on this host.
    pub workspace_root: String,
    /// True when something latency-sensitive also runs here, so a build will
    /// contend with it for CPU, cache and memory bandwidth. Allowed, but worth
    /// saying when reporting where a build ran.
    pub latency_critical: bool,
    /// True when this host's SSH host key has changed and is waiting for a
    /// human to review it. Nothing builds here until it is resolved, and
    /// accepting a key is deliberately not an agent action.
    pub host_key_change_pending: bool,
    pub labels: std::collections::BTreeMap<String, String>,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct ServiceSummary {
    pub id: String,
    pub name: String,
    pub target_id: String,
    pub target_name: String,
    /// Where the binary comes from: "url", "git:owner/repo@branch" or "upload".
    pub source: String,
    pub current_release_id: String,
    /// Mirrors the target's flag, so an agent does not have to cross-reference.
    pub target_latency_critical: bool,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct UnitStatusSummary {
    pub service_id: String,
    /// One of: running, starting, stopping, failed, stopped, unreachable,
    /// exited cleanly, unknown.
    pub state: String,
    pub healthy: bool,
    pub enabled_at_boot: bool,
    pub pid: u32,
    pub memory_bytes: u64,
    pub restart_count: u32,
    pub running_since: Option<String>,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct DeploymentSummary {
    pub id: String,
    pub service_id: String,
    pub release_id: String,
    /// One of: queued, building, uploading, activating, health_checking,
    /// succeeded, failed, rolled_back, cancelled.
    pub status: String,
    pub finished: bool,
    pub actor: String,
    pub error: String,
    pub started_at: Option<String>,
    pub finished_at: Option<String>,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct DeployResult {
    /// True when nothing was changed.
    pub dry_run: bool,
    /// What happened, or what would happen.
    pub summary: String,
    pub deployment_id: String,
    /// The release a failed deploy would be rolled back to, if any.
    pub would_roll_back_to: String,
    /// How to watch it: pass this deployment_id to list_deployments.
    pub next_step: String,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct LogsResult {
    pub service_id: String,
    pub returned: usize,
    /// True when the cap was hit and older lines exist.
    pub truncated: bool,
    pub lines: Vec<LogLineSummary>,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct LogLineSummary {
    pub at: String,
    /// journald level: emerg, alert, crit, err, warning, notice, info, debug.
    pub level: String,
    pub message: String,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct CommandResult {
    pub dry_run: bool,
    /// The exact command that ran, or would run, with arguments quoted.
    pub command: String,
    pub exit_code: i32,
    pub stdout: String,
    pub stderr: String,
}
