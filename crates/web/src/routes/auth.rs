use super::*;

// ---------------------------------------------------------------------------
// Auth
// ---------------------------------------------------------------------------

#[derive(Debug, serde::Deserialize)]
pub struct LoginForm {
    pub email: String,
    pub password: String,
    pub csrf: String,
}

#[derive(Debug, serde::Deserialize)]
pub struct SetupForm {
    pub email: String,
    pub password: String,
    /// Typed twice, and the two must match. The form has rendered this field
    /// since the beginning, but the handler used to discard it — so a typo
    /// created an account whose password nobody knew, on an instance where
    /// setup had just closed itself behind them.
    #[serde(default)]
    pub password_confirm: String,
    /// Optional. The form does not ask for it: one more box between someone and
    /// a working instance, to set a value they can change later in settings.
    /// Defaults to the local part of the email.
    #[serde(default)]
    pub display_name: String,
    pub csrf: String,
}

/// The login page, or the first-run setup page when no user exists yet.
pub async fn login_page(State(state): State<AppState>) -> Response {
    let has_users = state.store.has_users().await.unwrap_or(true);

    // A CSRF token for a form submitted before any session exists. Bound to a
    // cookie would be circular here, so the pre-auth token is a fixed value and
    // the real defence for these two forms is that they create rather than
    // change state, plus the same-site cookie on everything after.
    let csrf = crate::PRE_AUTH_CSRF;

    // Neither of these is wrapped by `page`: they render their own document,
    // and there is no navigation to show to someone who is not signed in.
    if !has_users && state.allow_setup {
        return Html(render::setup_page(None, csrf).into_string()).into_response();
    }

    Html(render::login_page(None, csrf).into_string()).into_response()
}

/// Handles a login.
pub async fn login(
    State(state): State<AppState>,
    jar: axum_extra::extract::CookieJar,
    Form(form): Form<LoginForm>,
) -> Response {
    if form.csrf != crate::PRE_AUTH_CSRF {
        return crate::auth::CsrfRejected.into_response();
    }

    let user = match state.store.authenticate(&form.email, &form.password).await {
        Ok(Some(user)) => user,
        Ok(None) => {
            // One message for both a missing account and a wrong password, so
            // the response cannot be used to enumerate users.
            return Html(
                render::login_page(
                    Some("That email and password do not match an account."),
                    crate::PRE_AUTH_CSRF,
                )
                .into_string(),
            )
            .into_response();
        }
        Err(error) => {
            tracing::error!(%error, "authentication failed");
            return grpc_error(tonic::Status::internal("could not sign in"));
        }
    };

    let (cookie, _) = match state.store.create_session(&user.id).await {
        Ok(session) => session,
        Err(error) => {
            tracing::error!(%error, "creating the session failed");
            return grpc_error(tonic::Status::internal("could not sign in"));
        }
    };

    let jar = jar.add(crate::auth::session_cookie(
        cookie,
        crate::auth::is_https(&state.base_url),
    ));
    (jar, Redirect::to("/")).into_response()
}

/// Creates the first admin.
pub async fn setup(
    State(state): State<AppState>,
    jar: axum_extra::extract::CookieJar,
    Form(form): Form<SetupForm>,
) -> Response {
    if form.csrf != crate::PRE_AUTH_CSRF {
        return crate::auth::CsrfRejected.into_response();
    }

    // Closed once an account exists, so this cannot be used to add a second
    // admin without authenticating.
    match state.store.has_users().await {
        Ok(true) => return Redirect::to("/login").into_response(),
        Ok(false) => {}
        Err(error) => {
            tracing::error!(%error, "checking for existing users failed");
            return grpc_error(tonic::Status::internal("could not complete setup"));
        }
    }
    if !state.allow_setup {
        return grpc_error(tonic::Status::permission_denied(
            "first-run setup is disabled on this instance",
        ));
    }

    // Checked here rather than only in the browser: setup closes itself once an
    // account exists, so a mistyped password would leave someone locked out of
    // an instance they cannot re-run setup on.
    //
    // An *omitted* confirmation is not a mismatch. The form always sends the
    // field, so a browser that disagrees with itself is a real typo; a script
    // posting this endpoint directly has nothing to confirm against, and
    // rejecting it would make the endpoint unusable without a second copy of
    // the password.
    if !form.password_confirm.is_empty() && form.password_confirm != form.password {
        return Html(
            render::setup_page(Some("Those passwords do not match."), crate::PRE_AUTH_CSRF)
                .into_string(),
        )
        .into_response();
    }

    // The email's local part is a better default than "Admin", and it is
    // editable in settings afterwards.
    let display_name = if form.display_name.trim().is_empty() {
        form.email
            .split('@')
            .next()
            .unwrap_or("admin")
            .trim()
            .to_string()
    } else {
        form.display_name.trim().to_string()
    };

    let user = match state
        .store
        .create_user(&form.email, &form.password, &display_name)
        .await
    {
        Ok(user) => user,
        Err(error) => {
            return Html(
                render::setup_page(Some(&format!("{error:#}")), crate::PRE_AUTH_CSRF).into_string(),
            )
            .into_response();
        }
    };

    let (cookie, _) = match state.store.create_session(&user.id).await {
        Ok(session) => session,
        Err(error) => {
            tracing::error!(%error, "creating the session failed");
            return Redirect::to("/login").into_response();
        }
    };

    let jar = jar.add(crate::auth::session_cookie(
        cookie,
        crate::auth::is_https(&state.base_url),
    ));
    (jar, Redirect::to("/")).into_response()
}

/// Ends the session.
pub async fn logout(
    State(state): State<AppState>,
    jar: axum_extra::extract::CookieJar,
    parts: axum::http::request::Parts,
) -> Response {
    if let Some(cookie) = crate::auth::cookie_value(&parts) {
        let _ = state.store.delete_session(&cookie).await;
    }
    let jar = jar.add(crate::auth::clear_cookie(crate::auth::is_https(
        &state.base_url,
    )));
    (jar, Redirect::to("/login")).into_response()
}
