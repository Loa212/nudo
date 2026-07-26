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
        .route("/targets/{id}/delete", post(routes::target_delete))
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
mod tests {
    use super::*;

    #[test]
    fn the_configuration_parses_with_working_defaults() {
        let config = WebConfig::parse_from(["nudo-web"]);
        assert_eq!(config.addr.port(), 3000);
        assert_eq!(config.grpc_endpoint, "http://127.0.0.1:50051");
        assert!(config.allow_setup);
    }

    #[test]
    fn the_configuration_accepts_explicit_values() {
        let config = WebConfig::parse_from([
            "nudo-web",
            "--addr",
            "0.0.0.0:8080",
            "--grpc-endpoint",
            "http://control:50051",
            "--base-url",
            "https://nudo.example.com",
        ]);
        assert_eq!(config.addr.to_string(), "0.0.0.0:8080");
        assert_eq!(config.grpc_endpoint, "http://control:50051");
        assert!(auth::is_https(&config.base_url));
    }

    #[test]
    fn the_command_tree_is_well_formed() {
        use clap::CommandFactory;
        WebConfig::command().debug_assert();
    }

    async fn state() -> AppState {
        let store = Store::open_in_memory().await.expect("store");
        let secret_key = SecretKey::generate();
        let bus = Bus::default();
        AppState {
            api: Api::new("http://127.0.0.1:1", None),
            store: store.clone(),
            secret_key: secret_key.clone(),
            engine: Engine {
                store: store.clone(),
                bus: bus.clone(),
                secret_key,
                config: Arc::new(nudo_server::Config::default()),
            },
            bus,
            updates: Arc::new(nudo_server::updates::UpdateChecker::new(
                store,
                nudo_server::updates::DEFAULT_MANIFEST_URL.to_string(),
                false,
            )),
            base_url: "http://localhost:3000".to_string(),
            allow_setup: true,
        }
    }

    #[tokio::test]
    async fn the_router_builds() {
        // Catches a duplicate route or a handler whose extractors do not satisfy
        // axum's bounds — both of which are compile or panic errors otherwise.
        let _ = router(state().await);
    }

    #[tokio::test]
    async fn every_form_the_dashboard_renders_posts_to_a_route_that_exists() {
        // A form whose action does not match a registered route is a 404 the
        // moment a user clicks it, and nothing else catches it: the renderer
        // compiles, the router compiles, and their own tests pass. Five real
        // defects shipped this way — the service start/stop/restart buttons, the
        // GitHub App creation form and the password form all posted to paths that
        // were never registered.
        use axum::body::Body;
        use axum::http::{Request, StatusCode};
        use tower::ServiceExt as _;

        // Every distinct action emitted by `render.rs`, with path parameters
        // filled in. Kept as a literal list so adding a form means adding a line
        // here, which is the prompt to check the route exists.
        let actions = [
            "/login",
            "/setup",
            "/logout",
            "/secrets",
            "/secrets/sec_1/delete",
            "/targets",
            "/targets/tgt_1/check",
            "/targets/tgt_1/delete",
            "/services",
            "/services/svc_1/edit",
            "/services/svc_1/action",
            "/services/svc_1/delete",
            "/services/svc_1/deploy",
            "/services/svc_1/rollback",
            "/deployments/dep_1/cancel",
            "/sources/github",
            "/sources/src_1/delete",
            "/settings/password",
            "/settings/tokens",
            "/settings/tokens/tok_1/revoke",
            "/settings/updates",
            "/settings/support",
            "/support/dismiss",
        ];

        for action in actions {
            let response = router(state().await)
                .oneshot(
                    Request::builder()
                        .method("POST")
                        .uri(action)
                        .header("content-type", "application/x-www-form-urlencoded")
                        .body(Body::from(""))
                        .expect("request"),
                )
                .await
                .expect("response");

            // Anything but 404/405 means the route is registered. Most will
            // redirect to /login (no session) or reject the empty body, both of
            // which prove the path resolves.
            assert_ne!(
                response.status(),
                StatusCode::NOT_FOUND,
                "POST {action} is rendered by a form but matches no route"
            );
            assert_ne!(
                response.status(),
                StatusCode::METHOD_NOT_ALLOWED,
                "POST {action} is rendered by a form but the route is not a POST"
            );
        }
    }

