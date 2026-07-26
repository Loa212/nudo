//! `nudo` — the command-line client.
//!
//! A pure gRPC client: every decision, guardrail and side effect lives in the
//! server, so the CLI, the dashboard and the MCP server cannot drift apart.
//! Designed to be usable from CI, which is why every command takes
//! `--output json` and mutating commands take `--idempotency-key`.

mod format;

use std::path::PathBuf;

use anyhow::{Context as _, anyhow, bail};
use clap::{Parser, Subcommand};
use format::Output;
use nudo_proto::*;
use tokio_stream::StreamExt;
use tonic::transport::Channel;

#[derive(Parser)]
#[command(
    name = "nudo",
    about = "Deploy bare-metal binaries over SSH and systemd",
    version
)]
struct Cli {
    /// The control plane's gRPC endpoint.
    #[arg(long, env = "NUDO_ENDPOINT", default_value = "http://127.0.0.1:50051", global = true)]
    endpoint: String,

    /// An API token, for CI use.
    #[arg(long, env = "NUDO_TOKEN", global = true)]
    token: Option<String>,

    /// How to render results.
    #[arg(long, value_enum, default_value_t = Output::Table, global = true)]
    output: Output,

    /// Show what would happen without doing it.
    #[arg(long, global = true)]
    dry_run: bool,

    /// Required to mutate a target marked latency-critical.
    #[arg(long, global = true)]
    allow_latency_critical: bool,

    /// Retrying with the same key returns the original result instead of acting
    /// twice. Set this in CI.
    #[arg(long, global = true)]
    idempotency_key: Option<String>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Write a config file recording the endpoint, so other commands need no flags.
    Init {
        /// Where to write it.
        #[arg(long)]
        path: Option<PathBuf>,
    },
    /// Manage the machines you deploy to.
    #[command(subcommand)]
    Targets(TargetCommand),
    /// Manage deployable units.
    #[command(subcommand)]
    Services(ServiceCommand),
    /// Deploy a service.
    Deploy {
        service: String,
        /// A branch, tag or sha to build, overriding the service's default.
        #[arg(long)]
        git_ref: Option<String>,
        /// A prebuilt binary to fetch instead of building.
        #[arg(long)]
        artifact_url: Option<String>,
        /// Deploy without verifying the service came back healthy.
        #[arg(long)]
        skip_health_check: bool,
        /// Stream progress until the deployment finishes, and exit non-zero if
        /// it fails. This is what you want in CI.
        #[arg(long)]
        wait: bool,
    },
    /// Roll a service back to a previous release.
    Rollback {
        service: String,
        /// Which release. Defaults to the one before the current.
        #[arg(long)]
        release: Option<String>,
        #[arg(long)]
        wait: bool,
    },
    /// Follow a service's logs.
    Logs {
        service: String,
        /// Keep streaming rather than exiting after the backfill.
        #[arg(long, short)]
        follow: bool,
        /// How many past lines to show first.
        #[arg(long, short = 'n', default_value_t = 100)]
        lines: u32,
        /// Only lines containing this text.
        #[arg(long, short)]
        grep: Option<String>,
    },
    /// Run one command on a target.
    ///
    /// Everything after the target is the remote command, including anything
    /// that looks like a flag — so nudo's own global flags must come before
    /// `exec`, e.g. `nudo --allow-latency-critical exec <target> systemctl
    /// restart bot`.
    Exec {
        target: String,
        /// The command and its arguments.
        #[arg(required = true, trailing_var_arg = true, allow_hyphen_values = true)]
        command: Vec<String>,
        #[arg(long, default_value_t = 60)]
        timeout: u32,
    },
    /// Open an interactive shell on a target.
    Terminal {
        target: String,
        /// Run this instead of a login shell.
        #[arg(long)]
        command: Option<String>,
    },
    /// Manage secrets. Values are write-only.
    #[command(subcommand)]
    Secrets(SecretCommand),
    /// Show the audit log.
    Audit {
        /// Only entries about this target, service or deployment.
        #[arg(long)]
        subject: Option<String>,
        #[arg(long, default_value_t = 50)]
        limit: u32,
    },
    /// List connected git sources.
    Sources,
}

#[derive(Subcommand)]
enum TargetCommand {
    List {
        /// e.g. `env=prod,role=indexer`
        #[arg(long)]
        selector: Option<String>,
    },
    Get {
        id: String,
    },
    Add {
        name: String,
        #[arg(long)]
        host: String,
        #[arg(long, default_value_t = 22)]
        port: u32,
        #[arg(long, default_value = "root")]
        user: String,
        /// The secret holding this target's SSH private key.
        #[arg(long)]
        ssh_key: String,
        /// Mark this host as one nothing should touch unattended.
        #[arg(long)]
        latency_critical: bool,
        /// Repeatable, as `key=value`.
        #[arg(long = "label")]
        labels: Vec<String>,
    },
    Remove {
        id: String,
    },
    /// Check SSH, sudo, systemd and a writable release directory.
    Check {
        id: String,
    },
}

#[derive(Subcommand)]
enum ServiceCommand {
    List {
        #[arg(long)]
        target: Option<String>,
    },
    Get {
        id: String,
    },
    /// Print the systemd unit that a deploy would write.
    Unit {
        id: String,
    },
    /// Show the live state of a service's unit.
    Status {
        id: String,
    },
    Start {
        id: String,
    },
    Stop {
        id: String,
    },
    Restart {
        id: String,
    },
    Enable {
        id: String,
    },
    Disable {
        id: String,
    },
    /// Show a service's deployment history.
    Deployments {
        id: String,
        #[arg(long, default_value_t = 20)]
        limit: u32,
    },
    /// Show a service's retained releases.
    Releases {
        id: String,
    },
}

