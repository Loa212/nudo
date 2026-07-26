//! The MCP server: the control plane as a tool set for an LLM agent.
//!
//! This is a curated surface, not a mechanical mapping of every RPC. An agent
//! does not need `UpdateTarget` with a field mask; it needs to see what exists,
//! deploy, roll back, read logs, and run a command. Anything that would be
//! dangerous or incoherent for an agent to drive — creating targets, editing
//! secrets, holding an interactive PTY — is deliberately absent.
//!
//! Three rules shape every mutating tool:
//!
//! 1. `dry_run` exists on all of them and **defaults to true** on the
//!    destructive ones, so a mistaken call reports a plan instead of acting.
//! 2. A latency-critical target is refused unless the call explicitly opts in.
//!    The server enforces this too; asking here as well means the agent gets a
//!    clear error rather than a generic failure.
//! 3. Every call is attributed to the agent in the audit log, so an operator can
//!    see what the agent did and when.

use nudo_proto::*;
use rmcp::handler::server::wrapper::{Json, Parameters};
use rmcp::model::{ErrorData, ServerCapabilities, ServerInfo};
use rmcp::{ServerHandler, tool, tool_handler, tool_router};
use serde::{Deserialize, Serialize};
use tokio_stream::StreamExt;
use tonic::transport::Channel;

/// How many log lines a single `stream_logs` call will return.
///
/// An agent's context is finite, and a tool that can return a hundred thousand
/// lines is a tool that will fill it. The cap is applied here rather than trusted
/// to the caller.
const MAX_LOG_LINES: u32 = 500;

/// How long a `run_command` call may take.
const DEFAULT_COMMAND_TIMEOUT: u32 = 60;

/// The MCP server's shared state.
#[derive(Clone)]
pub struct NudoTools {
    endpoint: String,
    /// Identifies this agent session in the audit log.
    session_id: String,
    /// The label an operator sees in the audit log.
    agent_label: String,
    /// Read by the `#[tool_handler]` macro's generated dispatch, not by this
    /// module directly.
    #[allow(dead_code)]
    tool_router: rmcp::handler::server::router::tool::ToolRouter<Self>,
}

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

// ---------------------------------------------------------------------------
// The tools
// ---------------------------------------------------------------------------

#[tool_router]
impl NudoTools {
    pub fn new(endpoint: impl Into<String>, agent_label: impl Into<String>) -> Self {
        Self {
            endpoint: endpoint.into(),
            session_id: format!("mcp-{}", uuid_like()),
            agent_label: agent_label.into(),
            tool_router: Self::tool_router(),
        }
    }

