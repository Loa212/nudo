use super::*;

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
