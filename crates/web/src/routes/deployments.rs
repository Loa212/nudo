use super::*;

// ---------------------------------------------------------------------------
// Deployments
// ---------------------------------------------------------------------------

/// Which deployments to list.
#[derive(Debug, Default, serde::Deserialize)]
pub struct DeploymentsQuery {
    /// Only this service's deployments. A service detail page links here with it
    /// set, so the filter has to be honoured rather than ignored.
    #[serde(default)]
    pub service: Option<String>,
}

pub async fn deployments_list(
    State(state): State<AppState>,
    Query(query): Query<DeploymentsQuery>,
    _user: CurrentUser,
) -> Response {
    let service_filter = query.service.unwrap_or_default();
    let deployments = state.api.list_deployments(&service_filter, 50).await;
    let services = state.api.list_services("").await;
    page(
        "Deployments",
        Nav::Deployments,
        render::deployments_list(&deployments, &services),
    )
}

#[derive(Debug, serde::Deserialize)]
pub struct DeployForm {
    pub csrf: String,
    #[serde(default)]
    pub git_ref: String,
    #[serde(default)]
    pub skip_health_check: Option<String>,
    #[serde(default)]
    pub allow_latency_critical: Option<String>,
}

pub async fn deploy(
    State(state): State<AppState>,
    Path(id): Path<String>,
    user: CurrentUser,
    Form(form): Form<DeployForm>,
) -> Response {
    if let Err(rejection) = check_csrf(&user, &form.csrf) {
        return rejection.into_response();
    }

    let mut client = state.api.deployments();

    let result = client
        .deploy(DeployRequest {
            mutation: Some(mutation(
                &user,
                &MutationFlags {
                    allow_latency_critical: form.allow_latency_critical,
                },
            )),
            service_id: id,
            git_ref: form.git_ref.trim().to_string(),
            artifact_url: String::new(),
            skip_health_check: form.skip_health_check.is_some(),
            auto_rollback_on_failure: true,
        })
        .await;

    match result {
        // Straight to the live view, which is what someone who just clicked
        // Deploy wants to see.
        Ok(response) => {
            Redirect::to(&format!("/deployments/{}", response.into_inner().id)).into_response()
        }
        Err(status) => grpc_error(status),
    }
}

#[derive(Debug, serde::Deserialize)]
pub struct RollbackForm {
    pub csrf: String,
    #[serde(default)]
    pub release_id: String,
    #[serde(default)]
    pub allow_latency_critical: Option<String>,
}

pub async fn rollback(
    State(state): State<AppState>,
    Path(id): Path<String>,
    user: CurrentUser,
    Form(form): Form<RollbackForm>,
) -> Response {
    if let Err(rejection) = check_csrf(&user, &form.csrf) {
        return rejection.into_response();
    }

    let mut client = state.api.deployments();

    let result = client
        .rollback(RollbackRequest {
            mutation: Some(mutation(
                &user,
                &MutationFlags {
                    allow_latency_critical: form.allow_latency_critical,
                },
            )),
            service_id: id,
            release_id: form.release_id.trim().to_string(),
        })
        .await;

    match result {
        Ok(response) => {
            Redirect::to(&format!("/deployments/{}", response.into_inner().id)).into_response()
        }
        Err(status) => grpc_error(status),
    }
}

pub async fn deployment_detail(
    State(state): State<AppState>,
    Path(id): Path<String>,
    user: CurrentUser,
) -> Response {
    let mut client = state.api.deployments();

    let deployment = match client.get(GetDeploymentRequest { id: id.clone() }).await {
        Ok(response) => response.into_inner(),
        Err(status) => return grpc_error(status),
    };

    let service = state
        .api
        .services()
        .get(GetServiceRequest {
            id: deployment.service_id.clone(),
        })
        .await
        .map(|response| response.into_inner())
        .unwrap_or_default();

    let status =
        deployment::Status::try_from(deployment.status).unwrap_or(deployment::Status::Unspecified);
    let live = !status.is_terminal();

    // A finished deployment's output is read once here; a live one is streamed,
    // so the initial render stays cheap and the stream carries the history.
    let lines = if live {
        Vec::new()
    } else {
        collect_deployment_lines(&state, &id).await
    };

    page(
        "Deployment",
        Nav::Deployments,
        render::deployment_detail(&deployment, &service, &lines, live, &user.csrf_token),
    )
}

pub async fn deployment_cancel(
    State(state): State<AppState>,
    Path(id): Path<String>,
    user: CurrentUser,
    Form(form): Form<DeleteForm>,
) -> Response {
    if let Err(rejection) = check_csrf(&user, &form.csrf) {
        return rejection.into_response();
    }

    let mut client = state.api.deployments();

    match client
        .cancel(CancelDeploymentRequest {
            mutation: Some(mutation(
                &user,
                &MutationFlags {
                    allow_latency_critical: form.allow_latency_critical,
                },
            )),
            deployment_id: id.clone(),
        })
        .await
    {
        Ok(_) => Redirect::to(&format!("/deployments/{id}")).into_response(),
        Err(status) => grpc_error(status),
    }
}