    /// Lists the machines this control plane deploys to.
    ///
    /// Start here. Every other tool takes an id from this or from
    /// `list_services`. Note `latency_critical` on each target: those hosts run
    /// latency-sensitive workloads and every mutating tool refuses them unless
    /// you explicitly opt in.
    #[tool(
        description = "List the machines (targets) this control plane deploys to, with their \
                       reachability and labels. Start here: other tools take ids from this. \
                       Targets marked latency_critical run workloads that cannot tolerate an \
                       unattended restart, and mutating tools refuse them without an explicit \
                       opt-in."
    )]
    pub async fn list_targets(
        &self,
        Parameters(params): Parameters<ListTargetsParams>,
    ) -> Result<Json<TargetList>, ErrorData> {
        let mut client = self.targets().await?;

        let response = client
            .list(ListTargetsRequest {
                label_selector: params.label_selector.unwrap_or_default(),
                page_size: 200,
                page_token: String::new(),
            })
            .await
            .map_err(status_to_error)?
            .into_inner();

        let targets: Vec<TargetSummary> = response
                .targets
                .into_iter()
                .map(|target| TargetSummary {
                    id: target.id,
                    name: target.name,
                    host: target.host,
                    reachability: target::Status::try_from(target.status)
                        .unwrap_or(target::Status::Unknown)
                        .as_str()
                        .to_string(),
                    latency_critical: target.latency_critical,
                    labels: target.labels.into_iter().collect(),
                })
                .collect();

        Ok(Json(TargetList {
            count: targets.len(),
            targets,
        }))
    }

    /// Lists deployable services.
    #[tool(
        description = "List the deployable services (each is one systemd unit on one target), \
                       where each gets its binary from, and which release is currently live. \
                       Pass target_id to narrow to one machine."
    )]
    pub async fn list_services(
        &self,
        Parameters(params): Parameters<ListServicesParams>,
    ) -> Result<Json<ServiceList>, ErrorData> {
        let mut services_client = self.services().await?;
        let response = services_client
            .list(ListServicesRequest {
                target_id: params.target_id.unwrap_or_default(),
                page_size: 200,
                page_token: String::new(),
            })
            .await
            .map_err(status_to_error)?
            .into_inner();

        // Resolved once so each summary can name its target and mirror the
        // latency-critical flag, saving the agent a second call.
        let targets: std::collections::HashMap<String, Target> = {
            let mut client = self.targets().await?;
            client
                .list(ListTargetsRequest {
                    page_size: 200,
                    ..Default::default()
                })
                .await
                .map(|response| {
                    response
                        .into_inner()
                        .targets
                        .into_iter()
                        .map(|target| (target.id.clone(), target))
                        .collect()
                })
                .unwrap_or_default()
        };

        let services: Vec<ServiceSummary> = response
                .services
                .into_iter()
                .map(|service| {
                    let target = targets.get(&service.target_id);
                    ServiceSummary {
                        source: describe_artifact(&service),
                        target_name: target
                            .map(|t| t.name.clone())
                            .unwrap_or_else(|| "(unknown)".to_string()),
                        target_latency_critical: target.is_some_and(|t| t.latency_critical),
                        id: service.id,
                        name: service.name,
                        target_id: service.target_id,
                        current_release_id: service.current_release_id,
                    }
                })
                .collect();

        Ok(Json(ServiceList {
            count: services.len(),
            services,
        }))
    }

    /// Reads a service's live systemd state.
    #[tool(
        description = "Read a service's live systemd state: whether it is running, its PID, \
                       memory use, how many times it has restarted, and how long it has been \
                       up. Use this to check whether a service is actually healthy, rather \
                       than inferring it from a deployment's status."
    )]
    pub async fn get_unit_status(
        &self,
        Parameters(params): Parameters<ServiceParams>,
    ) -> Result<Json<UnitStatusSummary>, ErrorData> {
        let mut client = self.services().await?;

        let status = client
            .get_unit_status(GetUnitStatusRequest {
                service_id: params.service_id,
            })
            .await
            .map_err(status_to_error)?
            .into_inner();

        Ok(Json(UnitStatusSummary {
            state: describe_unit_state(&status).to_string(),
            healthy: status.active_state == "active" && status.sub_state != "dead",
            enabled_at_boot: status.enabled,
            running_since: status
                .since
                .as_ref()
                .and_then(nudo_proto::from_timestamp)
                .map(|t| t.to_rfc3339()),
            service_id: status.service_id,
            pid: status.pid,
            memory_bytes: status.memory_bytes,
            restart_count: status.restart_count,
        }))
    }

    /// Deploys a service.
    #[tool(
        description = "Deploy a service: build or fetch its binary, ship it to the target as a \
                       new release, swap the 'current' symlink, restart the unit and verify it \
                       came back healthy. A failed health check automatically rolls back to the \
                       previous release. \
                       \
                       dry_run defaults to TRUE — call it that way first to see the plan, then \
                       call again with dry_run false to actually deploy. Deploying to a \
                       latency_critical target additionally requires allow_latency_critical, \
                       which you should set only when a human has sanctioned it."
    )]
    pub async fn deploy(
        &self,
        Parameters(params): Parameters<DeployParams>,
    ) -> Result<Json<DeployResult>, ErrorData> {
        let mut client = self.deployments().await?;

        let response = client
            .deploy(DeployRequest {
                mutation: Some(self.mutation(params.dry_run, params.allow_latency_critical)),
                service_id: params.service_id.clone(),
                git_ref: params.git_ref.unwrap_or_default(),
                artifact_url: String::new(),
                skip_health_check: false,
                // An agent-initiated deploy is unattended by definition, so an
                // unhealthy release must put the previous one back.
                auto_rollback_on_failure: true,
            })
            .await
            .map_err(status_to_error)?
            .into_inner();

        let summary = if params.dry_run {
            format!(
                "Dry run: nothing was changed. A real deploy of {} would ship a new release \
                 and restart the unit.",
                params.service_id
            )
        } else {
            format!("Deployment {} is queued and running.", response.id)
        };

        Ok(Json(DeployResult {
            dry_run: params.dry_run,
            summary,
            deployment_id: response.id.clone(),
            would_roll_back_to: response.previous_release_id,
            next_step: if params.dry_run {
                "Call deploy again with dry_run false to carry this out.".to_string()
            } else {
                "Call list_deployments with this service_id to see whether it succeeded."
                    .to_string()
            },
        }))
    }

    /// Rolls a service back to a previous release.
    #[tool(
        description = "Roll a service back to a previously deployed release by re-pointing the \
                       'current' symlink and restarting. Omit release_id to go back one \
                       release, which is what you want after a bad deploy. Only releases still \
                       retained on the target can be selected. \
                       \
                       dry_run defaults to TRUE. A latency_critical target requires \
                       allow_latency_critical."
    )]
    pub async fn rollback(
        &self,
        Parameters(params): Parameters<RollbackParams>,
    ) -> Result<Json<DeployResult>, ErrorData> {
        let mut client = self.deployments().await?;

        let response = client
            .rollback(RollbackRequest {
                mutation: Some(self.mutation(params.dry_run, params.allow_latency_critical)),
                service_id: params.service_id.clone(),
                release_id: params.release_id.unwrap_or_default(),
            })
            .await
            .map_err(status_to_error)?
            .into_inner();

        let summary = if params.dry_run {
            format!(
                "Dry run: nothing was changed. A real rollback would activate release {}.",
                response.release_id
            )
        } else {
            format!(
                "Rolling {} back to release {}.",
                params.service_id, response.release_id
            )
        };

        Ok(Json(DeployResult {
            dry_run: params.dry_run,
            summary,
            deployment_id: response.id,
            would_roll_back_to: response.release_id,
            next_step: if params.dry_run {
                "Call rollback again with dry_run false to carry this out.".to_string()
            } else {
                "Call get_unit_status to confirm the service came back.".to_string()
            },
        }))
    }

    /// Reads recent log lines from a service's journal.
    #[tool(
        description = "Read recent lines from a service's systemd journal on the target. Read \
                       only: safe to call freely. Pass grep to filter — an unfiltered read of a \
                       busy service is mostly noise and will fill your context. At most 500 \
                       lines are returned."
    )]
    pub async fn stream_logs(
        &self,
        Parameters(params): Parameters<StreamLogsParams>,
    ) -> Result<Json<LogsResult>, ErrorData> {
        let requested = params.lines.unwrap_or(100).min(MAX_LOG_LINES);
        let mut client = self.logs().await?;

        let response = client
            .stream(StreamLogsRequest {
                service_id: params.service_id.clone(),
                // Never follow: a tool call has to return.
                follow: false,
                tail_lines: requested,
                since_cursor: String::new(),
                since: None,
                grep: params.grep.unwrap_or_default(),
            })
            .await
            .map_err(status_to_error)?;

        let mut stream = response.into_inner();
        let mut lines = Vec::new();

        // Bounded in both count and time: a wedged target must not hang the
        // agent's tool call.
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(20);
        while lines.len() < requested as usize {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                break;
            }
            match tokio::time::timeout(remaining, stream.next()).await {
                Ok(Some(Ok(line))) => lines.push(LogLineSummary {
                    at: line
                        .at
                        .as_ref()
                        .and_then(nudo_proto::from_timestamp)
                        .map(|t| t.to_rfc3339())
                        .unwrap_or_default(),
                    level: describe_priority(&line.priority).to_string(),
                    message: line.message,
                }),
                _ => break,
            }
        }

        Ok(Json(LogsResult {
            service_id: params.service_id,
            returned: lines.len(),
            truncated: lines.len() >= requested as usize,
            lines,
        }))
    }

    /// Runs one command on a target.
    #[tool(
        description = "Run a single command on a target over SSH and return its output and exit \
                       code. Pass the program in `command` and each argument separately in \
                       `args` — this is not a shell line, and nothing is re-parsed by a shell. \
                       \
                       dry_run defaults to TRUE, so call it once to see exactly what would run. \
                       A latency_critical target requires allow_latency_critical. Use this for \
                       diagnosis (systemctl status, ss -ltnp, df -h); use deploy to change what \
                       is running."
    )]
    pub async fn run_command(
        &self,
        Parameters(params): Parameters<RunCommandParams>,
    ) -> Result<Json<CommandResult>, ErrorData> {
        let mut client = self.logs().await?;

        let response = client
            .run_command(RunCommandRequest {
                mutation: Some(self.mutation(params.dry_run, params.allow_latency_critical)),
                target_id: params.target_id,
                command: params.command.clone(),
                args: params.args.clone(),
                timeout_seconds: params
                    .timeout_seconds
                    .unwrap_or(DEFAULT_COMMAND_TIMEOUT)
                    .min(600),
            })
            .await
            .map_err(status_to_error)?;

        let mut stream = response.into_inner();
        let mut stdout = String::new();
        let mut stderr = String::new();
        let mut exit_code = -1;

        while let Some(Ok(chunk)) = stream.next().await {
            match chunk.chunk {
                Some(command_output::Chunk::Stdout(bytes)) => {
                    stdout.push_str(&String::from_utf8_lossy(&bytes));
                }
                Some(command_output::Chunk::Stderr(bytes)) => {
                    stderr.push_str(&String::from_utf8_lossy(&bytes));
                }
                Some(command_output::Chunk::ExitCode(code)) => exit_code = code,
                None => {}
            }

            // An agent's context is finite; a command that dumps megabytes is
            // truncated rather than allowed to fill it.
            const MAX_OUTPUT: usize = 64 * 1024;
            if stdout.len() + stderr.len() > MAX_OUTPUT {
                stdout.push_str("\n[output truncated]\n");
                break;
            }
        }

        Ok(Json(CommandResult {
            dry_run: params.dry_run,
            command: describe_command(&params.command, &params.args),
            exit_code,
            stdout,
            stderr,
        }))
    }

    /// Lists deployment history.
    #[tool(
        description = "List deployment history, newest first: what was deployed, by whom, \
                       whether it succeeded, and the error if it did not. Read only. Use this \
                       after a deploy to find out how it ended."
    )]
    pub async fn list_deployments(
        &self,
        Parameters(params): Parameters<ListDeploymentsParams>,
    ) -> Result<Json<DeploymentList>, ErrorData> {
        let mut client = self.deployments().await?;

        let response = client
            .list(ListDeploymentsRequest {
                service_id: params.service_id.unwrap_or_default(),
                page_size: params.limit.unwrap_or(20).min(100),
                page_token: String::new(),
            })
            .await
            .map_err(status_to_error)?
            .into_inner();

        let deployments: Vec<DeploymentSummary> = response
                .deployments
                .into_iter()
                .map(|deployment| {
                    let status = deployment::Status::try_from(deployment.status)
                        .unwrap_or(deployment::Status::Unspecified);
                    DeploymentSummary {
                        id: deployment.id,
                        service_id: deployment.service_id,
                        release_id: deployment.release_id,
                        status: status.as_str().to_string(),
                        finished: status.is_terminal(),
                        actor: deployment
                            .actor
                            .map(|actor| actor.label)
                            .unwrap_or_default(),
                        error: deployment.error,
                        started_at: deployment
                            .started_at
                            .as_ref()
                            .and_then(nudo_proto::from_timestamp)
                            .map(|t| t.to_rfc3339()),
                        finished_at: deployment
                            .finished_at
                            .as_ref()
                            .and_then(nudo_proto::from_timestamp)
                            .map(|t| t.to_rfc3339()),
                    }
                })
                .collect();

        Ok(Json(DeploymentList {
            count: deployments.len(),
            deployments,
        }))
    }
}

