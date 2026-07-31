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
use tokio_stream::StreamExt;

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
    /// One lazily-connected channel, shared by every tool call in the session.
    client: nudo_client::Client,
    /// Identifies this agent session in the audit log.
    session_id: String,
    /// The label an operator sees in the audit log.
    agent_label: String,
    /// Read by the `#[tool_handler]` macro's generated dispatch, not by this
    /// module directly.
    #[allow(dead_code)]
    tool_router: rmcp::handler::server::router::tool::ToolRouter<Self>,
}

mod model;
mod presentation;

pub use model::*;
pub use presentation::*;

// ---------------------------------------------------------------------------
// The tools
// ---------------------------------------------------------------------------

#[tool_router]
impl NudoTools {
    /// Fails only on an endpoint that is not a URL, so a misconfigured server
    /// refuses to start rather than failing identically on every tool call.
    pub fn new(
        endpoint: impl Into<String>,
        agent_label: impl Into<String>,
    ) -> anyhow::Result<Self> {
        Ok(Self {
            // No API token: the MCP server has no option to carry one yet.
            client: nudo_client::Client::new(endpoint, None)?,
            session_id: session_id(),
            agent_label: agent_label.into(),
            tool_router: Self::tool_router(),
        })
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
        let mut client = self.client.targets();

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
                host_key_change_pending: target
                    .host_key
                    .as_ref()
                    .is_some_and(|key| !key.pending_key.is_empty()),
                labels: target.labels.into_iter().collect(),
            })
            .collect();

        Ok(Json(TargetList {
            count: targets.len(),
            targets,
        }))
    }

    /// Lists build hosts.
    ///
    /// Read-only, like `list_targets`: registering infrastructure is not an
    /// agent action. This is here so an agent can say where a build ran, or why
    /// one failed, rather than inferring it.
    #[tool(
        description = "List the machines that run builds, when builds do not run on the \
                       control plane. A build host is never deployed to — that is what \
                       targets are — and is not a sandbox: builds on one are not isolated \
                       from each other. The response also reports which build host is the \
                       instance default; empty means builds run on the control plane."
    )]
    pub async fn list_build_hosts(
        &self,
        Parameters(params): Parameters<ListBuildHostsParams>,
    ) -> Result<Json<BuildHostList>, ErrorData> {
        let mut client = self.client.build_hosts();

        let response = client
            .list(ListBuildHostsRequest {
                label_selector: params.label_selector.unwrap_or_default(),
                page_size: 200,
                page_token: String::new(),
            })
            .await
            .map_err(status_to_error)?
            .into_inner();

        let build_hosts: Vec<BuildHostSummary> = response
            .build_hosts
            .into_iter()
            .map(|host| BuildHostSummary {
                id: host.id,
                name: host.name,
                host: host.host,
                reachability: build_host::Status::try_from(host.status)
                    .unwrap_or(build_host::Status::Unknown)
                    .as_str()
                    .to_string(),
                workspace_root: host.workspace_root,
                latency_critical: host.latency_critical,
                host_key_change_pending: host
                    .host_key
                    .as_ref()
                    .is_some_and(|key| !key.pending_key.is_empty()),
                labels: host.labels.into_iter().collect(),
            })
            .collect();

        // Reported alongside the list because a build host means little without
        // it: an agent asked where a service builds needs both.
        let default_build_host_id = client
            .get_defaults(GetBuildDefaultsRequest {})
            .await
            .map(|response| response.into_inner().build_host_id)
            .unwrap_or_default();

        Ok(Json(BuildHostList {
            count: build_hosts.len(),
            build_hosts,
            default_build_host_id,
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
        let mut services_client = self.client.services();
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
            let mut client = self.client.targets();
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
                    source: nudo_format::artifact_description(&service),
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
        let mut client = self.client.services();

        let status = client
            .get_unit_status(GetUnitStatusRequest {
                service_id: params.service_id,
            })
            .await
            .map_err(status_to_error)?
            .into_inner();

        Ok(Json(UnitStatusSummary {
            state: nudo_format::unit_state_label(&status).to_string(),
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
        let mut client = self.client.deployments();

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
        let mut client = self.client.deployments();

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
        let mut client = self.client.logs();

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
                    level: nudo_format::journal_priority_label(&line.priority).to_string(),
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
        let mut client = self.client.logs();

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
        let mut client = self.client.deployments();

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
        // An unreachable control plane used to be reported by the dial, which
        // named it. Now the failure arrives here as a transport status whose
        // message is "transport error", which tells an agent nothing — so this
        // is where it gets named again.
        tonic::Code::Unavailable => ErrorData::internal_error(
            format!(
                "the nudo control plane is not reachable: {}",
                status.message()
            ),
            None,
        ),
        _ => ErrorData::internal_error(status.message().to_string(), None),
    }
}

/// A random identifier for one MCP agent session.
fn session_id() -> String {
    format!("mcp-{}", uuid::Uuid::new_v4().simple())
}

#[cfg(test)]
mod tests;
