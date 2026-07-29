use super::*;

pub async fn settings(State(state): State<AppState>, user: CurrentUser) -> Response {
    let tokens = state
        .store
        .list_api_tokens()
        .await
        .unwrap_or_default()
        .into_iter()
        .map(|token| render::TokenView {
            id: token.id,
            name: token.name,
            scopes: token.scopes,
            last_used: token.last_used_at,
            revoked: token.revoked_at.is_some(),
            created: token.created_at,
        })
        .collect::<Vec<_>>();

    let prefs = render::SettingsPrefs {
        update_check_enabled: state.store.release_check_enabled().await.unwrap_or(true),
        support_prompt_enabled: state.store.support_prompt_enabled().await.unwrap_or(true),
        self_upgrade_enabled: state.store.self_upgrade_enabled().await.unwrap_or(false),
        last_checked: state
            .store
            .release_checked_at()
            .await
            .ok()
            .flatten()
            .map(render::ago_at)
            .unwrap_or_default(),
    };

    page(
        "Settings",
        Nav::Settings,
        render::settings_page(&tokens, &user.email, &prefs, &user.csrf_token),
    )
}

#[derive(Debug, serde::Deserialize)]
pub struct TokenForm {
    pub name: String,
    #[serde(default)]
    pub write: Option<String>,
    pub csrf: String,
}

/// Mints an API token and shows it once.
pub async fn token_create(
    State(state): State<AppState>,
    user: CurrentUser,
    Form(form): Form<TokenForm>,
) -> Response {
    if let Err(rejection) = check_csrf(&user, &form.csrf) {
        return rejection.into_response();
    }

    let mut scopes = vec!["read".to_string()];
    if form.write.is_some() {
        scopes.push("write".to_string());
    }

    match state
        .store
        .create_api_token(&form.name, &scopes, &user.id)
        .await
    {
        Ok((token, plaintext)) => {
            state
                .store
                .audit(nudo_server::store::NewAuditEntry {
                    actor: Actor::human(user.id.clone(), user.email.clone()),
                    action: "Settings.CreateApiToken".to_string(),
                    subject_id: token.id.clone(),
                    dry_run: false,
                    summary: format!("created API token {} ({})", token.name, token.scopes),
                })
                .await;

            // Shown once and never again, because only its digest is stored.
            page(
                "API token",
                Nav::Settings,
                render::token_created(&token.name, &plaintext),
            )
        }
        Err(error) => grpc_error(tonic::Status::invalid_argument(format!("{error:#}"))),
    }
}

pub async fn token_revoke(
    State(state): State<AppState>,
    Path(id): Path<String>,
    user: CurrentUser,
    Form(form): Form<DeleteForm>,
) -> Response {
    if let Err(rejection) = check_csrf(&user, &form.csrf) {
        return rejection.into_response();
    }

    match state.store.revoke_api_token(&id).await {
        Ok(()) => {
            state
                .store
                .audit(nudo_server::store::NewAuditEntry {
                    actor: Actor::human(user.id.clone(), user.email.clone()),
                    action: "Settings.RevokeApiToken".to_string(),
                    subject_id: id,
                    dry_run: false,
                    summary: "revoked an API token".to_string(),
                })
                .await;
            Redirect::to("/settings").into_response()
        }
        Err(error) => grpc_error(tonic::Status::invalid_argument(format!("{error:#}"))),
    }
}

#[derive(Debug, serde::Deserialize)]
pub struct PasswordForm {
    pub current_password: String,
    pub new_password: String,
    pub csrf: String,
}

/// Changes the signed-in user's password.
///
/// Requires the current one, and the store invalidates every other session on
/// success — a password change is how someone responds to a suspected
/// compromise, so it has to end the sessions they are worried about.
pub async fn change_password(
    State(state): State<AppState>,
    jar: axum_extra::extract::CookieJar,
    user: CurrentUser,
    Form(form): Form<PasswordForm>,
) -> Response {
    if let Err(rejection) = check_csrf(&user, &form.csrf) {
        return rejection.into_response();
    }

    match state
        .store
        .change_password(&user.id, &form.current_password, &form.new_password)
        .await
    {
        Ok(()) => {
            state
                .store
                .audit(nudo_server::store::NewAuditEntry {
                    actor: Actor::human(user.id.clone(), user.email.clone()),
                    action: "Settings.ChangePassword".to_string(),
                    subject_id: user.id.clone(),
                    dry_run: false,
                    // The password itself is obviously never recorded.
                    summary: "changed their password; all other sessions ended".to_string(),
                })
                .await;

            // This session was invalidated along with the others, so the cookie
            // is cleared and the user signs in again rather than silently
            // hitting a redirect loop.
            let jar = jar.add(crate::auth::clear_cookie(crate::auth::is_https(
                &state.base_url,
            )));
            (jar, Redirect::to("/login")).into_response()
        }
        Err(error) => grpc_error(tonic::Status::invalid_argument(format!("{error:#}"))),
    }
}
