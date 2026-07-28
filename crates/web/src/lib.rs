//! The nudo dashboard.
//!
//! A server-rendered web tier that is a **gRPC client** of the control plane.
//! The browser holds no gRPC connection and receives only HTML: live views are
//! driven by the web tier holding a gRPC stream server-side and pushing rendered
//! fragments over Server-Sent Events.
//!
//! Two things reach past the gRPC API into the control plane's store, because
//! they are properties of the dashboard rather than of the deployment API: login
//! sessions, and the GitHub webhook receiver's secret lookup.

pub mod assets;
pub mod auth;
pub mod client;
pub mod render;
pub mod routes;
pub mod support;
pub mod terminal;
pub mod webhook;

use std::net::SocketAddr;
use std::sync::Arc;

use axum::Router;
use axum::routing::{get, post};
use clap::Parser;
use nudo_server::crypto::SecretKey;
use nudo_server::deploy::Engine;
use nudo_server::events::Bus;
use nudo_server::store::Store;

use client::Api;

/// The CSRF token used by the two forms that run before any session exists.
///
/// A per-session token would be circular here, since there is no session yet.
/// Those two forms create the first account or exchange a password for a session
/// rather than changing existing state, and everything afterwards uses the
/// session's own token plus a same-site cookie.
pub const PRE_AUTH_CSRF: &str = "nudo-pre-auth";

/// Configuration for the web tier.
#[derive(Parser, Debug, Clone)]
#[command(
    name = "nudo-web",
    about = "nudo dashboard (a gRPC client of the control plane)",
    version
)]
pub struct WebConfig {
    /// Address to serve the dashboard on.
    #[arg(long, env = "NUDO_WEB_ADDR", default_value = "127.0.0.1:3000")]
    pub addr: SocketAddr,

    /// The control plane's gRPC endpoint.
    #[arg(long, env = "NUDO_ENDPOINT", default_value = "http://127.0.0.1:50051")]
    pub grpc_endpoint: String,

    /// The SQLite database, shared with the control plane for sessions and the
    /// webhook receiver's secret lookup.
    #[arg(long, env = "NUDO_DB", default_value = "nudo.db")]
    pub database: std::path::PathBuf,

    /// Directory holding the generated secret key, when one was not configured.
    #[arg(long, env = "NUDO_DATA_DIR", default_value = "./data")]
    pub data_dir: std::path::PathBuf,

    /// The secret-store key, hex or base64. Must match the control plane's.
    #[arg(long, env = "NUDO_SECRET_KEY")]
    pub secret_key: Option<String>,

    /// File holding the secret-store key.
    #[arg(long, env = "NUDO_SECRET_KEY_FILE")]
    pub secret_key_file: Option<std::path::PathBuf>,

    /// This dashboard's public base URL. Decides the session cookie's `Secure`
    /// attribute and the URLs GitHub is told to call.
    #[arg(long, env = "NUDO_BASE_URL", default_value = "http://localhost:3000")]
    pub base_url: String,

    /// Allow the first request to create the initial admin when no user exists.
    #[arg(long, env = "NUDO_ALLOW_SETUP", default_value_t = true)]
    pub allow_setup: bool,

    /// An API token to present to the control plane.
    ///
    /// Only needed when the control plane runs with `--require-api-token`. It
    /// must carry the `write` scope, since the dashboard performs mutations on
    /// its user's behalf. Without it the dashboard cannot reach an
    /// authentication-requiring API at all — including the page that mints
    /// tokens — so the all-in-one binary provisions one for itself.
    #[arg(long, env = "NUDO_TOKEN")]
    pub api_token: Option<String>,
}

/// Shared state for the handlers.
#[derive(Clone)]
pub struct AppState {
    /// The gRPC client. Everything the API covers goes through here.
    pub api: Api,
    /// Sessions and the webhook receiver's source lookup.
    pub store: Store,
    /// Used by the webhook receiver to open sealed webhook secrets.
    pub secret_key: SecretKey,
    /// The deploy engine, used only by the webhook receiver, which must start a
    /// deploy without a gRPC round trip through its own mutation envelope.
    pub engine: Engine,
    /// Live events, for the terminal and the commit-status writeback.
    pub bus: Bus,
    /// Reads the last release check for the update banner and the changelog.
    /// Never fetches — the control plane's background loop owns the network.
    pub updates: std::sync::Arc<nudo_server::updates::UpdateChecker>,
    pub base_url: String,
    pub allow_setup: bool,
}

impl axum::extract::FromRef<AppState> for Store {
    fn from_ref(state: &AppState) -> Self {
        state.store.clone()
    }
}