    #[test]
    fn no_form_action_in_the_renderer_is_missing_from_that_list() {
        // The list above is only as good as its completeness, so the renderer's
        // own source is scanned for actions it does not mention.
        let source = include_str!("render.rs");

        // Static actions, e.g. `action="/secrets"`.
        let mut found: Vec<String> = source
            .split("action=\"")
            .skip(1)
            .filter_map(|rest| rest.split('"').next())
            .filter(|action| action.starts_with('/'))
            .map(str::to_string)
            .collect();

        // Interpolated ones, e.g. `action=(format!("/services/{}/deploy", ...))`.
        for rest in source.split("action=(format!(\"").skip(1) {
            if let Some(action) = rest.split('"').next() {
                found.push(action.to_string());
            }
        }

        found.sort();
        found.dedup();

        // The shapes the route-coverage test above exercises, with parameters
        // written the way the renderer emits them.
        let covered = [
            "/login",
            "/setup",
            "/secrets",
            "/secrets/{}/delete",
            "/targets",
            "/targets/{id}/delete",
            "/services",
            "/services/{}/action",
            "/services/{}/deploy",
            "/services/{}/rollback",
            "/services/{id}/delete",
            "/deployments/{}/cancel",
            "/sources/github",
            "/sources/{}/delete",
            "/settings/password",
            "/settings/tokens",
            "/settings/tokens/{}/revoke",
            "/settings/updates",
            "/settings/support",
            "/support/dismiss",
        ];

        for action in &found {
            assert!(
                covered.contains(&action.as_str()),
                "render.rs emits action {action:?}, which the route-coverage test \
                 does not check — add it there and confirm the route exists"
            );
        }
    }

    #[tokio::test]
    async fn every_dashboard_route_requires_a_session() {
        use axum::body::Body;
        use axum::http::{Request, StatusCode};
        use tower::ServiceExt as _;

        // The pages an unauthenticated visitor must not reach.
        for path in [
            "/",
            "/targets",
            "/targets/new",
            "/services",
            "/services/new",
            "/deployments",
            "/secrets",
            "/sources",
            "/audit",
            "/settings",
            "/terminal",
        ] {
            let app = router(state().await);
            let response = app
                .oneshot(
                    Request::builder()
                        .uri(path)
                        .body(Body::empty())
                        .expect("request"),
                )
                .await
                .expect("response");

            assert_eq!(
                response.status(),
                StatusCode::SEE_OTHER,
                "{path} did not redirect an anonymous visitor"
            );
            assert_eq!(
                response
                    .headers()
                    .get(axum::http::header::LOCATION)
                    .and_then(|v| v.to_str().ok()),
                Some("/login"),
                "{path} redirected somewhere other than the login page"
            );
        }
    }

    #[tokio::test]
    async fn the_live_status_stream_resolves_rather_than_matching_the_id_route() {
        // `/services/stream` sits alongside `/services/{id}`; a static segment has
        // to win, or the stream would be treated as a service called "stream".
        use axum::body::Body;
        use axum::http::{Request, StatusCode};
        use tower::ServiceExt as _;

        let response = router(state().await)
            .oneshot(
                Request::builder()
                    .uri("/services/stream")
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("response");

        // Anonymous, so it redirects — which still proves the path is registered.
        assert_eq!(response.status(), StatusCode::SEE_OTHER);
        assert_eq!(
            response
                .headers()
                .get(axum::http::header::LOCATION)
                .and_then(|v| v.to_str().ok()),
            Some("/login")
        );
    }

    #[tokio::test]
    async fn the_login_page_and_assets_are_reachable_without_a_session() {
        use axum::body::Body;
        use axum::http::{Request, StatusCode};
        use tower::ServiceExt as _;

        for path in ["/login", "/assets/app.css", "/assets/htmx.min.js"] {
            let app = router(state().await);
            let response = app
                .oneshot(
                    Request::builder()
                        .uri(path)
                        .body(Body::empty())
                        .expect("request"),
                )
                .await
                .expect("response");
            assert_eq!(
                response.status(),
                StatusCode::OK,
                "{path} was not reachable"
            );
        }
    }

    #[tokio::test]
    async fn the_webhook_endpoint_is_reachable_without_a_session() {
        // GitHub has no cookie; the signature is what authenticates a delivery.
        use axum::body::Body;
        use axum::http::{Request, StatusCode};
        use tower::ServiceExt as _;

        let app = router(state().await);
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/webhooks/github")
                    .header("x-github-event", "ping")
                    .body(Body::from("{}"))
                    .expect("request"),
            )
            .await
            .expect("response");

        // A ping is answered rather than redirected to a login page.
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn an_unsigned_push_delivery_is_rejected() {
        use axum::body::Body;
        use axum::http::{Request, StatusCode};
        use tower::ServiceExt as _;

        let app = router(state().await);
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/webhooks/github")
                    .header("x-github-event", "push")
                    .header("x-github-hook-installation-target-id", "123")
                    .body(Body::from(r#"{"ref":"refs/heads/main"}"#))
                    .expect("request"),
            )
            .await
            .expect("response");

        // No source is configured for that app id, so there is nothing to act
        // on — and crucially it is not treated as authentic.
        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), 64 * 1024)
            .await
            .expect("body");
        let text = String::from_utf8_lossy(&body);
        assert!(text.contains("no source"), "got: {text}");
    }