#[tool_handler]
impl ServerHandler for NudoTools {
    fn get_info(&self) -> ServerInfo {
        // ServerInfo is non-exhaustive, so it is built from its default and
        // then adjusted rather than constructed with a struct literal.
        let mut info = ServerInfo::default();
        info.capabilities = ServerCapabilities::builder().enable_tools().build();
        // Named so an operator reading an agent host's tool list sees which
        // system these tools belong to, rather than the library's default.
        info.server_info.name = "nudo".to_string();
        info.server_info.version = env!("CARGO_PKG_VERSION").to_string();
        info.instructions = Some(
                "nudo deploys plain binaries to remote machines over SSH and manages them \
                 with systemd. Targets are machines; services are systemd units on them.\n\n\
                 Work in this order: list_targets, then list_services, then get_unit_status \
                 or stream_logs to understand the current state before changing anything.\n\n\
                 Mutating tools (deploy, rollback, run_command) default to dry_run TRUE. Call \
                 them once as a dry run, read the plan back to the operator, and only then \
                 call again with dry_run false.\n\n\
                 Targets marked latency_critical run workloads that cannot tolerate an \
                 unattended restart — a trading system, a latency-sensitive service. Mutating \
                 them requires allow_latency_critical, and you should set it only when a human \
                 has explicitly sanctioned that specific action. Do not set it merely to clear \
                 an error.\n\n\
                 There is no tool to create or edit targets, services or secrets, and no \
                 interactive shell. Those are deliberately left to a human using the dashboard \
                 or the CLI."
                    .to_string(),
        );
        info
    }
}

