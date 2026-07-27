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
/// Posts a form through the real router.
async fn post(state: AppState, path: &str, body: &str) -> axum::http::Response<axum::body::Body> {
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt as _;

    router(state)
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(path)
                .header("content-type", "application/x-www-form-urlencoded")
                .body(Body::from(body.to_string()))
                .expect("a valid request"),
        )
        .await
        .expect("the router responds")
}

/// The fields a rendered form actually submits: every named input, with a
/// value. This is what a browser sends, as opposed to what a test author
/// remembers to include.
fn submitted_fields(html: &str, action: &str) -> Vec<(String, String)> {
    let form = html
        .split(&format!(r#"action="{action}""#))
        .nth(1)
        .expect("the form is on the page")
        .split("</form>")
        .next()
        .expect("a closed form");

    let mut fields = Vec::new();
    for chunk in form.split("name=\"").skip(1) {
        let name = chunk.split('"').next().unwrap_or_default().to_string();
        // Hidden inputs carry their value in the markup; the rest are typed.
        let value = chunk
            .split("value=\"")
            .nth(1)
            .and_then(|rest| rest.split('"').next())
            .filter(|_| chunk.starts_with("csrf"))
            .unwrap_or("x")
            .to_string();
        fields.push((name, value));
    }
    fields
}

#[tokio::test]
async fn the_setup_form_submits_exactly_what_the_handler_requires() {
    // The bug this pins: the handler required `display_name`, the form had
    // no such input, and every real signup failed with "missing field
    // `display_name`". Both halves compiled and both test suites passed —
    // because the tests built the request by hand instead of submitting
    // what the page renders.
    let html = get(state().await, "/login").await;
    let fields = submitted_fields(&html, "/setup");

    let names: Vec<&str> = fields.iter().map(|(n, _)| n.as_str()).collect();
    assert!(names.contains(&"email"), "the form has no email field");
    assert!(
        names.contains(&"password"),
        "the form has no password field"
    );
    assert!(names.contains(&"csrf"), "the form has no csrf field");

    // Submitting precisely those fields must work.
    let body: String = fields
        .iter()
        .map(|(name, value)| {
            let value = if name == "email" {
                "someone@example.com"
            } else if name.starts_with("password") {
                "correct horse battery staple"
            } else {
                value.as_str()
            };
            format!("{name}={}", urlencoding(value))
        })
        .collect::<Vec<_>>()
        .join("&");

    let response = post(state().await, "/setup", &body).await;
    assert_ne!(
        response.status(),
        axum::http::StatusCode::UNPROCESSABLE_ENTITY,
        "the rendered setup form does not deserialize into what the handler wants"
    );
    assert_ne!(response.status(), axum::http::StatusCode::BAD_REQUEST);
}

/// Minimal percent-encoding for the form bodies above.
fn urlencoding(value: &str) -> String {
    value
        .chars()
        .map(|c| match c {
            ' ' => "+".to_string(),
            'a'..='z' | 'A'..='Z' | '0'..='9' | '-' | '_' | '.' | '~' => c.to_string(),
            other => format!("%{:02X}", other as u32),
        })
        .collect()
}

#[tokio::test]
async fn setup_creates_the_account_without_being_told_a_display_name() {
    let app_state = state().await;
    let store = app_state.store.clone();

    let response = post(
        app_state,
        "/setup",
        concat!(
            "email=someone@example.com",
            "&password=correct+horse+battery+staple",
            "&password_confirm=correct+horse+battery+staple",
            "&csrf=nudo-pre-auth",
        ),
    )
    .await;

    if response.status() != axum::http::StatusCode::SEE_OTHER {
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("body");
        let html = String::from_utf8_lossy(&bytes);
        let error = html
            .split("callout bad")
            .nth(1)
            .and_then(|rest| rest.split("</div>").next())
            .unwrap_or("(no error rendered)");
        panic!("setup did not sign the user in: {error}");
    }
    assert!(
        store.has_users().await.expect("read"),
        "no account was created"
    );

    // The email's local part, rather than an empty name or a placeholder.
    let user = store
        .authenticate("someone@example.com", "correct horse battery staple")
        .await
        .expect("authenticate")
        .expect("the account exists");
    assert_eq!(user.display_name, "someone");
}

#[tokio::test]
async fn a_caller_that_omits_the_confirmation_is_not_treated_as_a_typo() {
    // The regression this pins: adding the confirmation check broke every
    // script that posts /setup directly, including this repository's own
    // demo. A browser always sends the field because the form renders it,
    // so a *disagreeing* pair is a real typo — but an absent one has
    // nothing to disagree with, and rejecting it makes the endpoint
    // unusable without a second copy of the password.
    let app_state = state().await;
    let store = app_state.store.clone();

    let response = post(
        app_state,
        "/setup",
        concat!(
            "email=someone@example.com",
            "&password=correct+horse+battery+staple",
            "&csrf=nudo-pre-auth",
        ),
    )
    .await;

    assert_eq!(response.status(), axum::http::StatusCode::SEE_OTHER);
    assert!(store.has_users().await.expect("read"));
}

#[tokio::test]
async fn a_mistyped_password_does_not_create_an_account() {
    // Setup closes itself once an account exists, so a typo here would lock
    // someone out of an instance they cannot re-run setup on. The form asks
    // twice; this is the half that checks.
    let app_state = state().await;
    let store = app_state.store.clone();

    let response = post(
        app_state,
        "/setup",
        concat!(
            "email=someone@example.com",
            "&password=correct+horse+battery+staple",
            // One character short: the typo this guards against.
            "&password_confirm=correct+horse+battery+stapl",
            "&csrf=nudo-pre-auth",
        ),
    )
    .await;

    assert_eq!(response.status(), axum::http::StatusCode::OK);
    assert!(
        !store.has_users().await.expect("read"),
        "an account was created from mismatched passwords"
    );
}
#[tokio::test]
async fn a_fingerprinted_asset_url_still_resolves() {
    // The query string must not reach the route matcher. If it did, every
    // page would 404 on its own stylesheet — which is worse than the stale
    // cache this fingerprint exists to fix.
    use axum::http::StatusCode;
    use tower::ServiceExt as _;

    let url = crate::assets::url("app.css");
    assert!(
        url.starts_with("/assets/app.css?v="),
        "unexpected shape: {url}"
    );

    let response = router(state().await)
        .oneshot(
            axum::http::Request::builder()
                .uri(&url)
                .body(axum::body::Body::empty())
                .expect("a valid request"),
        )
        .await
        .expect("the router responds");

    assert_eq!(response.status(), StatusCode::OK);
}

#[test]
fn the_asset_fingerprint_is_stable_across_calls() {
    // Recomputed per call, it would change mid-page and defeat the cache
    // entirely.
    assert_eq!(crate::assets::build_id(), crate::assets::build_id());
    assert!(!crate::assets::build_id().is_empty());
}

#[tokio::test]
async fn every_asset_a_page_references_is_fingerprinted() {
    // A reference that skips `url()` is one that goes on serving a stale
    // copy after a deploy — the exact bug this replaced, hiding in one
    // template rather than all of them.
    let html = get(state().await, "/login").await;

    for reference in html.split("/assets/").skip(1) {
        let url = reference.split('"').next().unwrap_or_default();
        assert!(
            url.contains("?v="),
            "/assets/{url} is referenced without a fingerprint"
        );
    }
}
