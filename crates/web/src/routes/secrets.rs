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