#[derive(Subcommand)]
enum SecretCommand {
    List {
        #[arg(long)]
        target: Option<String>,
        #[arg(long)]
        service: Option<String>,
    },
    /// Store a secret. The value is read from stdin unless --value is given.
    Set {
        name: String,
        /// The value. Prefer stdin so it does not reach your shell history.
        #[arg(long)]
        value: Option<String>,
        #[arg(long)]
        target: Option<String>,
        #[arg(long)]
        service: Option<String>,
    },
    Remove {
        id: String,
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    match run(&cli).await {
        Ok(()) => Ok(()),
        Err(error) => {
            // A gRPC status carries a message meant for a human; printing the
            // whole Debug form would bury it in transport detail.
            eprintln!("error: {error:#}");
            std::process::exit(1);
        }
    }
}

async fn run(cli: &Cli) -> anyhow::Result<()> {
    match &cli.command {
        Command::Init { path } => init(cli, path.as_deref()),
        Command::Targets(command) => targets(cli, command).await,
        Command::Services(command) => services(cli, command).await,
        Command::Deploy {
            service,
            git_ref,
            artifact_url,
            skip_health_check,
            wait,
        } => {
            deploy(
                cli,
                service,
                git_ref.as_deref(),
                artifact_url.as_deref(),
                *skip_health_check,
                *wait,
            )
            .await
        }
        Command::Rollback {
            service,
            release,
            wait,
        } => rollback(cli, service, release.as_deref(), *wait).await,
        Command::Logs {
            service,
            follow,
            lines,
            grep,
        } => logs(cli, service, *follow, *lines, grep.as_deref()).await,
        Command::Exec {
            target,
            command,
            timeout,
        } => exec(cli, target, command, *timeout).await,
        Command::Terminal { target, command } => terminal(cli, target, command.as_deref()).await,
        Command::Secrets(command) => secrets(cli, command).await,
        Command::Audit { subject, limit } => audit(cli, subject.as_deref(), *limit).await,
        Command::Sources => sources(cli).await,
    }
}

// ---------------------------------------------------------------------------
// Connection
// ---------------------------------------------------------------------------

/// Dials the control plane.
async fn channel(cli: &Cli) -> anyhow::Result<Channel> {
    Channel::from_shared(cli.endpoint.clone())
        .with_context(|| format!("{} is not a valid endpoint", cli.endpoint))?
        .connect()
        .await
        .with_context(|| {
            format!(
                "could not reach the control plane at {} — is nudo-server running?",
                cli.endpoint
            )
        })
}

/// Builds the mutation envelope every mutating RPC carries.
fn mutation(cli: &Cli) -> Mutation {
    Mutation {
        actor: Some(Actor::human(
            std::env::var("USER").unwrap_or_else(|_| "cli".to_string()),
            // The label is what the audit log shows, so it names the human and
            // the machine rather than just "cli".
            format!(
                "{}@{} (cli)",
                std::env::var("USER").unwrap_or_else(|_| "unknown".to_string()),
                hostname()
            ),
        )),
        dry_run: cli.dry_run,
        allow_latency_critical: cli.allow_latency_critical,
        idempotency_key: cli.idempotency_key.clone().unwrap_or_default(),
    }
}

/// The local hostname, for audit attribution.
fn hostname() -> String {
    std::env::var("HOSTNAME")
        .ok()
        .or_else(|| {
            std::fs::read_to_string("/etc/hostname")
                .ok()
                .map(|s| s.trim().to_string())
        })
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| "unknown".to_string())
}

/// Attaches the API token, when one was supplied.
fn authenticated<T>(cli: &Cli, message: T) -> tonic::Request<T> {
    let mut request = tonic::Request::new(message);
    if let Some(token) = &cli.token {
        if let Ok(value) = format!("Bearer {token}").parse() {
            request.metadata_mut().insert("authorization", value);
        }
    }
    request
}

/// Prints a value as JSON or via a table renderer.
fn emit<T: serde::Serialize>(cli: &Cli, value: &T, table: impl FnOnce() -> String) {
    match cli.output {
        Output::Json => match serde_json::to_string_pretty(value) {
            Ok(json) => println!("{json}"),
            Err(error) => eprintln!("error: rendering JSON: {error}"),
        },
        Output::Table => {
            let rendered = table();
            if rendered.trim().is_empty() {
                // A blank result should say so rather than printing nothing,
                // which reads as a failure.
                println!("(none)");
            } else {
                print!("{rendered}");
            }
        }
    }
}

// ---------------------------------------------------------------------------
// init
// ---------------------------------------------------------------------------

fn init(cli: &Cli, path: Option<&std::path::Path>) -> anyhow::Result<()> {
    let path = path
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| PathBuf::from(".nudo.toml"));

    let body = format!(
        "# nudo CLI configuration.\n\
         # Set NUDO_ENDPOINT and NUDO_TOKEN in your environment to use these.\n\
         endpoint = \"{}\"\n",
        cli.endpoint
    );

    std::fs::write(&path, body).with_context(|| format!("writing {}", path.display()))?;
    println!("wrote {}", path.display());
    println!();
    println!("Add to your shell or CI environment:");
    println!("  export NUDO_ENDPOINT={}", cli.endpoint);
    println!("  export NUDO_TOKEN=<an API token>");
    Ok(())
}

// ---------------------------------------------------------------------------
// targets
// ---------------------------------------------------------------------------