    #[tokio::test]
    async fn an_unknown_page_renders_a_not_found_rather_than_a_blank_response() {
        use axum::body::Body;
        use axum::http::{Request, StatusCode};
        use tower::ServiceExt as _;

        let app = router(state().await);
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/nope")
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("response");

        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        let body = axum::body::to_bytes(response.into_body(), 64 * 1024)
            .await
            .expect("body");
        assert!(String::from_utf8_lossy(&body).contains("does not exist"));
    }

    #[tokio::test]
    async fn the_setup_page_is_offered_only_until_an_account_exists() {
        use axum::body::Body;
        use axum::http::Request;
        use tower::ServiceExt as _;

        let state = state().await;

        // No users yet: the login route offers first-run setup.
        let response = router(state.clone())
            .oneshot(
                Request::builder()
                    .uri("/login")
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("response");
        let body = axum::body::to_bytes(response.into_body(), 256 * 1024)
            .await
            .expect("body");
        assert!(
            String::from_utf8_lossy(&body).contains("/setup"),
            "the first-run form should post to /setup"
        );

        // Once an account exists it is a plain login page.
        state
            .store
            .create_user("admin@example.com", "correct horse battery", "Admin")
            .await
            .expect("user");

        let response = router(state.clone())
            .oneshot(
                Request::builder()
                    .uri("/login")
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("response");
        let body = axum::body::to_bytes(response.into_body(), 256 * 1024)
            .await
            .expect("body");
        let text = String::from_utf8_lossy(&body);
        assert!(
            !text.contains("action=\"/setup\""),
            "setup must close once claimed"
        );
    }

    #[tokio::test]
    async fn setup_is_refused_once_an_account_exists() {
        // Otherwise anyone reaching the instance could add a second admin.
        use axum::body::Body;
        use axum::http::Request;
        use tower::ServiceExt as _;

        let state = state().await;
        state
            .store
            .create_user("admin@example.com", "correct horse battery", "Admin")
            .await
            .expect("user");

        let response = router(state)
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/setup")
                    .header("content-type", "application/x-www-form-urlencoded")
                    .body(Body::from(format!(
                        "email=attacker@example.com&password=correct+horse+battery&display_name=X&csrf={PRE_AUTH_CSRF}"
                    )))
                    .expect("request"),
            )
            .await
            .expect("response");

        assert!(response.status().is_redirection());
        assert_eq!(
            response
                .headers()
                .get(axum::http::header::LOCATION)
                .and_then(|v| v.to_str().ok()),
            Some("/login")
        );
    }

    #[tokio::test]
    async fn a_login_with_the_wrong_csrf_token_is_refused() {
        use axum::body::Body;
        use axum::http::{Request, StatusCode};
        use tower::ServiceExt as _;

        let state = state().await;
        state
            .store
            .create_user("admin@example.com", "correct horse battery", "Admin")
            .await
            .expect("user");

        let response = router(state)
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/login")
                    .header("content-type", "application/x-www-form-urlencoded")
                    .body(Body::from(
                        "email=admin@example.com&password=correct+horse+battery&csrf=wrong",
                    ))
                    .expect("request"),
            )
            .await
            .expect("response");

        assert_eq!(response.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn a_successful_login_sets_a_hardened_session_cookie() {
        use axum::body::Body;
        use axum::http::Request;
        use tower::ServiceExt as _;

        let state = state().await;
        state
            .store
            .create_user("admin@example.com", "correct horse battery", "Admin")
            .await
            .expect("user");

        let response = router(state)
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/login")
                    .header("content-type", "application/x-www-form-urlencoded")
                    .body(Body::from(format!(
                        "email=admin@example.com&password=correct+horse+battery&csrf={PRE_AUTH_CSRF}"
                    )))
                    .expect("request"),
            )
            .await
            .expect("response");

        assert!(response.status().is_redirection());
        let cookie = response
            .headers()
            .get(axum::http::header::SET_COOKIE)
            .and_then(|v| v.to_str().ok())
            .expect("a session cookie");

        assert!(cookie.contains(auth::COOKIE_NAME));
        assert!(cookie.contains("HttpOnly"), "got: {cookie}");
        assert!(cookie.contains("SameSite=Lax"), "got: {cookie}");
    }

    #[tokio::test]
    async fn a_wrong_password_does_not_set_a_session_and_says_nothing_specific() {
        use axum::body::Body;
        use axum::http::Request;
        use tower::ServiceExt as _;

        let state = state().await;
        state
            .store
            .create_user("admin@example.com", "correct horse battery", "Admin")
            .await
            .expect("user");

        let response = router(state)
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/login")
                    .header("content-type", "application/x-www-form-urlencoded")
                    .body(Body::from(format!(
                        "email=admin@example.com&password=wrong+password+here&csrf={PRE_AUTH_CSRF}"
                    )))
                    .expect("request"),
            )
            .await
            .expect("response");

        assert!(
            response
                .headers()
                .get(axum::http::header::SET_COOKIE)
                .is_none()
        );

        let body = axum::body::to_bytes(response.into_body(), 256 * 1024)
            .await
            .expect("body");
        let text = String::from_utf8_lossy(&body);
        // The same message for a missing account and a wrong password, so it
        // cannot be used to enumerate users.
        assert!(text.contains("do not match"), "got: {text}");
    }
    /// Fetches a page's HTML through the real router.
    async fn get(state: AppState, path: &str) -> String {
        use axum::body::Body;
        use axum::http::Request;
        use tower::ServiceExt as _;

        let response = router(state)
            .oneshot(
                Request::builder()
                    .uri(path)
                    .body(Body::empty())
                    .expect("a valid request"),
            )
            .await
            .expect("the router responds");

        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("a readable body");
        String::from_utf8(bytes.to_vec()).expect("utf-8")
    }