impl NudoTools {
    /// The mutation envelope, attributing the call to this agent session.
    fn mutation(&self, dry_run: bool, allow_latency_critical: bool) -> Mutation {
        Mutation {
            actor: Some(Actor::agent(
                self.session_id.clone(),
                self.agent_label.clone(),
            )),
            dry_run,
            allow_latency_critical,
            idempotency_key: String::new(),
        }
    }

    async fn channel(&self) -> Result<Channel, ErrorData> {
        Channel::from_shared(self.endpoint.clone())
            .map_err(|error| {
                ErrorData::internal_error(format!("bad gRPC endpoint: {error}"), None)
            })?
            .connect()
            .await
            .map_err(|error| {
                ErrorData::internal_error(
                    format!(
                        "the nudo control plane at {} is not reachable: {error}",
                        self.endpoint
                    ),
                    None,
                )
            })
    }

    async fn targets(&self) -> Result<targets_client::TargetsClient<Channel>, ErrorData> {
        Ok(targets_client::TargetsClient::new(self.channel().await?))
    }

    async fn services(
        &self,
    ) -> Result<services_api_client::ServicesApiClient<Channel>, ErrorData> {
        Ok(services_api_client::ServicesApiClient::new(
            self.channel().await?,
        ))
    }

    async fn deployments(
        &self,
    ) -> Result<deployments_client::DeploymentsClient<Channel>, ErrorData> {
        Ok(deployments_client::DeploymentsClient::new(
            self.channel().await?,
        ))
    }

