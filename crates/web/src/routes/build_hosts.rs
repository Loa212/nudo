use super::*;

// ---------------------------------------------------------------------------
// Build hosts
// ---------------------------------------------------------------------------

pub async fn build_hosts_list(State(state): State<AppState>, user: CurrentUser) -> Response {
    // Independent reads, issued together.
    let (hosts, default_id) = tokio::join!(
        state.api.list_build_hosts(),
        state.api.default_build_host_id()
    );
    page(
        "Build hosts",
        Nav::BuildHosts,
        render::build_hosts_list(&hosts, &default_id, &user.csrf_token),
    )
}

pub async fn build_host_new(State(state): State<AppState>, user: CurrentUser) -> Response {
    let secrets = state.api.list_secrets().await;
    page(
        "Add a build host",
        Nav::BuildHosts,
        render::build_host_form(None, &secrets, &user.csrf_token),
    )
}

#[derive(Debug, serde::Deserialize)]
pub struct BuildHostForm {
    pub name: String,
    pub host: String,
    #[serde(default)]
    pub port: String,
    pub user: String,
    #[serde(default)]
    pub ssh_key_id: String,
    #[serde(default)]
    pub workspace_root: String,
    #[serde(default)]
    pub latency_critical: Option<String>,
    #[serde(default)]
    pub labels: String,
    #[serde(default)]
    pub allow_latency_critical: Option<String>,
    pub csrf: String,
}

pub async fn build_host_create(
    State(state): State<AppState>,
    user: CurrentUser,
    Form(form): Form<BuildHostForm>,
) -> Response {
    if let Err(rejection) = check_csrf(&user, &form.csrf) {
        return rejection.into_response();
    }

    let latency_critical = form.latency_critical.is_some();
    let mut envelope = mutation(
        &user,
        &MutationFlags {
            allow_latency_critical: form.allow_latency_critical.clone(),
        },
    );
    // Registering a latency-critical build host is itself the acknowledgement
    // that a build here will contend with whatever else runs on the box, so the
    // checkbox is implied rather than demanded twice.
    if latency_critical {
        envelope.allow_latency_critical = true;
    }

    let mut client = match state.api.build_hosts().await {
        Ok(client) => client,
        Err(status) => return grpc_error(status),
    };

    let result = client
        .create(CreateBuildHostRequest {
            mutation: Some(envelope),
            name: form.name,
            host: form.host,
            port: form.port.trim().parse().unwrap_or(22),
            user: form.user,
            ssh_key_id: form.ssh_key_id,
            workspace_root: form.workspace_root,
            latency_critical,
            labels: parse_labels(&form.labels),
        })
        .await;

    match result {
        Ok(response) => {
            Redirect::to(&format!("/build-hosts/{}", response.into_inner().id)).into_response()
        }
        Err(status) => grpc_error(status),
    }
}

pub async fn build_host_detail(
    State(state): State<AppState>,
    Path(id): Path<String>,
    user: CurrentUser,
) -> Response {
    let mut client = match state.api.build_hosts().await {
        Ok(client) => client,
        Err(status) => return grpc_error(status),
    };

    let host = match client.get(GetBuildHostRequest { id: id.clone() }).await {
        Ok(response) => response.into_inner(),
        Err(status) => return grpc_error(status),
    };

    // Independent reads, issued together.
    let (default_id, services) = tokio::join!(
        state.api.default_build_host_id(),
        state.api.services_building_on(&id),
    );

    page(
        &host.name,
        Nav::BuildHosts,
        render::build_host_detail(&host, &services, &default_id, None, &user.csrf_token),
    )
}