async fn targets(cli: &Cli, command: &TargetCommand) -> anyhow::Result<()> {
    let mut client = targets_client::TargetsClient::new(channel(cli).await?);

    match command {
        TargetCommand::List { selector } => {
            let response = client
                .list(authenticated(
                    cli,
                    ListTargetsRequest {
                        label_selector: selector.clone().unwrap_or_default(),
                        page_size: 200,
                        page_token: String::new(),
                    },
                ))
                .await?
                .into_inner();

            let targets = response.targets;
            emit(cli, &JsonTargets::from(&targets), || {
                format::targets_table(&targets)
            });
        }

        TargetCommand::Get { id } => {
            let target = client
                .get(authenticated(cli, GetTargetRequest { id: id.clone() }))
                .await?
                .into_inner();
            let list = vec![target];
            emit(cli, &JsonTargets::from(&list), || {
                format::targets_table(&list)
            });
        }

        TargetCommand::Add {
            name,
            host,
            port,
            user,
            ssh_key,
            latency_critical,
            labels,
        } => {
            let target = client
                .create(authenticated(
                    cli,
                    CreateTargetRequest {
                        mutation: Some(mutation(cli)),
                        name: name.clone(),
                        host: host.clone(),
                        port: *port,
                        user: user.clone(),
                        ssh_key_id: ssh_key.clone(),
                        latency_critical: *latency_critical,
                        labels: parse_labels(labels)?,
                    },
                ))
                .await?
                .into_inner();

            let list = vec![target];
            emit(cli, &JsonTargets::from(&list), || {
                format::targets_table(&list)
            });
        }

        TargetCommand::Remove { id } => {
            client
                .delete(authenticated(
                    cli,
                    DeleteTargetRequest {
                        mutation: Some(mutation(cli)),
                        id: id.clone(),
                    },
                ))
                .await?;
            println!("{}removed target {id}", dry_run_prefix(cli));
        }

        TargetCommand::Check { id } => {
            let response = client
                .check(authenticated(cli, CheckTargetRequest { id: id.clone() }))
                .await?
                .into_inner();

            match cli.output {
                Output::Json => println!(
                    "{}",
                    serde_json::to_string_pretty(&JsonChecks::from(&response))?
                ),
                Output::Table => {
                    for check in &response.checks {
                        println!(
                            "{} {:<12} {}",
                            if check.ok { "ok  " } else { "FAIL" },
                            check.name,
                            check.detail
                        );
                    }
                }
            }

            // Non-zero exit so a CI step gating on reachability fails.
            if !response.ok {
                bail!("target {id} is not ready");
            }
        }
    }

    Ok(())
}

/// Parses repeated `key=value` label flags.
fn parse_labels(labels: &[String]) -> anyhow::Result<std::collections::HashMap<String, String>> {
    let mut parsed = std::collections::HashMap::new();
    for label in labels {
        let (key, value) = label
            .split_once('=')
            .ok_or_else(|| anyhow!("label {label:?} must be in key=value form"))?;
        if key.trim().is_empty() {
            bail!("label {label:?} has an empty key");
        }
        parsed.insert(key.trim().to_string(), value.trim().to_string());
    }
    Ok(parsed)
}

// ---------------------------------------------------------------------------
// services
// ---------------------------------------------------------------------------

async fn services(cli: &Cli, command: &ServiceCommand) -> anyhow::Result<()> {
    let mut client = services_api_client::ServicesApiClient::new(channel(cli).await?);

    match command {
        ServiceCommand::List { target } => {
            let response = client
                .list(authenticated(
                    cli,
                    ListServicesRequest {
                        target_id: target.clone().unwrap_or_default(),
                        page_size: 200,
                        page_token: String::new(),
                    },
                ))
                .await?
                .into_inner();

            let services = response.services;
            emit(cli, &JsonServices::from(&services), || {
                format::services_table(&services)
            });
        }

        ServiceCommand::Get { id } => {
            let service = client
                .get(authenticated(cli, GetServiceRequest { id: id.clone() }))
                .await?
                .into_inner();
            let list = vec![service];
            emit(cli, &JsonServices::from(&list), || {
                format::services_table(&list)
            });
        }

        ServiceCommand::Unit { id } => {
            let response = client
                .render_unit(authenticated(
                    cli,
                    RenderUnitRequest {
                        service_id: id.clone(),
                    },
                ))
                .await?
                .into_inner();
            // Printed raw so it can be piped to a file or diffed.
            print!("{}", response.unit_file);
        }

        ServiceCommand::Status { id } => {
            let status = client
                .get_unit_status(authenticated(
                    cli,
                    GetUnitStatusRequest {
                        service_id: id.clone(),
                    },
                ))
                .await?
                .into_inner();

            match cli.output {
                Output::Json => println!(
                    "{}",
                    serde_json::to_string_pretty(&JsonUnitStatus::from(&status))?
                ),
                Output::Table => println!("{}", format::unit_status_line(&status)),
            }
        }

        ServiceCommand::Start { id } => unit_action(cli, &mut client, id, "start").await?,
        ServiceCommand::Stop { id } => unit_action(cli, &mut client, id, "stop").await?,
        ServiceCommand::Restart { id } => unit_action(cli, &mut client, id, "restart").await?,
        ServiceCommand::Enable { id } => unit_action(cli, &mut client, id, "enable").await?,
        ServiceCommand::Disable { id } => unit_action(cli, &mut client, id, "disable").await?,

        ServiceCommand::Deployments { id, limit } => {
            let mut deployments =
                deployments_client::DeploymentsClient::new(channel(cli).await?);
            let response = deployments
                .list(authenticated(
                    cli,
                    ListDeploymentsRequest {
                        service_id: id.clone(),
                        page_size: *limit,
                        page_token: String::new(),
                    },
                ))
                .await?
                .into_inner();

            let list = response.deployments;
            emit(cli, &JsonDeployments::from(&list), || {
                format::deployments_table(&list)
            });
        }

        ServiceCommand::Releases { id } => {
            let mut deployments =
                deployments_client::DeploymentsClient::new(channel(cli).await?);
            let response = deployments
                .list_releases(authenticated(
                    cli,
                    ListReleasesRequest {
                        service_id: id.clone(),
                    },
                ))
                .await?
                .into_inner();

            let list = response.releases;
            emit(cli, &JsonReleases::from(&list), || {
                format::releases_table(&list)
            });
        }
    }

    Ok(())
}