impl AppState {
    /// Builds the state from configuration.
    pub async fn new(config: &WebConfig) -> anyhow::Result<Self> {
        // The web tier and the control plane share a database and a key, so the
        // key is resolved the same way in both.
        let server_config = nudo_server::Config {
            database: config.database.clone(),
            data_dir: config.data_dir.clone(),
            secret_key: config.secret_key.clone(),
            secret_key_file: config.secret_key_file.clone(),
            base_url: config.base_url.clone(),
            ..nudo_server::Config::default()
        };

        let secret_key = server_config.resolve_secret_key()?;
        let store = Store::open(&config.database).await?;
        // The dashboard and the control plane must hold the same key; a mismatch
        // is a startup error rather than a failure while opening a terminal.
        store.verify_secret_key(&secret_key).await?;
        let bus = Bus::default();
        let engine = Engine {
            store: store.clone(),
            bus: bus.clone(),
            secret_key: secret_key.clone(),
            config: Arc::new(server_config),
        };

        // Read-only here: this instance of the checker never fetches, it only
        // reads what the control plane's background loop recorded. Constructed
        // with `enabled: false` so that even a future call to `check_now` from
        // the web tier would be a no-op rather than a second poller.
        let updates = Arc::new(nudo_server::updates::UpdateChecker::new(
            store.clone(),
            nudo_server::updates::DEFAULT_MANIFEST_URL.to_string(),
            false,
        ));

        Ok(Self {
            api: Api::new(config.grpc_endpoint.clone(), config.api_token.clone()),
            store,
            secret_key,
            engine,
            bus,
            updates,
            base_url: config.base_url.clone(),
            allow_setup: config.allow_setup,
        })
    }
}

/// Builds the router.
pub fn router(state: AppState) -> Router {
    Router::new()
        // ---- public ----
        .route("/login", get(routes::login_page).post(routes::login))
        .route("/setup", post(routes::setup))
        .route("/logout", post(routes::logout))
        // The webhook is authenticated by its HMAC signature, not by a session:
        // GitHub has no cookie.
        .route("/webhooks/github", post(webhook::receive))
        .route("/assets/{name}", get(routes::asset))
        // ---- dashboard ----
        .route("/", get(routes::dashboard))
        // ---- targets ----
        .route("/targets", get(routes::targets_list))
        .route("/targets/new", get(routes::target_new))
        .route("/targets", post(routes::target_create))
        .route("/targets/{id}", get(routes::target_detail))
        .route("/targets/{id}/check", post(routes::target_check))
        .route(
            "/targets/{id}/host-key/accept",
            post(routes::target_accept_host_key),
        )
        .route("/targets/{id}/delete", post(routes::target_delete))
        // ---- build hosts ----
        .route("/build-hosts", get(routes::build_hosts_list))
        .route("/build-hosts/new", get(routes::build_host_new))
        .route("/build-hosts", post(routes::build_host_create))
        .route("/build-hosts/default", post(routes::build_default_set))
        .route("/build-hosts/{id}", get(routes::build_host_detail))
        .route("/build-hosts/{id}/check", post(routes::build_host_check))
        .route(
            "/build-hosts/{id}/host-key/accept",
            post(routes::build_host_accept_host_key),
        )
        .route("/build-hosts/{id}/delete", post(routes::build_host_delete))
        // ---- services ----
        .route("/services", get(routes::services_list))
        .route("/services/new", get(routes::service_new))
        .route("/services/stream", get(routes::services_stream))
        .route("/services", post(routes::service_create))
        .route("/services/{id}", get(routes::service_detail))
        .route("/services/{id}/edit", get(routes::service_edit))
        .route("/services/{id}/edit", post(routes::service_update))
        .route("/services/{id}/unit", get(routes::service_unit))
        .route("/services/{id}/action", post(routes::service_unit_action))
        .route("/services/{id}/delete", post(routes::service_delete))
        .route("/services/{id}/deploy", post(routes::deploy))
        .route("/services/{id}/rollback", post(routes::rollback))
        .route("/services/{id}/logs", get(routes::logs_view))
        .route("/services/{id}/logs/stream", get(routes::logs_stream))
        // ---- deployments ----
        .route("/deployments", get(routes::deployments_list))
        .route("/deployments/{id}", get(routes::deployment_detail))
        .route("/deployments/{id}/stream", get(routes::deployment_stream))
        .route("/deployments/{id}/cancel", post(routes::deployment_cancel))
        // ---- terminal ----
        .route("/terminal", get(routes::terminal_index))
        .route("/terminal/{id}", get(routes::terminal_page))
        .route("/terminal/ws", get(routes::terminal_websocket))
        // ---- secrets ----
        .route("/secrets", get(routes::secrets_list))
        .route("/secrets", post(routes::secret_put))
        .route("/secrets/{id}/delete", post(routes::secret_delete))
        // ---- sources ----
        .route("/sources", get(routes::sources_list))
        .route("/sources/github", post(routes::source_github_create))
        .route(
            "/sources/github/callback",
            get(routes::source_github_callback),
        )
        .route(
            "/sources/github/installed",
            get(routes::source_github_installed),
        )
        .route("/sources/{id}/delete", post(routes::source_delete))
        // ---- audit and settings ----
        .route("/audit", get(routes::audit_list))
        .route("/changelog", get(routes::changelog))
        .route("/upgrade", get(routes::upgrade))
        .route("/support/dismiss", post(routes::support_dismiss))
        .route("/settings", get(routes::settings))
        .route("/settings/updates", post(routes::settings_updates))
        .route("/settings/support", post(routes::settings_support))
        .route("/settings/password", post(routes::change_password))
        .route("/settings/tokens", post(routes::token_create))
        .route("/settings/tokens/{id}/revoke", post(routes::token_revoke))
        .fallback(routes::not_found)
        .layer(tower_http::trace::TraceLayer::new_for_http())
        // A body cap so a malformed or hostile request cannot exhaust memory.
        // Generous enough for the largest form (a service definition) and for a
        // webhook delivery, which can carry a long commit list.
        .layer(tower_http::limit::RequestBodyLimitLayer::new(
            2 * 1024 * 1024,
        ))
        .with_state(state)
}

#[cfg(test)]
mod tests;