    async fn logs(&self) -> Result<logs_client::LogsClient<Channel>, ErrorData> {
        Ok(logs_client::LogsClient::new(self.channel().await?))
    }
}

/// Turns a gRPC status into an MCP error the agent can act on.
///
/// The message is preserved because the server's messages are written to be read
/// — the latency-critical refusal in particular tells the agent exactly which
/// field to set if the action is sanctioned.
fn status_to_error(status: tonic::Status) -> ErrorData {
    match status.code() {
        tonic::Code::NotFound => ErrorData::invalid_params(status.message().to_string(), None),
        tonic::Code::InvalidArgument => {
            ErrorData::invalid_params(status.message().to_string(), None)
        }
        // This is the guardrail. Surfaced as an invalid-params error rather than
        // an internal one, because it is the request that needs changing.
        tonic::Code::FailedPrecondition => {
            ErrorData::invalid_params(status.message().to_string(), None)
        }
        _ => ErrorData::internal_error(status.message().to_string(), None),
    }
}

/// A one-line description of where a service's binary comes from.
pub fn describe_artifact(service: &Service) -> String {
    match service.artifact.as_ref().and_then(|a| a.kind.as_ref()) {
        Some(artifact_source::Kind::Url(url)) if !url.is_empty() => format!("url:{url}"),
        Some(artifact_source::Kind::Git(git)) => {
            if git.branch.is_empty() {
                format!("git:{}", git.repo)
            } else {
                format!("git:{}@{}", git.repo, git.branch)
            }
        }
        _ => "upload".to_string(),
    }
}

