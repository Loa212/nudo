use super::*;

// ---------------------------------------------------------------------------
// Sources
// ---------------------------------------------------------------------------

pub async fn sources_list(State(state): State<AppState>, user: CurrentUser) -> Response {
    let sources = state.api.list_sources().await;
    page(
        "Sources",
        Nav::Sources,
        render::sources_list(&sources, &user.csrf_token),
    )
}

#[derive(Debug, serde::Deserialize)]
pub struct SourceForm {
    pub name: String,
    #[serde(default)]
    pub organization: String,
    pub csrf: String,
}

/// Starts the GitHub App manifest flow.
///
/// Renders a self-submitting form rather than redirecting, because GitHub's
/// manifest endpoint takes the manifest as a POST body — a redirect cannot carry
/// it.
pub async fn source_github_create(
    State(state): State<AppState>,
    user: CurrentUser,
    Form(form): Form<SourceForm>,
) -> Response {
    if let Err(rejection) = check_csrf(&user, &form.csrf) {
        return rejection.into_response();
    }

    let mut client = match state.api.sources().await {
        Ok(client) => client,
        Err(status) => return grpc_error(status),
    };

    let response = match client
        .create_github_app_manifest(CreateGithubAppManifestRequest {
            mutation: Some(mutation(&user, &MutationFlags::default())),
            name: form.name,
            organization: form.organization,
            base_url: state.base_url.clone(),
        })
        .await
    {
        Ok(response) => response.into_inner(),
        Err(status) => return grpc_error(status),
    };

    Html(render::github_handoff(&response.post_url, &response.manifest_json).into_string())
        .into_response()
}

#[derive(Debug, serde::Deserialize)]
pub struct GithubCallback {
    #[serde(default)]
    pub code: String,
    #[serde(default)]
    pub state: String,
}

/// GitHub's redirect after the App is created.
pub async fn source_github_callback(
    State(state): State<AppState>,
    Query(query): Query<GithubCallback>,
    // A logged-in session is required, so a captured redirect cannot be replayed
    // by someone who is not signed in. The single-use state is the second check.
    _user: CurrentUser,
) -> Response {
    let mut client = match state.api.sources().await {
        Ok(client) => client,
        Err(status) => return grpc_error(status),
    };

    match client
        .exchange_github_manifest_code(ExchangeGithubManifestCodeRequest {
            state: query.state,
            code: query.code,
        })
        .await
    {
        Ok(_) => Redirect::to("/sources").into_response(),
        Err(status) => grpc_error(status),
    }
}

#[derive(Debug, serde::Deserialize)]
pub struct GithubInstalled {
    #[serde(default)]
    pub installation_id: String,
    #[serde(default)]
    pub setup_action: String,
}

/// GitHub's redirect after the App is installed on repositories.
///
/// The installation itself arrives as a webhook, which is authenticated; this
/// endpoint only returns the operator to the dashboard.
pub async fn source_github_installed(
    Query(_query): Query<GithubInstalled>,
    _user: CurrentUser,
) -> Response {
    Redirect::to("/sources").into_response()
}

pub async fn source_delete(
    State(state): State<AppState>,
    Path(id): Path<String>,
    user: CurrentUser,
    Form(form): Form<DeleteForm>,
) -> Response {
    if let Err(rejection) = check_csrf(&user, &form.csrf) {
        return rejection.into_response();
    }

    let mut client = match state.api.sources().await {
        Ok(client) => client,
        Err(status) => return grpc_error(status),
    };

    match client
        .delete(DeleteSourceRequest {
            mutation: Some(mutation(&user, &MutationFlags::default())),
            id,
        })
        .await
    {
        Ok(_) => Redirect::to("/sources").into_response(),
        Err(status) => grpc_error(status),
    }
}