async fn unit_action(
    cli: &Cli,
    client: &mut services_api_client::ServicesApiClient<Channel>,
    service_id: &str,
    verb: &str,
) -> anyhow::Result<()> {
    let action = match verb {
        "start" => unit_action_request::Action::Start,
        "stop" => unit_action_request::Action::Stop,
        "restart" => unit_action_request::Action::Restart,
        "enable" => unit_action_request::Action::Enable,
        "disable" => unit_action_request::Action::Disable,
        other => bail!("unknown action {other}"),
    };

    let status = client
        .unit_action(authenticated(
            cli,
            UnitActionRequest {
                mutation: Some(mutation(cli)),
                service_id: service_id.to_string(),
                action: action as i32,
            },
        ))
        .await?
        .into_inner();

    println!("{}{verb} {service_id}", dry_run_prefix(cli));
    println!("{}", format::unit_status_line(&status));
    Ok(())
}

// ---------------------------------------------------------------------------
// deploy / rollback
// ---------------------------------------------------------------------------

async fn deploy(
    cli: &Cli,
    service: &str,
    git_ref: Option<&str>,
    artifact_url: Option<&str>,
    skip_health_check: bool,
    wait: bool,
) -> anyhow::Result<()> {
    let mut client = deployments_client::DeploymentsClient::new(channel(cli).await?);

    let deployment = client
        .deploy(authenticated(
            cli,
            DeployRequest {
                mutation: Some(mutation(cli)),
                service_id: service.to_string(),
                git_ref: git_ref.unwrap_or_default().to_string(),
                artifact_url: artifact_url.unwrap_or_default().to_string(),
                skip_health_check,
                // Rolling back on a failed health check is the behaviour that
                // makes this safe to run from CI unattended.
                auto_rollback_on_failure: true,
            },
        ))
        .await?
        .into_inner();

    if cli.dry_run {
        println!("dry run: would deploy {service}");
        if !deployment.previous_release_id.is_empty() {
            println!("  would roll back to {}", deployment.previous_release_id);
        }
        return Ok(());
    }

    println!("deployment {} queued", deployment.id);

    if wait {
        return follow_deployment(cli, &deployment.id).await;
    }

    println!("follow it with: nudo services deployments {service}");
    Ok(())
}

async fn rollback(
    cli: &Cli,
    service: &str,
    release: Option<&str>,
    wait: bool,
) -> anyhow::Result<()> {
    let mut client = deployments_client::DeploymentsClient::new(channel(cli).await?);

    let deployment = client
        .rollback(authenticated(
            cli,
            RollbackRequest {
                mutation: Some(mutation(cli)),
                service_id: service.to_string(),
                release_id: release.unwrap_or_default().to_string(),
            },
        ))
        .await?
        .into_inner();

    if cli.dry_run {
        println!(
            "dry run: would roll {service} back to release {}",
            deployment.release_id
        );
        return Ok(());
    }

    println!(
        "rolling {service} back to release {}",
        deployment.release_id
    );

    if wait {
        return follow_deployment(cli, &deployment.id).await;
    }
    Ok(())
}

/// Streams a deployment to completion, exiting non-zero if it did not succeed.
///
/// This is the shape CI needs: the process's exit status has to reflect whether
/// the deploy actually worked, not merely whether it was accepted.
async fn follow_deployment(cli: &Cli, deployment_id: &str) -> anyhow::Result<()> {
    let mut client = deployments_client::DeploymentsClient::new(channel(cli).await?);

    let mut stream = client
        .watch(authenticated(
            cli,
            WatchDeploymentRequest {
                deployment_id: deployment_id.to_string(),
            },
        ))
        .await?
        .into_inner();

    while let Some(event) = stream.next().await {
        let event = event?;
        match event.event {
            Some(deployment_event::Event::OutputLine(line)) => println!("{line}"),
            Some(deployment_event::Event::StatusChange(status)) => {
                let status = deployment::Status::try_from(status)
                    .unwrap_or(deployment::Status::Unspecified);
                println!("--- {} ---", status.as_str());
            }
            Some(deployment_event::Event::TerminalState(state)) => {
                let status = deployment::Status::try_from(state.status)
                    .unwrap_or(deployment::Status::Unspecified);
                println!("--- {} ---", status.as_str());

                if !state.error.is_empty() {
                    eprintln!("{}", state.error);
                }

                return match status {
                    deployment::Status::Succeeded => Ok(()),
                    other => bail!("deployment {} ({})", other.as_str(), deployment_id),
                };
            }
            None => {}
        }
    }

    // The stream ended without a verdict, which is not something to report as
    // success.
    bail!("the deployment stream ended without a result")
}

