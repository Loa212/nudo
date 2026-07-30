//! The MCP server's entry point.
//!
//! Speaks MCP over stdio, which is what an agent host launches as a subprocess.
//! Logging goes to stderr, because stdout is the protocol channel and writing
//! anything else there corrupts it.

use clap::Parser;
use nudo_mcp::NudoTools;
use rmcp::ServiceExt;

#[derive(Parser)]
#[command(
    name = "nudo-mcp",
    about = "Expose the nudo control plane to LLM agents over MCP",
    version
)]
struct Args {
    /// The control plane's gRPC endpoint.
    #[arg(long, env = "NUDO_ENDPOINT", default_value = "http://127.0.0.1:50051")]
    endpoint: String,

    /// How this agent is identified in the audit log. Set it to something an
    /// operator will recognize.
    #[arg(long, env = "NUDO_AGENT_LABEL", default_value = "mcp agent")]
    agent_label: String,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // stderr, never stdout: stdout carries the MCP protocol.
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,nudo_mcp=debug".into()),
        )
        .init();

    let args = Args::parse();

    tracing::info!(
        endpoint = %args.endpoint,
        agent = %args.agent_label,
        "nudo MCP server ready on stdio"
    );

    let service = NudoTools::new(args.endpoint, args.agent_label)?
        .serve(rmcp::transport::stdio())
        .await?;

    service.waiting().await?;

    tracing::info!("MCP session ended");
    Ok(())
}