/// Reads a finished deployment's stored output.
async fn collect_deployment_lines(
    state: &AppState,
    deployment_id: &str,
) -> Vec<(chrono::DateTime<chrono::Utc>, bool, String)> {
    let mut client = state.api.deployments();

    let Ok(response) = client
        .watch(WatchDeploymentRequest {
            deployment_id: deployment_id.to_string(),
        })
        .await
    else {
        return Vec::new();
    };

    let mut stream = response.into_inner();
    let mut lines = Vec::new();

    // The server replays a finished deployment's history and then closes, so
    // this terminates; the bound guards against a pathological volume.
    while let Some(Ok(event)) = stream.next().await {
        let at = event
            .at
            .as_ref()
            .and_then(nudo_proto::from_timestamp)
            .unwrap_or_else(chrono::Utc::now);

        match event.event {
            Some(deployment_event::Event::OutputLine(line)) => lines.push((at, false, line)),
            Some(deployment_event::Event::StatusChange(status)) => {
                let status =
                    deployment::Status::try_from(status).unwrap_or(deployment::Status::Unspecified);
                lines.push((at, false, format!("--- {} ---", status.as_str())));
            }
            Some(deployment_event::Event::TerminalState(state)) => {
                if !state.error.is_empty() {
                    lines.push((at, true, state.error));
                }
                break;
            }
            None => {}
        }

        if lines.len() >= 5_000 {
            break;
        }
    }

    lines
}

/// The live deployment stream.
///
/// Holds the gRPC `Watch` stream server-side, folds output into a buffer, and
/// pushes a rendered fragment on a fixed tick — so a build that emits thousands
/// of lines a second cannot pin the browser.
pub async fn deployment_stream(
    State(state): State<AppState>,
    Path(id): Path<String>,
    _user: CurrentUser,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let stream = async_stream::stream! {
        let mut client = state.api.deployments();
        let Ok(response) = client
            .watch(WatchDeploymentRequest { deployment_id: id.clone() })
            .await
        else {
            return;
        };
        let mut upstream = response.into_inner();

        // Everything seen so far, so each fragment is a complete pane and htmx
        // can swap rather than append.
        let mut lines: Vec<(chrono::DateTime<chrono::Utc>, bool, String)> = Vec::new();
        let mut dirty = false;
        let mut finished = false;
        let mut ticker = tokio::time::interval(RENDER_INTERVAL);

        loop {
            tokio::select! {
                // Biased so a burst of frames can never starve the render tick;
                // `next()` would otherwise always be ready and nothing would be
                // emitted.
                biased;

                _ = ticker.tick() => {
                    if dirty {
                        dirty = false;
                        let html = render::deployment_log_lines(&lines).into_string();
                        yield Ok(Event::default().event("log").data(html));
                    }
                    if finished {
                        // One last frame has been sent; tell the page to stop.
                        yield Ok(Event::default().event("done").data(""));
                        break;
                    }
                }

                frame = upstream.next() => {
                    match frame {
                        Some(Ok(event)) => {
                            let at = event
                                .at
                                .as_ref()
                                .and_then(nudo_proto::from_timestamp)
                                .unwrap_or_else(chrono::Utc::now);

                            match event.event {
                                Some(deployment_event::Event::OutputLine(line)) => {
                                    lines.push((at, false, line));
                                    dirty = true;
                                }
                                Some(deployment_event::Event::StatusChange(status)) => {
                                    let status = deployment::Status::try_from(status)
                                        .unwrap_or(deployment::Status::Unspecified);
                                    lines.push((at, false, format!("--- {} ---", status.as_str())));
                                    dirty = true;
                                }
                                Some(deployment_event::Event::TerminalState(state)) => {
                                    if !state.error.is_empty() {
                                        lines.push((at, true, state.error));
                                    }
                                    let status = deployment::Status::try_from(state.status)
                                        .unwrap_or(deployment::Status::Unspecified);
                                    lines.push((at, false, format!("--- {} ---", status.as_str())));
                                    dirty = true;
                                    finished = true;
                                }
                                None => {}
                            }

                            // Bound memory for a very chatty build.
                            if lines.len() > 5_000 {
                                let overflow = lines.len() - 5_000;
                                lines.drain(..overflow);
                            }
                        }
                        // Stream ended or errored: emit whatever is pending and
                        // stop. htmx reconnects, and a finished deployment
                        // replays from the store.
                        _ => {
                            if dirty {
                                let html = render::deployment_log_lines(&lines).into_string();
                                yield Ok(Event::default().event("log").data(html));
                            }
                            yield Ok(Event::default().event("done").data(""));
                            break;
                        }
                    }
                }
            }
        }
    };

    Sse::new(stream).keep_alive(KeepAlive::default())
}