// ---------------------------------------------------------------------------
// logs / exec / terminal
// ---------------------------------------------------------------------------

async fn logs(
    cli: &Cli,
    service: &str,
    follow: bool,
    lines: u32,
    grep: Option<&str>,
) -> anyhow::Result<()> {
    let mut client = logs_client::LogsClient::new(channel(cli).await?);

    let mut stream = client
        .stream(authenticated(
            cli,
            StreamLogsRequest {
                service_id: service.to_string(),
                follow,
                tail_lines: lines,
                since_cursor: String::new(),
                since: None,
                grep: grep.unwrap_or_default().to_string(),
            },
        ))
        .await?
        .into_inner();

    while let Some(line) = stream.next().await {
        let line = line?;
        match cli.output {
            Output::Json => println!("{}", serde_json::to_string(&JsonLogLine::from(&line))?),
            Output::Table => {
                let when = nudo_proto::from_timestamp(line.at.as_ref().unwrap_or(&Timestamp {
                    seconds: 0,
                    nanos: 0,
                }))
                .map(|t| t.format("%H:%M:%S").to_string())
                .unwrap_or_else(|| "--:--:--".to_string());
                println!("{when}  {}", line.message);
            }
        }
    }

    Ok(())
}

async fn exec(cli: &Cli, target: &str, command: &[String], timeout: u32) -> anyhow::Result<()> {
    let mut client = logs_client::LogsClient::new(channel(cli).await?);

    let (program, args) = command
        .split_first()
        .ok_or_else(|| anyhow!("a command is required"))?;

    let mut stream = client
        .run_command(authenticated(
            cli,
            RunCommandRequest {
                mutation: Some(mutation(cli)),
                target_id: target.to_string(),
                command: program.clone(),
                args: args.to_vec(),
                timeout_seconds: timeout,
            },
        ))
        .await?
        .into_inner();

    let mut exit_code = -1;
    while let Some(chunk) = stream.next().await {
        match chunk?.chunk {
            Some(command_output::Chunk::Stdout(bytes)) => {
                print!("{}", String::from_utf8_lossy(&bytes));
            }
            Some(command_output::Chunk::Stderr(bytes)) => {
                eprint!("{}", String::from_utf8_lossy(&bytes));
            }
            Some(command_output::Chunk::ExitCode(code)) => exit_code = code,
            None => {}
        }
    }

    // The remote command's status becomes ours, so `nudo exec ... && next` works.
    if exit_code != 0 {
        std::process::exit(if exit_code < 0 { 1 } else { exit_code });
    }
    Ok(())
}