    #[tokio::test]
    async fn a_signed_out_visitor_is_shown_no_navigation() {
        // The bug this pins: both auth pages render their own document, and the
        // handler wrapped them in `page()` as well — so someone who was not
        // signed in got the full navigation rail beside the login form, every
        // item of which redirects straight back to login.
        let state = state().await;
        let store = state.store.clone();
        store
            .create_user("someone@example.com", "correct horse battery", "Someone")
            .await
            .expect("a user, so /login is the login page rather than setup");

        let html = get(state, "/login").await;

        assert!(html.contains("Sign in"), "this is not the login page");
        assert!(
            !html.contains(r#"class="rail""#),
            "the login page renders the navigation rail"
        );
        // One document, not two nested ones.
        assert_eq!(html.matches("<!DOCTYPE html>").count(), 1);
    }

    #[tokio::test]
    async fn a_fresh_instance_offers_to_create_the_first_account() {
        // With no users, /login is the setup page instead — that is the whole
        // first-run experience, and nothing else prompts for it.
        let state = state().await;

        let html = get(state, "/login").await;

        assert!(
            html.contains("Set up nudo"),
            "no setup form on a fresh instance"
        );
        assert!(html.contains(r#"action="/setup""#));
        assert!(
            !html.contains(r#"class="rail""#),
            "the setup page renders the navigation rail"
        );
    }

    #[tokio::test]
    async fn setup_closes_once_an_account_exists() {
        // Otherwise anyone reaching the instance could add themselves as a
        // second admin without authenticating.
        let state = state().await;
        state
            .store
            .create_user("first@example.com", "correct horse battery", "First")
            .await
            .expect("user");

        let html = get(state, "/login").await;
        assert!(!html.contains("Set up nudo"));
        assert!(html.contains("Sign in"));
    }
}
