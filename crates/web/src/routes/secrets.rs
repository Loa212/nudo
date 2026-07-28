use super::*;

// ---------------------------------------------------------------------------
// Secrets
// ---------------------------------------------------------------------------

pub async fn secrets_list(State(state): State<AppState>, user: CurrentUser) -> Response {
    let secrets = state.api.list_secrets().await;
    let targets = state.api.list_targets().await;
    let services = state.api.list_services("").await;
    page(
        "Secrets",
        Nav::Secrets,
        render::secrets_list(&secrets, &targets, &services, &user.csrf_token),
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

    let mut client = match state.api.secrets().await {
        Ok(client) => client,
        Err(status) => return grpc_error(status),
    };

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
        })
        .await
    {
        Ok(_) => Redirect::to("/secrets").into_response(),
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

    if let Err(message) = looks_like_private_key(&form.value) {
        return grpc_error(tonic::Status::invalid_argument(message));
    }

    let mut client = match state.api.secrets().await {
        Ok(client) => client,
        Err(status) => return grpc_error(status),
    };

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
        })
        .await
    {
        Ok(_) => Redirect::to("/secrets").into_response(),
        Err(status) => grpc_error(status),
    }
}

/// Whether a pasted value is plausibly an OpenSSH or PEM private key.
///
/// Deliberately a shape check rather than a parse: nudo accepts several key
/// formats and refusing one this did not recognise would be worse than storing
/// it. What it catches is the mistakes that are otherwise invisible until a
/// deploy fails — a public key, and a truncated paste.
pub(super) fn looks_like_private_key(value: &str) -> Result<(), String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Err("paste the private key".to_string());
    }

    // The single most likely mistake: `id_ed25519.pub` instead of `id_ed25519`.
    if trimmed.starts_with("ssh-") || trimmed.starts_with("ecdsa-sha2-") {
        return Err(
            "that is a public key. nudo needs the private half — the file \
             without the .pub extension."
                .to_string(),
        );
    }

    if !trimmed.starts_with("-----BEGIN ") {
        return Err(
            "that does not look like a private key. It should start with \
             `-----BEGIN OPENSSH PRIVATE KEY-----` or a PEM header."
                .to_string(),
        );
    }

    // A paste that lost its last line is otherwise accepted and fails later.
    if !trimmed.ends_with("-----") {
        return Err(
            "the key looks truncated: it should end with an `-----END ...-----` \
             line. Copy the whole file."
                .to_string(),
        );
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

    let mut client = match state.api.secrets().await {
        Ok(client) => client,
        Err(status) => return grpc_error(status),
    };

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