async fn terminal(cli: &Cli, target: &str, command: Option<&str>) -> anyhow::Result<()> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let mut terminals = terminals_client::TerminalsClient::new(channel(cli).await?);

    let (cols, rows) = terminal_size();
    let session = terminals
        .create_session(authenticated(
            cli,
            CreateTerminalSessionRequest {
                mutation: Some(mutation(cli)),
                target_id: target.to_string(),
                initial_command: command.unwrap_or_default().to_string(),
                cols,
                rows,
            },
        ))
        .await?
        .into_inner();

    if cli.dry_run {
        println!("dry run: would open a terminal on {target}");
        return Ok(());
    }

    // The first message attaches; the token is single-use, so this is the only
    // chance to spend it.
    let (outbound, outbound_rx) = tokio::sync::mpsc::channel(64);
    outbound
        .send(TerminalClientMessage {
            message: Some(terminal_client_message::Message::Attach(session)),
        })
        .await
        .map_err(|_| anyhow!("the terminal stream closed before attaching"))?;

    let mut inbound = terminals
        .attach(tokio_stream::wrappers::ReceiverStream::new(outbound_rx))
        .await?
        .into_inner();

    // Forward local stdin to the PTY.
    let stdin_forwarder = tokio::spawn(async move {
        let mut stdin = tokio::io::stdin();
        let mut buffer = vec![0u8; 4096];
        loop {
            match stdin.read(&mut buffer).await {
                Ok(0) => break,
                Ok(read) => {
                    let message = TerminalClientMessage {
                        message: Some(terminal_client_message::Message::Stdin(
                            buffer[..read].to_vec(),
                        )),
                    };
                    if outbound.send(message).await.is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    });

    let mut stdout = tokio::io::stdout();
    let mut exit_code = 0;

    while let Some(message) = inbound.next().await {
        match message?.message {
            Some(terminal_server_message::Message::Stdout(bytes)) => {
                stdout.write_all(&bytes).await?;
                stdout.flush().await?;
            }
            Some(terminal_server_message::Message::ExitCode(code)) => {
                exit_code = code;
                break;
            }
            Some(terminal_server_message::Message::Error(error)) => {
                bail!("{error}");
            }
            None => {}
        }
    }

    stdin_forwarder.abort();

    if exit_code != 0 {
        std::process::exit(exit_code);
    }
    Ok(())
}

/// The terminal's size, defaulting to a conventional 80x24.
///
/// Read from the environment rather than an ioctl so the CLI stays free of a
/// libc dependency; a wrong guess is corrected by the first resize.
fn terminal_size() -> (u32, u32) {
    let read = |name: &str, fallback: u32| {
        std::env::var(name)
            .ok()
            .and_then(|v| v.parse::<u32>().ok())
            .filter(|v| *v > 0)
            .unwrap_or(fallback)
    };
    (read("COLUMNS", 80), read("LINES", 24))
}

// ---------------------------------------------------------------------------
// secrets / audit / sources
// ---------------------------------------------------------------------------

async fn secrets(cli: &Cli, command: &SecretCommand) -> anyhow::Result<()> {
    let mut client = secrets_client::SecretsClient::new(channel(cli).await?);

    match command {
        SecretCommand::List { target, service } => {
            let response = client
                .list(authenticated(
                    cli,
                    ListSecretsRequest {
                        target_id: target.clone().unwrap_or_default(),
                        service_id: service.clone().unwrap_or_default(),
                    },
                ))
                .await?
                .into_inner();

            let list = response.secrets;
            emit(cli, &JsonSecrets::from(&list), || {
                format::secrets_table(&list)
            });
        }

        SecretCommand::Set {
            name,
            value,
            target,
            service,
        } => {
            // Reading from stdin by default keeps the value out of shell history
            // and out of the process table.
            let value = match value {
                Some(value) => value.clone(),
                None => {
                    use tokio::io::AsyncReadExt;
                    let mut buffer = String::new();
                    tokio::io::stdin()
                        .read_to_string(&mut buffer)
                        .await
                        .context("reading the secret value from stdin")?;
                    let trimmed = buffer.trim_end_matches('\n').to_string();
                    if trimmed.is_empty() {
                        bail!(
                            "no value given: pass --value or pipe one in, \
                             e.g. `printf %s \"$TOKEN\" | nudo secrets set NAME`"
                        );
                    }
                    trimmed
                }
            };

            let secret = client
                .put(authenticated(
                    cli,
                    PutSecretRequest {
                        mutation: Some(mutation(cli)),
                        name: name.clone(),
                        value,
                        scope_target_id: target.clone().unwrap_or_default(),
                        scope_service_id: service.clone().unwrap_or_default(),
                    },
                ))
                .await?
                .into_inner();

            println!(
                "{}stored {} ({}) digest {}",
                dry_run_prefix(cli),
                secret.name,
                format::scope_label(&secret),
                secret.digest.chars().take(12).collect::<String>()
            );
        }

        SecretCommand::Remove { id } => {
            client
                .delete(authenticated(
                    cli,
                    DeleteSecretRequest {
                        mutation: Some(mutation(cli)),
                        id: id.clone(),
                    },
                ))
                .await?;
            println!("{}removed secret {id}", dry_run_prefix(cli));
        }
    }

    Ok(())
}

async fn audit(cli: &Cli, subject: Option<&str>, limit: u32) -> anyhow::Result<()> {
    let mut client = audit_client::AuditClient::new(channel(cli).await?);

    let response = client
        .list(authenticated(
            cli,
            ListAuditRequest {
                subject_id: subject.unwrap_or_default().to_string(),
                actor_kind: actor::Kind::Unspecified as i32,
                page_size: limit,
                page_token: String::new(),
            },
        ))
        .await?
        .into_inner();

    let entries = response.entries;
    emit(cli, &JsonAudit::from(&entries), || {
        let rows: Vec<Vec<String>> = entries
            .iter()
            .map(|entry| {
                vec![
                    format::ago(entry.at.as_ref()),
                    entry
                        .actor
                        .as_ref()
                        .map(|a| a.kind_str().to_string())
                        .unwrap_or_else(|| "-".to_string()),
                    entry
                        .actor
                        .as_ref()
                        .map(|a| a.label.clone())
                        .filter(|l| !l.is_empty())
                        .unwrap_or_else(|| "-".to_string()),
                    entry.action.clone(),
                    if entry.dry_run { "yes".to_string() } else { "-".to_string() },
                    format::truncate(&entry.summary, 60),
                ]
            })
            .collect();
        format::table(
            &["when", "kind", "actor", "action", "dry run", "summary"],
            &rows,
        )
    });

    Ok(())
}

async fn sources(cli: &Cli) -> anyhow::Result<()> {
    let mut client = sources_client::SourcesClient::new(channel(cli).await?);
    let response = client
        .list(authenticated(cli, ListSourcesRequest {}))
        .await?
        .into_inner();

    let sources = response.sources;
    emit(cli, &JsonSources::from(&sources), || {
        let rows: Vec<Vec<String>> = sources
            .iter()
            .map(|source| {
                vec![
                    source.id.clone(),
                    source.name.clone(),
                    source::Kind::try_from(source.kind)
                        .unwrap_or(source::Kind::Unspecified)
                        .as_str()
                        .to_string(),
                    if source.account_login.is_empty() {
                        "-".to_string()
                    } else {
                        source.account_login.clone()
                    },
                    if source.installed { "yes".to_string() } else { "no".to_string() },
                ]
            })
            .collect();
        format::table(&["id", "name", "kind", "account", "installed"], &rows)
    });

    Ok(())
}

/// Prefixes human output during a dry run, so it is never mistaken for a real
/// effect.
fn dry_run_prefix(cli: &Cli) -> &'static str {
    if cli.dry_run { "dry run: would have " } else { "" }
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

// ---------------------------------------------------------------------------
// JSON shapes
// ---------------------------------------------------------------------------
//
// The generated proto types do not derive Serialize, and deriving it on them
// would leak wire details (i32 enums, nested oneofs) into scripts. These
// wrappers are the CLI's stable JSON contract.

#[derive(serde::Serialize)]
struct JsonTargets {
    targets: Vec<JsonTarget>,
}

#[derive(serde::Serialize)]
struct JsonTarget {
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
                    labels: t.labels.iter().map(|(k, v)| (k.clone(), v.clone())).collect(),
                })
                .collect(),
        }
    }
}

#[derive(serde::Serialize)]
struct JsonServices {
    services: Vec<JsonService>,
}

