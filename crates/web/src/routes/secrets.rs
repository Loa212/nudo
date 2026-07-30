use super::*;

// ---------------------------------------------------------------------------
// Secrets
// ---------------------------------------------------------------------------

/// Query parameters carrying a message back after a redirect.
///
/// A refusal has to survive the redirect that follows a POST, and it has to be
/// a redirect: re-rendering the page in the POST response would leave a
/// resubmittable form in the browser's history for a request that stores a
/// secret. The message is looked up by key rather than passed as text, so a
/// crafted link cannot put arbitrary words in a red banner on the operator's
/// own dashboard.
#[derive(Debug, Default, serde::Deserialize)]
pub struct SecretsQuery {
    #[serde(default)]
    pub error: String,
    #[serde(default)]
    pub name: String,
}

pub async fn secrets_list(
    State(state): State<AppState>,
    Query(query): Query<SecretsQuery>,
    user: CurrentUser,
) -> Response {
    let secrets = state.api.list_secrets().await;
    let targets = state.api.list_targets().await;
    let services = state.api.list_services("").await;

    let notice = render::SecretNotice::from_key(&query.error, &query.name);

    page(
        "Secrets",
        Nav::Secrets,
        render::secrets_list(&secrets, &targets, &services, notice, &user.csrf_token),
    )
}

#[derive(Debug, serde::Deserialize)]
pub struct SecretForm {
    pub name: String,
    pub value: String,
    #[serde(default)]
    pub scope_target_id: String,
    #[serde(default)]
    pub scope_service_id: String,
    #[serde(default)]
    pub allow_latency_critical: Option<String>,
    pub csrf: String,
}

pub async fn secret_put(
    State(state): State<AppState>,
    user: CurrentUser,
    Form(form): Form<SecretForm>,
) -> Response {
    if let Err(rejection) = check_csrf(&user, &form.csrf) {
        return rejection.into_response();
    }

    let mut client = state.api.secrets();

    let name = form.name.clone();
    match client
        .put(PutSecretRequest {
            mutation: Some(mutation(
                &user,
                &MutationFlags {
                    allow_latency_critical: form.allow_latency_critical,
                },
            )),
            name: form.name,
            value: form.value,
            scope_target_id: form.scope_target_id,
            scope_service_id: form.scope_service_id,
            // Never from this form. Replacing a value is what the rotate action
            // is for, where the operator is told what they are about to destroy.
            replace: false,
        })
        .await
    {
        Ok(_) => Redirect::to("/secrets").into_response(),
        Err(status) => taken_name_or_error(status, &name),
    }
}

/// Turns the "already exists" refusal into a message on the page.
///
/// Every other failure keeps the generic error page: this one is expected —
/// somebody typed a name that is taken — and telling them so where they typed it
/// is more useful than a full-page error.
fn taken_name_or_error(status: tonic::Status, name: &str) -> Response {
    if status.code() == tonic::Code::InvalidArgument && status.message().contains("already exists")
    {
        return Redirect::to(&format!(
            "/secrets?error=taken&name={}",
            urlencoding::encode(name)
        ))
        .into_response();
    }
    grpc_error(status)
}

#[derive(Debug, serde::Deserialize)]
pub struct RotateSecretForm {
    pub name: String,
    pub value: String,
    #[serde(default)]
    pub scope_target_id: String,
    #[serde(default)]
    pub scope_service_id: String,
    pub csrf: String,
}

/// Replaces the value of a secret that already exists.
///
/// Separate from `secret_put` because it is a different act: the value being
/// replaced cannot be read back, so it is gone the moment this succeeds. Having
/// its own endpoint means the ordinary write can never do it by accident, and
/// the audit trail records a rotation rather than a store.
pub async fn secret_rotate(
    State(state): State<AppState>,
    user: CurrentUser,
    Form(form): Form<RotateSecretForm>,
) -> Response {
    if let Err(rejection) = check_csrf(&user, &form.csrf) {
        return rejection.into_response();
    }

    if form.value.trim().is_empty() {
        return Redirect::to(&format!(
            "/secrets?error=empty&name={}",
            urlencoding::encode(&form.name)
        ))
        .into_response();
    }

    let mut client = state.api.secrets();

    match client
        .put(PutSecretRequest {
            mutation: Some(mutation(&user, &MutationFlags::default())),
            name: form.name,
            value: form.value,
            scope_target_id: form.scope_target_id,
            scope_service_id: form.scope_service_id,
            replace: true,
        })
        .await
    {
        Ok(_) => Redirect::to("/secrets?error=rotated").into_response(),
        Err(status) => grpc_error(status),
    }
}

