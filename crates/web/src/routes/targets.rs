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

    let mut client = state.api.targets();

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
    let mut client = state.api.targets();

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
    let mut client = state.api.targets();

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

    let mut client = state.api.targets();

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

    let mut client = state.api.targets();

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

// ---------------------------------------------------------------------------
// Ingress
//
// Under `/targets/{id}/ingress/...` because ingress is a property of the
// target: there is exactly one per host, and it has no life of its own.
// ---------------------------------------------------------------------------

#[derive(Debug, serde::Deserialize)]
pub struct IngressEnableForm {
    pub csrf: String,
    #[serde(default)]
    pub mode: String,
    #[serde(default)]
    pub acme_email: String,
    #[serde(default)]
    pub allow_latency_critical: Option<String>,
}

pub async fn target_ingress_enable(
    State(state): State<AppState>,
    Path(id): Path<String>,
    user: CurrentUser,
    Form(form): Form<IngressEnableForm>,
) -> Response {
    if let Err(rejection) = check_csrf(&user, &form.csrf) {
        return rejection.into_response();
    }

    // Defaults to managed, which is what the form offers first and what
    // somebody who does not know the difference wants.
    let mode = match form.mode.trim() {
        "external" => ingress::Mode::External,
        _ => ingress::Mode::Managed,
    };

    let mut client = state.api.targets();

    match client
        .enable_ingress(EnableIngressRequest {
            mutation: Some(mutation(
                &user,
                &MutationFlags {
                    allow_latency_critical: form.allow_latency_critical,
                },
            )),
            target_id: id.clone(),
            mode: mode as i32,
            // Caddy's default, filled in server-side.
            admin_port: 0,
            acme_email: form.acme_email.trim().to_string(),
        })
        .await
    {
        Ok(_) => Redirect::to(&format!("/targets/{id}")).into_response(),
        Err(status) => grpc_error(status),
    }
}

pub async fn target_ingress_disable(
    State(state): State<AppState>,
    Path(id): Path<String>,
    user: CurrentUser,
    Form(form): Form<DeleteForm>,
) -> Response {
    if let Err(rejection) = check_csrf(&user, &form.csrf) {
        return rejection.into_response();
    }

    let mut client = state.api.targets();

    match client
        .disable_ingress(DisableIngressRequest {
            mutation: Some(mutation(
                &user,
                &MutationFlags {
                    allow_latency_critical: form.allow_latency_critical,
                },
            )),
            target_id: id.clone(),
        })
        .await
    {
        Ok(_) => Redirect::to(&format!("/targets/{id}")).into_response(),
        Err(status) => grpc_error(status),
    }
}

pub async fn target_ingress_reload(
    State(state): State<AppState>,
    Path(id): Path<String>,
    user: CurrentUser,
    Form(form): Form<DeleteForm>,
) -> Response {
    if let Err(rejection) = check_csrf(&user, &form.csrf) {
        return rejection.into_response();
    }

    let mut client = state.api.targets();

    // A rejected config is not an error page: the reason is recorded against
    // the target and the card shows it, which is where somebody looking at this
    // host will find it — and where it stays visible after the redirect.
    match client
        .reload_ingress(ReloadIngressRequest {
            mutation: Some(mutation(
                &user,
                &MutationFlags {
                    allow_latency_critical: form.allow_latency_critical,
                },
            )),
            target_id: id.clone(),
        })
        .await
    {
        Ok(_) => Redirect::to(&format!("/targets/{id}")).into_response(),
        Err(status) => grpc_error(status),
    }
}

/// The proxy config this target would be given.
///
/// The `View unit` of ingress, and the whole of what external mode offers:
/// render it, copy it, run your own proxy.
pub async fn target_ingress_config(
    State(state): State<AppState>,
    Path(id): Path<String>,
    _user: CurrentUser,
) -> Response {
    let mut client = state.api.targets();

    let response = match client
        .render_ingress(RenderIngressRequest {
            target_id: id.clone(),
        })
        .await
    {
        Ok(response) => response.into_inner(),
        Err(status) => return grpc_error(status),
    };

    // The name rather than the id in the heading, falling back to the id when
    // the target has gone between the render and this lookup.
    let name = client
        .get(GetTargetRequest { id: id.clone() })
        .await
        .map(|target| target.into_inner().name)
        .unwrap_or_else(|_| id.clone());

    page(
        "Proxy config",
        Nav::Targets,
        render::ingress_config(&id, &name, &response),
    )
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