/// A word for a unit's state, so an agent does not have to interpret
/// systemd's two-field vocabulary.
pub fn describe_unit_state(status: &UnitStatus) -> &'static str {
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

/// A level name for a journald numeric priority.
pub fn describe_priority(priority: &str) -> &'static str {
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

/// Renders a command and its arguments for display, quoting anything that
/// contains whitespace or a shell metacharacter.
///
/// For the agent's benefit only — the server quotes independently before the
/// command reaches a target.
pub fn describe_command(command: &str, args: &[String]) -> String {
    let quote = |value: &str| -> String {
        if value.is_empty()
            || value
                .chars()
                .any(|c| c.is_whitespace() || "';|&$`\"\\<>()*?[]{}!#~".contains(c))
        {
            format!("'{}'", value.replace('\'', r"'\''"))
        } else {
            value.to_string()
        }
    };

    let mut rendered = quote(command);
    for arg in args {
        rendered.push(' ');
        rendered.push_str(&quote(arg));
    }
    rendered
}

/// A random-enough session id, without pulling in the uuid crate for one use.
fn uuid_like() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or_default();
    format!("{nanos:x}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tools() -> NudoTools {
        NudoTools::new("http://127.0.0.1:50051", "claude (mcp)")
    }

    #[test]
    fn the_tool_set_is_curated_rather_than_a_mapping_of_every_rpc() {
        let router = NudoTools::tool_router();
        let names: Vec<String> = router
            .list_all()
            .iter()
            .map(|tool| tool.name.to_string())
            .collect();

        // The eight the plan calls for.
        for expected in [
            "list_targets",
            "list_services",
            "get_unit_status",
            "deploy",
            "rollback",
            "stream_logs",
            "run_command",
            "list_deployments",
        ] {
            assert!(names.contains(&expected.to_string()), "missing {expected}");
        }
        assert_eq!(names.len(), 8, "the surface should stay curated: {names:?}");
    }

    #[test]
    fn nothing_that_should_be_left_to_a_human_is_exposed() {
        let router = NudoTools::tool_router();
        let names: Vec<String> = router
            .list_all()
            .iter()
            .map(|tool| tool.name.to_string())
            .collect();

        // Creating infrastructure, editing secrets and holding a PTY are
        // deliberately absent: an agent should not be able to do them at all,
        // rather than being trusted not to.
        for forbidden in [
            "create_target",
            "delete_target",
            "create_service",
            "delete_service",
            "put_secret",
            "delete_secret",
            "attach",
            "terminal",
            "create_terminal_session",
        ] {
            assert!(
                !names.iter().any(|name| name.contains(forbidden)),
                "{forbidden} must not be exposed to an agent"
            );
        }
    }

    #[test]
    fn every_tool_has_a_description_that_explains_when_to_use_it() {
        // The descriptions are what determine whether an agent uses this
        // correctly, so an empty or perfunctory one is a defect.
        let router = NudoTools::tool_router();
        for tool in router.list_all() {
            let description = tool.description.clone().unwrap_or_default();
            assert!(
                description.len() > 80,
                "{} has a thin description: {description:?}",
                tool.name
            );
        }
    }

    #[test]
    fn the_mutating_tools_warn_about_the_latency_critical_guardrail() {
        let router = NudoTools::tool_router();
        for name in ["deploy", "rollback", "run_command"] {
            let tool = router
                .list_all()
                .into_iter()
                .find(|tool| tool.name == name)
                .expect(name);
            let description = tool.description.clone().unwrap_or_default();
            assert!(
                description.contains("latency_critical"),
                "{name} does not mention the guardrail"
            );
            assert!(
                description.contains("dry_run"),
                "{name} does not mention dry_run"
            );
        }
    }

    #[test]
    fn destructive_tools_default_to_a_dry_run() {
        // A mistaken call must report a plan, not act. Deserializing an empty
        // object is exactly what an agent that omits the field produces.
        let deploy: DeployParams =
            serde_json::from_str(r#"{"service_id":"svc_1"}"#).expect("parse");
        assert!(deploy.dry_run, "deploy must default to a dry run");
        assert!(!deploy.allow_latency_critical, "the guardrail must default to closed");

        let rollback: RollbackParams =
            serde_json::from_str(r#"{"service_id":"svc_1"}"#).expect("parse");
        assert!(rollback.dry_run);
        assert!(!rollback.allow_latency_critical);

        let command: RunCommandParams =
            serde_json::from_str(r#"{"target_id":"tgt_1","command":"uptime"}"#).expect("parse");
        assert!(command.dry_run);
        assert!(!command.allow_latency_critical);
    }

    #[test]
    fn a_deliberate_call_can_turn_the_dry_run_off() {
        let deploy: DeployParams =
            serde_json::from_str(r#"{"service_id":"svc_1","dry_run":false}"#).expect("parse");
        assert!(!deploy.dry_run);
    }

    #[test]
    fn read_only_tools_have_no_dry_run_or_guardrail_field() {
        // They cannot change anything, so those fields would be noise in the
        // schema and would suggest the tool mutates.
        let schema = serde_json::to_value(schemars::schema_for!(StreamLogsParams))
            .expect("schema");
        let text = schema.to_string();
        assert!(!text.contains("dry_run"));
        assert!(!text.contains("allow_latency_critical"));

        let schema = serde_json::to_value(schemars::schema_for!(ListTargetsParams))
            .expect("schema");
        assert!(!schema.to_string().contains("dry_run"));
    }

    #[test]
    fn the_mutation_envelope_attributes_the_call_to_the_agent() {
        // An operator has to be able to see in the audit log that an agent did
        // this, and which session.
        let tools = tools();
        let envelope = tools.mutation(false, false);

        let actor = envelope.actor.expect("actor");
        assert_eq!(actor.kind, actor::Kind::Agent as i32);
        assert_eq!(actor.label, "claude (mcp)");
        assert!(actor.id.starts_with("mcp-"));
    }

    #[test]
    fn the_envelope_carries_the_dry_run_and_guardrail_flags_through() {
        let tools = tools();

        let dry = tools.mutation(true, false);
        assert!(dry.dry_run);
        assert!(!dry.allow_latency_critical);

        let live = tools.mutation(false, true);
        assert!(!live.dry_run);
        assert!(live.allow_latency_critical);
    }

    #[test]
    fn the_server_instructions_tell_an_agent_the_order_to_work_in() {
        let info = tools().get_info();
        let instructions = info.instructions.expect("instructions");

        assert!(instructions.contains("list_targets"));
        assert!(instructions.contains("dry_run"));
        assert!(instructions.contains("latency_critical"));
        // And that some things are not its job.
        assert!(instructions.contains("interactive shell"));
    }

    #[test]
    fn a_guardrail_refusal_reaches_the_agent_as_a_fixable_request_error() {
        // As an internal error it would read as a fault to retry; as invalid
        // params it reads as "change your request", which is correct.
        let error = status_to_error(tonic::Status::failed_precondition(
            "target hft-box is marked latency-critical; set allow_latency_critical on the request",
        ));
        let rendered = format!("{error:?}");
        assert!(rendered.contains("allow_latency_critical"), "got: {rendered}");
    }

    #[test]
    fn a_missing_entity_reaches_the_agent_as_a_request_error_too() {
        let error = status_to_error(tonic::Status::not_found("no such service: svc_x"));
        assert!(format!("{error:?}").contains("no such service"));
    }

    #[test]
    fn artifact_sources_are_described_in_one_line() {
        use nudo_proto::{ArtifactSource, GitSource, artifact_source::Kind};

        let with = |kind: Kind| {
            describe_artifact(&Service {
                artifact: Some(ArtifactSource { kind: Some(kind) }),
                ..Default::default()
            })
        };

        assert_eq!(with(Kind::Url("https://x/bot".to_string())), "url:https://x/bot");
        assert_eq!(
            with(Kind::Git(GitSource {
                repo: "owner/bot".to_string(),
                branch: "main".to_string(),
                ..Default::default()
            })),
            "git:owner/bot@main"
        );
        assert_eq!(with(Kind::DirectUpload(true)), "upload");
        assert_eq!(describe_artifact(&Service::default()), "upload");
    }

    #[test]
    fn unit_states_are_described_in_a_single_word() {
        let state = |active: &str, sub: &str| {
            describe_unit_state(&UnitStatus {
                active_state: active.to_string(),
                sub_state: sub.to_string(),
                ..Default::default()
            })
        };

        assert_eq!(state("active", "running"), "running");
        assert_eq!(state("failed", "failed"), "failed");
        assert_eq!(state("inactive", "dead"), "stopped");
        assert_eq!(state("activating", "start"), "starting");
        assert_eq!(state("unknown", ""), "unreachable");
        assert_eq!(state("something-else", ""), "unknown");
    }

    #[test]
    fn journald_priorities_are_named() {
        assert_eq!(describe_priority("3"), "err");
        assert_eq!(describe_priority("4"), "warning");
        assert_eq!(describe_priority("6"), "info");
        assert_eq!(describe_priority(""), "info");
    }

    #[test]
    fn a_described_command_quotes_arguments_that_need_it() {
        assert_eq!(describe_command("uptime", &[]), "uptime");
        assert_eq!(
            describe_command("systemctl", &["restart".to_string(), "bot.service".to_string()]),
            "systemctl restart bot.service"
        );

        // So the agent can see that an argument is one argument.
        let rendered = describe_command("echo", &["two words".to_string()]);
        assert_eq!(rendered, "echo 'two words'");

        let hostile = describe_command("echo", &["; rm -rf /".to_string()]);
        assert!(hostile.contains("'; rm -rf /'"), "got: {hostile}");
    }

    #[test]
    fn each_result_shape_names_its_enums_rather_than_exposing_wire_integers() {
        // An agent reading `status: 2` has to guess.
        let schema = serde_json::to_value(schemars::schema_for!(DeploymentSummary))
            .expect("schema");
        let status = &schema["properties"]["status"];
        assert_eq!(status["type"], "string");

        let schema = serde_json::to_value(schemars::schema_for!(TargetSummary)).expect("schema");
        assert_eq!(schema["properties"]["reachability"]["type"], "string");
        // And the flag that changes everything is a plain boolean.
        assert_eq!(schema["properties"]["latency_critical"]["type"], "boolean");
    }

    #[test]
    fn a_service_summary_mirrors_its_targets_guardrail_flag() {
        // Otherwise the agent must cross-reference two calls to know whether a
        // deploy needs the opt-in.
        let schema = serde_json::to_value(schemars::schema_for!(ServiceSummary))
            .expect("schema");
        assert!(
            schema["properties"]
                .get("target_latency_critical")
                .is_some(),
            "a service summary must say whether its target is latency-critical"
        );
    }

    #[test]
    fn the_log_cap_is_enforced_by_the_tool_rather_than_trusted_to_the_caller() {
        assert_eq!(MAX_LOG_LINES, 500);
        // The parameter is optional, so an omitted value must still be bounded.
        let params: StreamLogsParams =
            serde_json::from_str(r#"{"service_id":"svc_1"}"#).expect("parse");
        assert!(params.lines.is_none());
        assert_eq!(params.lines.unwrap_or(100).min(MAX_LOG_LINES), 100);

        let huge: StreamLogsParams =
            serde_json::from_str(r#"{"service_id":"svc_1","lines":999999}"#).expect("parse");
        assert_eq!(huge.lines.unwrap_or(100).min(MAX_LOG_LINES), MAX_LOG_LINES);
    }

    #[tokio::test]
    async fn an_unreachable_control_plane_is_reported_rather_than_hanging() {
        let tools = NudoTools::new("http://127.0.0.1:1", "test");
        // `Json<T>` has no Debug, so expect_err cannot be used here.
        let error = match tools
            .list_targets(Parameters(ListTargetsParams {
                label_selector: None,
            }))
            .await
        {
            Ok(_) => panic!("a call against a dead endpoint must not succeed"),
            Err(error) => error,
        };
        assert!(format!("{error:?}").contains("not reachable"));
    }
}
