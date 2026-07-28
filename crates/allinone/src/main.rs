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

    /// Control-plane configuration shared verbatim with `nudo-server`.
    #[command(flatten)]
    server: nudo_server::Config,
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

    let server_config = args.server;

    // Resolved once, before either half starts, so a bad key is a startup error
    // rather than a failure on the first secret read.
    let secret_key = server_config.resolve_secret_key()?;

    // With enforcement on, the dashboard needs a credential of its own or it
    // cannot reach the API — including the page that mints tokens, which would be
    // a lockout. Since both halves are this process, one is provisioned here.
    let dashboard_token = if server_config.require_api_token {
        let store = nudo_server::store::Store::open(&server_config.database).await?;
        store.verify_secret_key(&secret_key).await?;
        Some(provision_dashboard_token(&store).await?)
    } else {
        None
    };

    let web_config = nudo_web::WebConfig {
        addr: args.web_addr,
        grpc_endpoint: format!("http://{}", server_config.grpc_addr),
        database: server_config.database.clone(),
        data_dir: server_config.data_dir.clone(),
        secret_key: server_config.secret_key.clone(),
        secret_key_file: server_config.secret_key_file.clone(),
        base_url: server_config.base_url.clone(),
        allow_setup: server_config.allow_setup,
        api_token: dashboard_token,
    };

    // The gRPC server first, so the dashboard's first request has something to
    // talk to.
    let grpc_addr = server_config.grpc_addr;
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
            .with_graceful_shutdown(nudo_server::serve::shutdown_signal())
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

/// Mints the token the dashboard presents to the API.
///
/// A fresh one per boot, because only the digest is stored and the plaintext
/// cannot be recovered — so reuse is impossible. The previous one is revoked as
/// it is replaced, which keeps the settings list from filling with dead tokens
/// and means a restart invalidates whatever the last process held.
async fn provision_dashboard_token(store: &nudo_server::store::Store) -> anyhow::Result<String> {
    const NAME: &str = "dashboard (all-in-one)";

    // Revoke any earlier one: its plaintext is unrecoverable, so it can never be
    // used again and would otherwise linger in the settings list.
    for token in store.list_api_tokens().await? {
        if token.name == NAME && !token.revoked() {
            let _ = store.revoke_api_token(&token.id).await;
        }
    }

    let (_, plaintext) = store
        .create_api_token(NAME, &["write".to_string()], "system")
        .await?;
    tracing::info!("provisioned an API token for the dashboard");
    Ok(plaintext)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_shared_server_configuration_flattens_into_one_command() {
        use clap::CommandFactory;
        Args::command().debug_assert();

        let args = Args::parse_from([
            "nudo-all-in-one",
            "--web-addr",
            "127.0.0.1:4000",
            "--grpc-addr",
            "127.0.0.1:6000",
            "--log-buffer-lines",
            "42",
        ]);
        assert_eq!(args.web_addr.port(), 4000);
        assert_eq!(args.server.grpc_addr.port(), 6000);
        assert_eq!(args.server.log_buffer_lines, 42);
    }
}
