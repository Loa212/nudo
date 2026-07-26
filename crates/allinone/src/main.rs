//! The control plane and dashboard in one process.
//!
//! Most people run nudo on one box, and running two units to manage one host is
//! friction with nothing to show for it. This binary starts the gRPC server and
//! the dashboard in the same runtime; the dashboard still reaches the server over
//! gRPC on loopback, so the split-deployment path and this one exercise exactly
//! the same code.

use std::net::SocketAddr;

use clap::Parser;

#[derive(Parser)]
#[command(
    name = "nudo-all-in-one",
    about = "The nudo control plane and dashboard in one process",
    version
)]
struct Args {
    /// Address the dashboard listens on.
    #[arg(long, env = "NUDO_WEB_ADDR", default_value = "127.0.0.1:3000")]
    web_addr: SocketAddr,

    /// Address the gRPC API listens on.
    ///
    /// Loopback by default: with both halves in one process there is no reason to
    /// expose the API, and the CLI can still reach it locally or through a
    /// tunnel.
    #[arg(long, env = "NUDO_GRPC_ADDR", default_value = "127.0.0.1:50051")]
    grpc_addr: SocketAddr,

    #[arg(long, env = "NUDO_DB", default_value = "nudo.db")]
    database: std::path::PathBuf,

    #[arg(long, env = "NUDO_DATA_DIR", default_value = "./data")]
    data_dir: std::path::PathBuf,

    /// 32-byte AES-256-GCM key for the secret store, hex or base64.
    #[arg(long, env = "NUDO_SECRET_KEY")]
    secret_key: Option<String>,

    #[arg(long, env = "NUDO_SECRET_KEY_FILE")]
    secret_key_file: Option<std::path::PathBuf>,

    /// This instance's public base URL. Used for the session cookie's `Secure`
    /// attribute and for the URLs GitHub is told to call.
    #[arg(long, env = "NUDO_BASE_URL", default_value = "http://localhost:3000")]
    base_url: String,

    #[arg(long, env = "NUDO_LOG_BUFFER", default_value_t = 2000)]
    log_buffer_lines: usize,

    #[arg(long, env = "NUDO_PROBE_INTERVAL", default_value_t = 60)]
    probe_interval_seconds: u64,

    #[arg(long, env = "NUDO_ALLOW_SETUP", default_value_t = true)]
    allow_setup: bool,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,nudo_server=debug,nudo_web=debug".into()),
        )
        .init();

    let args = Args::parse();

    let server_config = nudo_server::Config {
        grpc_addr: args.grpc_addr,
        database: args.database.clone(),
        data_dir: args.data_dir.clone(),
        secret_key: args.secret_key.clone(),
        secret_key_file: args.secret_key_file.clone(),
        base_url: args.base_url.clone(),
        log_buffer_lines: args.log_buffer_lines,
        probe_interval_seconds: args.probe_interval_seconds,
        allow_setup: args.allow_setup,
    };

    // Resolved once, before either half starts, so a bad key is a startup error
    // rather than a failure on the first secret read.
    let _ = server_config.resolve_secret_key()?;

    let web_config = nudo_web::WebConfig {
        addr: args.web_addr,
        grpc_endpoint: format!("http://{}", args.grpc_addr),
        database: args.database,
        data_dir: args.data_dir,
        secret_key: args.secret_key,
        secret_key_file: args.secret_key_file,
        base_url: args.base_url,
        allow_setup: args.allow_setup,
    };

    // The gRPC server first, so the dashboard's first request has something to
    // talk to.
    let grpc_addr = args.grpc_addr;
    let grpc = tokio::spawn(async move {
        if let Err(error) = nudo_server::serve::run(server_config, grpc_addr).await {
            tracing::error!(%error, "the control plane stopped");
        }
    });

    // Wait for the listener rather than sleeping a fixed amount: the dashboard
    // degrades gracefully if the API is down, but a clean start should not show
    // an "unreachable" banner on the first page load.
    wait_for(grpc_addr).await;

    let state = nudo_web::AppState::new(&web_config).await?;
    let app = nudo_web::router(state);
    let listener = tokio::net::TcpListener::bind(web_config.addr).await?;

    tracing::info!(
        dashboard = %web_config.addr,
        api = %grpc_addr,
        "nudo is running — open the dashboard to finish setup"
    );

    let web = tokio::spawn(async move {
        if let Err(error) = axum::serve(listener, app)
            .with_graceful_shutdown(shutdown_signal())
            .await
        {
            tracing::error!(%error, "the dashboard stopped");
        }
    });

    // Either half exiting means this process should exit: running the dashboard
    // without the control plane, or the reverse, is not a useful state.
    tokio::select! {
        _ = grpc => tracing::info!("the control plane exited"),
        _ = web => tracing::info!("the dashboard exited"),
    }

    Ok(())
}

/// Waits briefly for an address to accept connections.
async fn wait_for(addr: SocketAddr) {
    for _ in 0..100 {
        if tokio::net::TcpStream::connect(addr).await.is_ok() {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    // Not fatal: the dashboard renders an "unreachable" banner and recovers on
    // its own once the API is up.
    tracing::warn!(%addr, "the control plane did not come up in time");
}

/// Resolves on SIGINT or SIGTERM.
async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };

    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut signal) => {
                signal.recv().await;
            }
            Err(error) => {
                tracing::warn!(%error, "could not install the SIGTERM handler");
                std::future::pending::<()>().await;
            }
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => tracing::info!("received SIGINT, shutting down"),
        _ = terminate => tracing::info!("received SIGTERM, shutting down"),
    }
}
