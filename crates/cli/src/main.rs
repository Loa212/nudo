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
    #[arg(
        long,
        env = "NUDO_ENDPOINT",
        default_value = "http://127.0.0.1:50051",
        global = true
    )]
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
    /// Manage the machines that build, when the control plane does not.
    #[command(subcommand)]
    BuildHosts(BuildHostCommand),
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
        /// A locally built binary to push, e.g. `--artifact target/release/bot`.
        ///
        /// Served to the control plane over a short-lived loopback HTTP listener
        /// for the duration of the deploy, so it is streamed rather than staged
        /// anywhere. Use `--artifact-url` when the control plane is on another
        /// host and can fetch the binary itself.
        #[arg(long, conflicts_with = "artifact_url")]
        artifact: Option<PathBuf>,
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
    /// Check the host key, SSH, sudo, systemd and a writable release directory.
    Check {
        id: String,
    },
    /// Show a target's pinned SSH host key, and accept a change to it.
    ///
    /// nudo pins a host key on the first successful connection and refuses to
    /// connect if it later changes. A rebuilt host legitimately has a new key,
    /// which is what `--accept` is for; compare the fingerprint against
    /// `ssh-keyscan -t ed25519 <host>` on the machine itself before accepting.
    HostKey {
        id: String,
        /// Accept the pending key with this fingerprint, making it the pinned
        /// one. Naming the fingerprint is required so that what is accepted is
        /// the key that was reviewed.
        #[arg(long, value_name = "SHA256:...")]
        accept: Option<String>,
        /// Forget the pinned key, so the next connection pins afresh.
        ///
        /// Weaker than accepting a reviewed key — it reopens the trust-on-first-
        /// use window — so prefer `--accept` whenever there is a fingerprint to
        /// compare against.
        #[arg(long, conflicts_with = "accept")]
        forget: bool,
    },
    /// Put services on this host behind a domain over HTTPS.
    #[command(subcommand)]
    Ingress(IngressCommand),
}

/// Ingress: the reverse proxy that puts a service on a domain.
///
/// Under `targets` rather than as its own noun because there is exactly one per
/// target and it is a property of the host, not a thing that exists on its own.
///
/// Certificates are Caddy's problem — nudo implements no ACME and never handles
/// a private key for one. What nudo does is install Caddy, write its config
/// from the domains your services declare, and reload it.
#[derive(Subcommand, Debug)]
enum IngressCommand {
    /// Install and start the proxy on this target.
    Enable {
        target: String,
        /// `managed` for nudo to install and drive Caddy, `external` to render
        /// the config for a proxy you run yourself without nudo touching it.
        #[arg(long, default_value = "managed")]
        mode: String,
        /// Where Let's Encrypt sends expiry warnings. Optional, but without it
        /// the first notice of an expiring certificate is the outage.
        #[arg(long)]
        acme_email: Option<String>,
        /// Caddy's admin API port, on loopback.
        #[arg(long)]
        admin_port: Option<u32>,
    },
    /// Stop the proxy, leaving its config on disk.
    Disable { target: String },
    /// Print the proxy config nudo would write, without writing it.
    Show { target: String },
    /// Write the config and reload the proxy.
    ///
    /// A deploy does this for its own target, so this is for when the host and
    /// the database have drifted — a rebuilt machine, a hand-edited config — or
    /// to retry a reload that failed.
    Reload { target: String },
    /// Check the proxy is up and the domains resolve here.
    Check { target: String },
}

