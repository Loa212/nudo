//! The control plane's gRPC server.

use clap::Parser;
use nudo_server::Config;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,nudo_server=debug".into()),
        )
        .init();

    let config = Config::parse();
    let addr = config.grpc_addr;
    nudo_server::serve::run(config, addr).await
}
