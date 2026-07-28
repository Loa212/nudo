use super::*;

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
    let source = include_str!("../render.rs");

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
