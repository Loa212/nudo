use super::*;

// ---------------------------------------------------------------------------
// Targets
// ---------------------------------------------------------------------------

pub async fn targets_list(State(state): State<AppState>, _user: CurrentUser) -> Response {
    let targets = state.api.list_targets().await;
    page("Targets", Nav::Targets, render::targets_list(&targets))
}

pub async fn target_new(State(state): State<AppState>, user: CurrentUser) -> Response {
    let secrets = state.api.list_secrets().await;
    page(
        "Add a target",
        Nav::Targets,
        render::target_form(None, &secrets, &user.csrf_token),
    )
}

#[derive(Debug, serde::Deserialize)]
pub struct TargetForm {
    pub name: String,
    pub host: String,
    #[serde(default)]
    pub port: String,
    pub user: String,
    #[serde(default)]
    pub ssh_key_id: String,
    #[serde(default)]
    pub latency_critical: Option<String>,
    #[serde(default)]
    pub labels: String,
    #[serde(default)]
    pub allow_latency_critical: Option<String>,
    pub csrf: String,
}

pub async fn target_create(
    State(state): State<AppState>,
    user: CurrentUser,
    Form(form): Form<TargetForm>,
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
    // Creating a latency-critical target is itself the acknowledgement that it is
    // one, so the checkbox is implied here rather than demanded twice.
    if latency_critical {
        envelope.allow_latency_critical = true;
    }

    let mut client = match state.api.targets().await {
        Ok(client) => client,
        Err(status) => return grpc_error(status),
    };

    let result = client
        .create(CreateTargetRequest {
            mutation: Some(envelope),
            name: form.name,
            host: form.host,
            port: form.port.trim().parse().unwrap_or(22),
            user: form.user,
            ssh_key_id: form.ssh_key_id,
            latency_critical,
            labels: parse_labels(&form.labels),
        })
        .await;

    match result {
        Ok(response) => {
            Redirect::to(&format!("/targets/{}", response.into_inner().id)).into_response()
        }
        Err(status) => grpc_error(status),
    }
}

pub async fn target_detail(
    State(state): State<AppState>,
    Path(id): Path<String>,
    user: CurrentUser,
) -> Response {
    let mut client = match state.api.targets().await {
        Ok(client) => client,
        Err(status) => return grpc_error(status),
    };

    let target = match client.get(GetTargetRequest { id: id.clone() }).await {
        Ok(response) => response.into_inner(),
        Err(status) => return grpc_error(status),
    };

    let services = state.api.list_services(&id).await;
    let statuses = state.api.unit_statuses(&services).await;

    page(
        &target.name,
        Nav::Targets,
        render::target_detail(&target, &services, &statuses, None, &user.csrf_token),
    )
}

/// Runs the readiness check and re-renders the page with its results.
pub async fn target_check(
    State(state): State<AppState>,
    Path(id): Path<String>,
    user: CurrentUser,
) -> Response {
    let mut client = match state.api.targets().await {
        Ok(client) => client,
        Err(status) => return grpc_error(status),
    };

    let target = match client.get(GetTargetRequest { id: id.clone() }).await {
        Ok(response) => response.into_inner(),
        Err(status) => return grpc_error(status),
    };

    // A failing check is information, not an error page: the whole point is to
    // show which part is broken.
    let checks = client
        .check(CheckTargetRequest { id: id.clone() })
        .await
        .map(|response| response.into_inner())
        .unwrap_or_else(|status| CheckTargetResponse {
            ok: false,
            checks: vec![check_target_response::Check {
                name: "ssh".to_string(),
                ok: false,
                detail: status.message().to_string(),
            }],
        });

    // Re-read the target: the check is what pins a host key on first use, and
    // what records a change, so the copy fetched above is already stale.
    let target = client
        .get(GetTargetRequest { id: id.clone() })
        .await
        .map(|response| response.into_inner())
        .unwrap_or(target);

    let services = state.api.list_services(&id).await;
    let statuses = state.api.unit_statuses(&services).await;

    page(
        &target.name,
        Nav::Targets,
        render::target_detail(
            &target,
            &services,
            &statuses,
            Some(&checks),
            &user.csrf_token,
        ),
    )
}

#[derive(Debug, serde::Deserialize)]
pub struct AcceptHostKeyForm {
    /// The fingerprint shown on the page that produced this submission, so a
    /// key that changed again in between is refused rather than accepted
    /// unseen. Checked server-side against what is actually pending.
    pub fingerprint: String,
    pub csrf: String,
    #[serde(default)]
    pub allow_latency_critical: Option<String>,
}

/// Accepts a reviewed host-key change.
pub async fn target_accept_host_key(
    State(state): State<AppState>,
    Path(id): Path<String>,
    user: CurrentUser,
    Form(form): Form<AcceptHostKeyForm>,
) -> Response {
    if let Err(rejection) = check_csrf(&user, &form.csrf) {
        return rejection.into_response();
    }

    let mut client = match state.api.targets().await {
        Ok(client) => client,
        Err(status) => return grpc_error(status),
    };

    match client
        .accept_host_key(AcceptHostKeyRequest {
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
        Ok(_) => Redirect::to(&format!("/targets/{id}")).into_response(),
        Err(status) => grpc_error(status),
    }
}

#[derive(Debug, serde::Deserialize)]
pub struct DeleteForm {
    pub csrf: String,
    #[serde(default)]
    pub allow_latency_critical: Option<String>,
}

pub async fn target_delete(
    State(state): State<AppState>,
    Path(id): Path<String>,
    user: CurrentUser,
    Form(form): Form<DeleteForm>,
) -> Response {
    if let Err(rejection) = check_csrf(&user, &form.csrf) {
        return rejection.into_response();
    }

    let mut client = match state.api.targets().await {
        Ok(client) => client,
        Err(status) => return grpc_error(status),
    };

    match client
        .delete(DeleteTargetRequest {
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
        Ok(_) => Redirect::to("/targets").into_response(),
        Err(status) => grpc_error(status),
    }
}

/// Parses the labels textarea, one `key=value` per line.
pub(super) fn parse_labels(raw: &str) -> std::collections::HashMap<String, String> {
    raw.lines()
        .filter_map(|line| {
            let (key, value) = line.split_once('=')?;
            let key = key.trim();
            if key.is_empty() {
                return None;
            }
            Some((key.to_string(), value.trim().to_string()))
        })
        .collect()
}