/// Build hosts: where a build runs when it does not run on the control plane.
///
/// A build host is not a deploy target and is never deployed to, which is why
/// it is its own noun. It is also not a sandbox: builds on one host are not
/// isolated from each other, and making them so is a property of how the host
/// is run — a one-shot container, an ephemeral VM — rather than something nudo
/// implements.
#[derive(Subcommand)]
enum BuildHostCommand {
    List {
        /// e.g. `arch=arm64,pool=ci`
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
        /// The secret holding this build host's SSH private key.
        #[arg(long)]
        ssh_key: String,
        /// Where checkouts and build trees go. Must be absolute.
        ///
        /// Each build gets a fresh directory underneath, removed when it
        /// finishes however it finishes.
        #[arg(long, value_name = "/var/lib/nudo/builds")]
        workspace_root: Option<String>,
        /// Mark a host where a build will contend with something latency-
        /// sensitive.
        ///
        /// Allowed — you may have exactly one spare machine — but every surface
        /// says so, and mutating it needs `--allow-latency-critical`.
        #[arg(long)]
        latency_critical: bool,
        /// Repeatable, as `key=value`.
        #[arg(long = "label")]
        labels: Vec<String>,
    },
    Remove {
        id: String,
    },
    /// Check the host key, SSH, a writable workspace and git.
    ///
    /// Deliberately not sudo or systemd: nothing is installed or supervised on
    /// a build host.
    Check {
        id: String,
    },
    /// Show a build host's pinned SSH host key, and accept a change to it.
    ///
    /// A build host is handed repository credentials, so its identity is pinned
    /// and verified exactly as a deploy target's is. Compare the fingerprint
    /// against `ssh-keyscan -t ed25519 <host>` on the machine itself before
    /// accepting.
    HostKey {
        id: String,
        /// Accept the pending key with this fingerprint, making it the pinned
        /// one.
        #[arg(long, value_name = "SHA256:...")]
        accept: Option<String>,
        /// Forget the pinned key, so the next connection pins afresh.
        #[arg(long, conflicts_with = "accept")]
        forget: bool,
    },
    /// Show or set where builds run when a service does not say.
    ///
    /// With no argument, prints the current default. A service naming its own
    /// build host always overrides this.
    Default {
        /// The build host to build on by default.
        id: Option<String>,
        /// Build on the control plane by default — the original behaviour.
        #[arg(long, conflicts_with = "id")]
        local: bool,
        /// Print the current default without changing it.
        #[arg(long, conflicts_with_all = ["id", "local"])]
        show: bool,
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
    /// Put this service on a domain, or take it off one.
    ///
    /// Needs ingress enabled on the service's target — see
    /// `nudo targets ingress enable`. The proxy is reloaded immediately, so the
    /// domain works as soon as DNS points at the host.
    Domain {
        id: String,
        /// The hostname to route, e.g. `api.example.com`.
        #[arg(long, conflicts_with = "clear")]
        domain: Option<String>,
        /// The port the service listens on, on the target's loopback.
        #[arg(long, conflicts_with = "clear")]
        port: Option<u32>,
        /// Stop routing to this service.
        #[arg(long)]
        clear: bool,
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
    /// Store a new secret. The value is read from stdin unless --value is given.
    ///
    /// A name that already exists is refused — use `rotate` to replace one.
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
    /// Replace the value of a secret that already exists.
    ///
    /// The old value cannot be read back and is gone once this succeeds.
    /// Separate from `set` so an ordinary write can never destroy one by
    /// accident — including a `set` re-run from shell history.
    Rotate {
        name: String,
        /// The new value. Prefer stdin so it does not reach your shell history.
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
        Command::BuildHosts(command) => build_hosts(cli, command).await,
        Command::Services(command) => services(cli, command).await,
        Command::Deploy {
            service,
            git_ref,
            artifact_url,
            artifact,
            skip_health_check,
            wait,
        } => {
            deploy(
                cli,
                service,
                git_ref.as_deref(),
                artifact_url.as_deref(),
                artifact.as_deref(),
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
    if let Some(token) = &cli.token
        && let Ok(value) = format!("Bearer {token}").parse()
    {
        request.metadata_mut().insert("authorization", value);
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

mod audit_command;
mod build_host_commands;
mod deploy_commands;
mod log_commands;
mod output;
mod secret_commands;
mod service_commands;
mod source_command;
mod target_commands;
mod terminal_command;

use audit_command::audit;
use build_host_commands::build_hosts;
use deploy_commands::{deploy, rollback};
use log_commands::{exec, logs};
use output::*;
use secret_commands::secrets;
use service_commands::services;
use source_command::sources;
use target_commands::targets;
use terminal_command::terminal;

#[cfg(test)]
use deploy_commands::ArtifactServer;
#[cfg(test)]
use target_commands::parse_labels;
#[cfg(test)]
use terminal_command::terminal_size;

#[cfg(test)]
mod tests;