/// Runs the readiness check and re-renders the page with its results.
pub async fn build_host_check(
    State(state): State<AppState>,
    Path(id): Path<String>,
    user: CurrentUser,
) -> Response {
    let mut client = match state.api.build_hosts().await {
        Ok(client) => client,
        Err(status) => return grpc_error(status),
    };

    let host = match client.get(GetBuildHostRequest { id: id.clone() }).await {
        Ok(response) => response.into_inner(),
        Err(status) => return grpc_error(status),
    };

    // A failing check is information, not an error page: showing which part is
    // broken is the whole point.
    let checks = client
        .check(CheckBuildHostRequest { id: id.clone() })
        .await
        .map(|response| response.into_inner())
        .unwrap_or_else(|status| CheckBuildHostResponse {
            ok: false,
            checks: vec![check_build_host_response::Check {
                name: "ssh".to_string(),
                ok: false,
                detail: status.message().to_string(),
            }],
            warnings: Vec::new(),
        });

    // Re-read: the check is what pins a host key on first use and what records
    // a change, so the copy fetched above is already stale.
    let host = client
        .get(GetBuildHostRequest { id: id.clone() })
        .await
        .map(|response| response.into_inner())
        .unwrap_or(host);

    // Independent reads, issued together.
    let (default_id, services) = tokio::join!(
        state.api.default_build_host_id(),
        state.api.services_building_on(&id),
    );

    page(
        &host.name,
        Nav::BuildHosts,
        render::build_host_detail(
            &host,
            &services,
            &default_id,
            Some(&checks),
            &user.csrf_token,
        ),
    )
}

/// Accepts a reviewed host-key change.
pub async fn build_host_accept_host_key(
    State(state): State<AppState>,
    Path(id): Path<String>,
    user: CurrentUser,
    Form(form): Form<AcceptHostKeyForm>,
) -> Response {
    if let Err(rejection) = check_csrf(&user, &form.csrf) {
        return rejection.into_response();
    }

    let mut client = match state.api.build_hosts().await {
        Ok(client) => client,
        Err(status) => return grpc_error(status),
    };

    match client
        .accept_host_key(AcceptBuildHostKeyRequest {
            mutation: Some(mutation(
                &user,
                &MutationFlags {
                    allow_latency_critical: form.allow_latency_critical,
                },
            )),
            id: id.clone(),
            fingerprint: form.fingerprint,
        })
        .await
    {
        Ok(_) => Redirect::to(&format!("/build-hosts/{id}")).into_response(),
        Err(status) => grpc_error(status),
    }
}

pub async fn build_host_delete(
    State(state): State<AppState>,
    Path(id): Path<String>,
    user: CurrentUser,
    Form(form): Form<DeleteForm>,
) -> Response {
    if let Err(rejection) = check_csrf(&user, &form.csrf) {
        return rejection.into_response();
    }

    let mut client = match state.api.build_hosts().await {
        Ok(client) => client,
        Err(status) => return grpc_error(status),
    };

    match client
        .delete(DeleteBuildHostRequest {
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
        Ok(_) => Redirect::to("/build-hosts").into_response(),
        Err(status) => grpc_error(status),
    }
}

#[derive(Debug, serde::Deserialize)]
pub struct BuildDefaultForm {
    /// The build host to default to. Empty, or the `local` sentinel, returns
    /// the instance to building on the control plane.
    #[serde(default)]
    pub build_host_id: String,
    pub csrf: String,
}

/// Sets where builds run when a service does not say.
pub async fn build_default_set(
    State(state): State<AppState>,
    user: CurrentUser,
    Form(form): Form<BuildDefaultForm>,
) -> Response {
    if let Err(rejection) = check_csrf(&user, &form.csrf) {
        return rejection.into_response();
    }

    let mut client = match state.api.build_hosts().await {
        Ok(client) => client,
        Err(status) => return grpc_error(status),
    };

    match client
        .set_defaults(SetBuildDefaultsRequest {
            mutation: Some(mutation(&user, &MutationFlags::default())),
            build_host_id: form.build_host_id,
        })
        .await
    {
        Ok(_) => Redirect::to("/build-hosts").into_response(),
        Err(status) => grpc_error(status),
    }
}