#[derive(Debug, serde::Deserialize)]
pub struct SshKeyForm {
    pub name: String,
    pub value: String,
    pub csrf: String,
}

/// Stores an SSH private key.
///
/// The same `Secrets.Put` as an environment secret — the store holds one kind of
/// thing — with two differences that are the reason it is its own endpoint.
///
/// It never sets a target or service scope: a key is used to *open* the
/// connection, so scoping it to a target it is needed to reach is either
/// circular or a mistake.
///
/// And it checks the value looks like a private key before storing it. Getting
/// this wrong is otherwise silent: the key is write-only, so a public key or a
/// half-copied file is accepted, stored, and only surfaces much later as a
/// failed connection on a host the operator has no reason to doubt.
pub async fn ssh_key_put(
    State(state): State<AppState>,
    user: CurrentUser,
    Form(form): Form<SshKeyForm>,
) -> Response {
    if let Err(rejection) = check_csrf(&user, &form.csrf) {
        return rejection.into_response();
    }

    // Reported on the page rather than as an error page: this is a mistake in
    // what was typed, and it belongs next to the field it was typed into.
    if let Err(key) = looks_like_private_key(&form.value) {
        return Redirect::to(&format!("/secrets?error={key}")).into_response();
    }

    let mut client = state.api.secrets();

    let name = form.name.clone();
    match client
        .put(PutSecretRequest {
            mutation: Some(mutation(&user, &MutationFlags::default())),
            name: form.name,
            // Trailing whitespace is stripped but the trailing newline is kept:
            // OpenSSH requires the final newline, and a key pasted without one
            // is the more common mistake than one pasted with extra.
            value: format!("{}\n", form.value.trim_end()),
            scope_target_id: String::new(),
            scope_service_id: String::new(),
            replace: false,
        })
        .await
    {
        Ok(_) => Redirect::to("/secrets").into_response(),
        Err(status) => taken_name_or_error(status, &name),
    }
}

/// Whether a pasted value is plausibly an OpenSSH or PEM private key.
///
/// Deliberately a shape check rather than a parse: nudo accepts several key
/// formats and refusing one this did not recognise would be worse than storing
/// it. What it catches is the mistakes that are otherwise invisible until a
/// deploy fails — a public key, and a truncated paste.
///
/// Returns the *key* of the message to show rather than the message itself, so
/// the wording lives with the rest of the page's copy and nothing a caller
/// supplies can reach the banner.
pub(super) fn looks_like_private_key(value: &str) -> Result<(), &'static str> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Err("key-empty");
    }

    // The single most likely mistake: `id_ed25519.pub` instead of `id_ed25519`.
    if trimmed.starts_with("ssh-") || trimmed.starts_with("ecdsa-sha2-") {
        return Err("key-public");
    }

    if !trimmed.starts_with("-----BEGIN ") {
        return Err("key-shape");
    }

    // A paste that lost its last line is otherwise accepted and fails later.
    if !trimmed.ends_with("-----") {
        return Err("key-truncated");
    }

    Ok(())
}

pub async fn secret_delete(
    State(state): State<AppState>,
    Path(id): Path<String>,
    user: CurrentUser,
    Form(form): Form<DeleteForm>,
) -> Response {
    if let Err(rejection) = check_csrf(&user, &form.csrf) {
        return rejection.into_response();
    }

    let mut client = state.api.secrets();

    match client
        .delete(DeleteSecretRequest {
            mutation: Some(mutation(
                &user,
                &MutationFlags {
                    allow_latency_critical: form.allow_latency_critical,
                },
            )),
            id,
        })
        .await
    {
        Ok(_) => Redirect::to("/secrets").into_response(),
        Err(status) => grpc_error(status),
    }
}
