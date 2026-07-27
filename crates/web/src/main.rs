//! The nudo dashboard's HTTP server.

use clap::Parser;
use nudo_web::{AppState, WebConfig, router};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,nudo_web=debug,tower_http=info".into()),
        )
        .init();

    let config = WebConfig::parse();
    let addr = config.addr;

    let state = AppState::new(&config).await?;
    let app = router(state);

    let listener = tokio::net::TcpListener::bind(addr).await?;
    tracing::info!(
        %addr,
        control_plane = %config.grpc_endpoint,
        "nudo dashboard listening"
    );

    axum::serve(listener, app)
        .with_graceful_shutdown(nudo_server::serve::shutdown_signal())
        .await?;

    tracing::info!("shut down cleanly");
    Ok(())
}