#[derive(serde::Serialize)]
struct JsonService {
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
struct JsonDeployments {
    deployments: Vec<JsonDeployment>,
}

#[derive(serde::Serialize)]
struct JsonDeployment {
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
                    actor: d.actor.as_ref().map(|a| a.label.clone()).unwrap_or_default(),
                    error: d.error.clone(),
                })
                .collect(),
        }
    }
}

#[derive(serde::Serialize)]
struct JsonReleases {
    releases: Vec<JsonRelease>,
}

#[derive(serde::Serialize)]
struct JsonRelease {
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
struct JsonSecrets {
    secrets: Vec<JsonSecret>,
}

/// Note the absence of a value field. There is nothing to omit, because the API
/// never returns one.
#[derive(serde::Serialize)]
struct JsonSecret {
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
struct JsonUnitStatus {
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
struct JsonChecks {
    ok: bool,
    checks: Vec<JsonCheck>,
}

#[derive(serde::Serialize)]
struct JsonCheck {
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
struct JsonLogLine {
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
struct JsonAudit {
    entries: Vec<JsonAuditEntry>,
}

#[derive(serde::Serialize)]
struct JsonAuditEntry {
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
                    actor: e.actor.as_ref().map(|a| a.label.clone()).unwrap_or_default(),
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
struct JsonSources {
    sources: Vec<JsonSource>,
}

#[derive(serde::Serialize)]
struct JsonSource {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_command_tree_is_well_formed() {
        // Catches duplicate flags, conflicting shorts and bad defaults at test
        // time rather than on a user's first run.
        use clap::CommandFactory;
        Cli::command().debug_assert();
    }

    #[test]
    fn labels_parse_from_key_equals_value() {
        let parsed = parse_labels(&["env=prod".to_string(), " role = indexer ".to_string()])
            .expect("parse");
        assert_eq!(parsed.get("env").map(String::as_str), Some("prod"));
        assert_eq!(parsed.get("role").map(String::as_str), Some("indexer"));
    }

    #[test]
    fn a_label_without_an_equals_sign_is_rejected_with_advice() {
        let error = parse_labels(&["justakey".to_string()]).expect_err("must fail");
        assert!(error.to_string().contains("key=value"), "got: {error}");
        assert!(parse_labels(&["=value".to_string()]).is_err());
    }

    #[test]
    fn a_mutation_carries_a_human_actor_and_the_global_flags() {
        let cli = Cli::parse_from([
            "nudo",
            "--dry-run",
            "--allow-latency-critical",
            "--idempotency-key",
            "ci-42",
            "targets",
            "list",
        ]);
        let mutation = mutation(&cli);

        assert!(mutation.dry_run);
        assert!(mutation.allow_latency_critical);
        assert_eq!(mutation.idempotency_key, "ci-42");

        let actor = mutation.actor.expect("actor");
        assert_eq!(actor.kind, actor::Kind::Human as i32);
        // The label identifies who and from where, for the audit log.
        assert!(actor.label.contains("(cli)"));
    }

    #[test]
    fn the_guardrail_flags_default_to_off() {
        // A deploy must not touch a latency-critical box unless asked.
        let cli = Cli::parse_from(["nudo", "targets", "list"]);
        let mutation = mutation(&cli);
        assert!(!mutation.dry_run);
        assert!(!mutation.allow_latency_critical);
        assert!(mutation.idempotency_key.is_empty());
    }

    #[test]
    fn a_token_becomes_a_bearer_authorization_header() {
        let cli = Cli::parse_from(["nudo", "--token", "nudo_secret", "targets", "list"]);
        let request = authenticated(&cli, ListTargetsRequest::default());
        assert_eq!(
            request
                .metadata()
                .get("authorization")
                .and_then(|v| v.to_str().ok()),
            Some("Bearer nudo_secret")
        );
    }

    #[test]
    fn no_authorization_header_is_sent_without_a_token() {
        let cli = Cli::parse_from(["nudo", "targets", "list"]);
        let request = authenticated(&cli, ListTargetsRequest::default());
        assert!(request.metadata().get("authorization").is_none());
    }

    #[test]
    fn the_dry_run_prefix_marks_output_that_did_not_happen() {
        let dry = Cli::parse_from(["nudo", "--dry-run", "targets", "list"]);
        assert!(dry_run_prefix(&dry).contains("would"));

        let real = Cli::parse_from(["nudo", "targets", "list"]);
        assert!(dry_run_prefix(&real).is_empty());
    }

    #[test]
    fn terminal_size_falls_back_to_a_conventional_default() {
        // Read from the environment, so an unset COLUMNS must not yield zero.
        let (cols, rows) = terminal_size();
        assert!(cols > 0);
        assert!(rows > 0);
    }

    #[test]
    fn unit_states_get_a_badge_and_a_label() {
        let status = |active: &str, sub: &str| UnitStatus {
            active_state: active.to_string(),
            sub_state: sub.to_string(),
            ..Default::default()
        };

        assert_eq!(format_status_badge(&status("active", "running")), "[ok]");
        assert_eq!(format_status_badge(&status("failed", "failed")), "[!!]");
        assert_eq!(format_status_badge(&status("inactive", "dead")), "[--]");
        assert_eq!(format_status_badge(&status("unknown", "")), "[??]");

        assert_eq!(units_label(&status("active", "running")), "running");
        assert_eq!(units_label(&status("unknown", "")), "unreachable");
    }

