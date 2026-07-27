//! The control plane's gRPC server.

use clap::Parser;
use nudo_server::Config;

#[derive(Parser)]
#[command(
    name = "nudo-server",
    about = "nudo control plane (gRPC API + deploy engine)",
    version
)]
struct Args {
    #[command(flatten)]
    config: Config,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,nudo_server=debug".into()),
        )
        .init();

    let config = Args::parse().config;
    let addr = config.grpc_addr;
    nudo_server::serve::run(config, addr).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_server_command_wraps_the_reusable_configuration() {
        use clap::CommandFactory;
        Args::command().debug_assert();

        let args = Args::parse_from(["nudo-server", "--grpc-addr", "127.0.0.1:6000"]);
        assert_eq!(args.config.grpc_addr.port(), 6000);
    }
}