    #[test]
    fn a_unit_status_line_includes_the_operational_numbers() {
        let line = format::unit_status_line(&UnitStatus {
            active_state: "active".to_string(),
            sub_state: "running".to_string(),
            pid: 4242,
            memory_bytes: 52_428_800,
            restart_count: 3,
            ..Default::default()
        });

        assert!(line.contains("[ok]"));
        assert!(line.contains("running"));
        assert!(line.contains("pid 4242"));
        assert!(line.contains("50.0 MiB"));
        assert!(line.contains("restarts 3"));
    }

    #[test]
    fn a_stopped_unit_omits_the_numbers_that_do_not_apply() {
        let line = format::unit_status_line(&UnitStatus {
            active_state: "inactive".to_string(),
            sub_state: "dead".to_string(),
            ..Default::default()
        });
        assert!(line.contains("stopped"));
        assert!(!line.contains("pid"));
        assert!(!line.contains("mem"));
    }

    #[test]
    fn the_json_shape_for_secrets_has_no_field_that_could_hold_a_value() {
        let json = serde_json::to_value(JsonSecrets::from(&vec![Secret {
            id: "sec_1".to_string(),
            name: "API_KEY".to_string(),
            digest: "abc".to_string(),
            ..Default::default()
        }]))
        .expect("serialize");

        let secret = &json["secrets"][0];
        assert!(secret.get("value").is_none());
        assert!(secret.get("digest").is_some());
        let keys: Vec<&String> = secret.as_object().expect("object").keys().collect();
        assert_eq!(keys.len(), 4, "id, name, scope, digest — and nothing else");
    }

    #[test]
    fn json_output_renders_enums_as_names_rather_than_numbers() {
        // A script should not have to know that 2 means reachable.
        let json = serde_json::to_value(JsonTargets::from(&vec![Target {
            id: "tgt_1".to_string(),
            status: target::Status::Reachable as i32,
            ..Default::default()
        }]))
        .expect("serialize");
        assert_eq!(json["targets"][0]["status"], "reachable");
    }

    #[test]
    fn deploy_defaults_to_rolling_back_on_a_failed_health_check() {
        // Parsing check: the flag that makes unattended CI deploys safe is not
        // something the user has to remember.
        let cli = Cli::parse_from(["nudo", "deploy", "svc_1"]);
        match cli.command {
            Command::Deploy {
                skip_health_check,
                wait,
                ..
            } => {
                assert!(!skip_health_check);
                assert!(!wait, "--wait is opt-in");
            }
            _ => panic!("expected deploy"),
        }
    }

    #[test]
    fn every_subcommand_group_parses() {
        // A smoke test over the surface the README documents.
        for args in [
            vec!["nudo", "init"],
            vec!["nudo", "targets", "list"],
            vec!["nudo", "targets", "add", "box", "--host", "h", "--ssh-key", "sec_1"],
            vec!["nudo", "targets", "check", "tgt_1"],
            vec!["nudo", "services", "list"],
            vec!["nudo", "services", "unit", "svc_1"],
            vec!["nudo", "services", "restart", "svc_1"],
            vec!["nudo", "services", "releases", "svc_1"],
            vec!["nudo", "deploy", "svc_1", "--wait"],
            vec!["nudo", "rollback", "svc_1"],
            vec!["nudo", "logs", "svc_1", "-f"],
            vec!["nudo", "exec", "tgt_1", "uptime"],
            vec!["nudo", "terminal", "tgt_1"],
            vec!["nudo", "secrets", "list"],
            vec!["nudo", "secrets", "set", "KEY", "--value", "v"],
            vec!["nudo", "audit"],
            vec!["nudo", "sources"],
        ] {
            Cli::try_parse_from(&args).unwrap_or_else(|e| panic!("{args:?} failed to parse: {e}"));
        }
    }

    #[test]
    fn global_flags_apply_to_exec_when_given_before_the_subcommand() {
        // `exec` takes trailing var args, so anything after the target belongs
        // to the remote command — including `--allow-latency-critical`. This is
        // the command where that flag matters most, so the position dependence
        // is pinned here rather than discovered against a production host.
        let cli = Cli::parse_from([
            "nudo",
            "--allow-latency-critical",
            "exec",
            "tgt_1",
            "systemctl",
            "restart",
            "bot",
        ]);
        assert!(mutation(&cli).allow_latency_critical);
        match cli.command {
            Command::Exec { command, .. } => {
                assert_eq!(command, vec!["systemctl", "restart", "bot"]);
            }
            _ => panic!("expected exec"),
        }

        // After the target, the same text is part of the remote command.
        let swallowed = Cli::parse_from([
            "nudo",
            "exec",
            "tgt_1",
            "uptime",
            "--allow-latency-critical",
        ]);
        assert!(
            !mutation(&swallowed).allow_latency_critical,
            "a flag after the target is part of the remote command, not nudo's"
        );
        match swallowed.command {
            Command::Exec { command, .. } => {
                assert_eq!(command, vec!["uptime", "--allow-latency-critical"]);
            }
            _ => panic!("expected exec"),
        }
    }

    #[test]
    fn exec_captures_trailing_arguments_including_flags() {
        // `nudo exec tgt systemctl status --no-pager` must not have --no-pager
        // interpreted as a nudo flag.
        let cli = Cli::parse_from([
            "nudo",
            "exec",
            "tgt_1",
            "systemctl",
            "status",
            "--no-pager",
        ]);
        match cli.command {
            Command::Exec { target, command, .. } => {
                assert_eq!(target, "tgt_1");
                assert_eq!(command, vec!["systemctl", "status", "--no-pager"]);
            }
            _ => panic!("expected exec"),
        }
    }
}
