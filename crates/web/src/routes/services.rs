use super::*;

// ---------------------------------------------------------------------------
// Services
// ---------------------------------------------------------------------------

pub async fn services_list(State(state): State<AppState>, _user: CurrentUser) -> Response {
    let services = state.api.list_services("").await;
    let targets = state.api.list_targets().await;
    let statuses = state.api.unit_statuses(&services).await;
    page(
        "Services",
        Nav::Services,
        render::services_list(&services, &targets, &statuses),
    )
}

pub async fn service_new(State(state): State<AppState>, user: CurrentUser) -> Response {
    let targets = state.api.list_targets().await;
    let sources = state.api.list_sources().await;
    let secrets = state.api.list_secrets().await;
    let build_hosts = state.api.list_build_hosts().await;
    page(
        "Add a service",
        Nav::Services,
        render::service_form(
            None,
            &targets,
            &sources,
            &secrets,
            &build_hosts,
            &user.csrf_token,
        ),
    )
}

pub async fn service_detail(
    State(state): State<AppState>,
    Path(id): Path<String>,
    user: CurrentUser,
) -> Response {
    let (service, target) = match load_service_and_target(&state, &id).await {
        Ok(pair) => pair,
        Err(status) => return grpc_error(status),
    };

    let status = single_status(&state, &id).await;
    let releases = state.api.list_releases(&id).await;
    let deployments = state.api.list_deployments(&id, 10).await;

    page(
        &service.name,
        Nav::Services,
        render::service_detail(
            &service,
            &target,
            &status,
            &releases,
            &deployments,
            &user.csrf_token,
        ),
    )
}

pub async fn service_edit(
    State(state): State<AppState>,
    Path(id): Path<String>,
    user: CurrentUser,
) -> Response {
    let mut client = match state.api.services().await {
        Ok(client) => client,
        Err(status) => return grpc_error(status),
    };
    let service = match client.get(GetServiceRequest { id }).await {
        Ok(response) => response.into_inner(),
        Err(status) => return grpc_error(status),
    };

    let targets = state.api.list_targets().await;
    let sources = state.api.list_sources().await;
    let secrets = state.api.list_secrets().await;
    let build_hosts = state.api.list_build_hosts().await;

    page(
        &format!("Edit {}", service.name),
        Nav::Services,
        render::service_form(
            Some(&service),
            &targets,
            &sources,
            &secrets,
            &build_hosts,
            &user.csrf_token,
        ),
    )
}

/// Shows the unit file a deploy would write.
pub async fn service_unit(
    State(state): State<AppState>,
    Path(id): Path<String>,
    _user: CurrentUser,
) -> Response {
    let mut client = match state.api.services().await {
        Ok(client) => client,
        Err(status) => return grpc_error(status),
    };

    let service = match client.get(GetServiceRequest { id: id.clone() }).await {
        Ok(response) => response.into_inner(),
        Err(status) => return grpc_error(status),
    };

    let unit_file = match client
        .render_unit(RenderUnitRequest { service_id: id })
        .await
    {
        Ok(response) => response.into_inner().unit_file,
        Err(status) => return grpc_error(status),
    };

    page(
        &format!("{} — unit", service.name),
        Nav::Services,
        render::service_unit(&service, &unit_file),
    )
}

#[derive(Debug, serde::Deserialize)]
pub struct ServiceForm {
    pub name: String,
    pub target_id: String,
    #[serde(default)]
    pub release_root: String,
    #[serde(default)]
    pub keep_releases: String,

    #[serde(default)]
    pub artifact_kind: String,
    #[serde(default)]
    pub artifact_url: String,
    #[serde(default)]
    pub git_source_id: String,
    #[serde(default)]
    pub git_repo: String,
    #[serde(default)]
    pub git_branch: String,
    #[serde(default)]
    pub git_build_command: String,
    #[serde(default)]
    pub git_artifact_path: String,
    #[serde(default)]
    pub git_auto_deploy: Option<String>,
    /// Where this service builds. Empty means the instance default; the
    /// `local` sentinel pins the control plane.
    #[serde(default)]
    pub git_build_host_id: String,

    #[serde(default)]
    pub unit_name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub exec_args: String,
    #[serde(default)]
    pub working_directory: String,
    #[serde(default)]
    pub unit_user: String,
    #[serde(default)]
    pub unit_group: String,
    #[serde(default)]
    pub restart: String,
    #[serde(default)]
    pub restart_sec: String,
    #[serde(default)]
    pub after: String,
    #[serde(default)]
    pub cpu_affinity: String,
    #[serde(default)]
    pub nice: String,
    #[serde(default)]
    pub io_scheduling_class: String,
    #[serde(default)]
    pub extra_directives: String,

    #[serde(default)]
    pub health_kind: String,
    #[serde(default)]
    pub health_http_url: String,
    #[serde(default)]
    pub health_command: String,
    #[serde(default)]
    pub health_timeout_seconds: String,
    #[serde(default)]
    pub health_retries: String,
    #[serde(default)]
    pub health_initial_delay_seconds: String,

    #[serde(default)]
    pub env: String,
    /// Repeated checkbox: the secret ids this service should receive.
    #[serde(default)]
    pub secret_ids: Vec<String>,

    #[serde(default)]
    pub allow_latency_critical: Option<String>,
    pub csrf: String,
}

impl ServiceForm {
    /// Builds the proto message this form describes.
    pub(super) fn to_service(&self) -> Service {
        let artifact = match self.artifact_kind.as_str() {
            "url" => ArtifactSource {
                kind: Some(artifact_source::Kind::Url(
                    self.artifact_url.trim().to_string(),
                )),
            },
            "git" => ArtifactSource {
                kind: Some(artifact_source::Kind::Git(GitSource {
                    source_id: self.git_source_id.trim().to_string(),
                    repo: self.git_repo.trim().to_string(),
                    branch: self.git_branch.trim().to_string(),
                    build_command: self.git_build_command.trim().to_string(),
                    artifact_path: self.git_artifact_path.trim().to_string(),
                    auto_deploy_on_push: self.git_auto_deploy.is_some(),
                    build_host_id: self.git_build_host_id.trim().to_string(),
                })),
            },
            _ => ArtifactSource {
                kind: Some(artifact_source::Kind::DirectUpload(true)),
            },
        };

        let health_check = HealthCheck {
            kind: Some(match self.health_kind.as_str() {
                "http" => health_check::Kind::HttpUrl(self.health_http_url.trim().to_string()),
                "command" => health_check::Kind::Command(self.health_command.trim().to_string()),
                _ => health_check::Kind::SystemdActive(true),
            }),
            timeout_seconds: self.health_timeout_seconds.trim().parse().unwrap_or(10),
            retries: self.health_retries.trim().parse().unwrap_or(3),
            initial_delay_seconds: self
                .health_initial_delay_seconds
                .trim()
                .parse()
                .unwrap_or(2),
        };

        Service {
            id: String::new(),
            target_id: self.target_id.trim().to_string(),
            name: self.name.trim().to_string(),
            artifact: Some(artifact),
            unit: Some(SystemdUnit {
                unit_name: self.unit_name.trim().to_string(),
                description: self.description.trim().to_string(),
                exec_args: self.exec_args.trim().to_string(),
                working_directory: self.working_directory.trim().to_string(),
                user: self.unit_user.trim().to_string(),
                group: self.unit_group.trim().to_string(),
                restart: self.restart.trim().to_string(),
                restart_sec: self.restart_sec.trim().parse().unwrap_or(5),
                after: self
                    .after
                    .lines()
                    .map(str::trim)
                    .filter(|line| !line.is_empty())
                    .map(str::to_string)
                    .collect(),
                cpu_affinity: self.cpu_affinity.trim().to_string(),
                nice: self.nice.trim().to_string(),
                io_scheduling_class: self.io_scheduling_class.trim().to_string(),
                extra_directives: parse_labels(&self.extra_directives),
            }),
            health_check: Some(health_check),
            release_root: self.release_root.trim().to_string(),
            keep_releases: self.keep_releases.trim().parse().unwrap_or(0),
            secret_ids: self
                .secret_ids
                .iter()
                .map(|id| id.trim().to_string())
                .filter(|id| !id.is_empty())
                .collect(),
            env: parse_labels(&self.env),
            current_release_id: String::new(),
            created_at: None,
        }
    }
}

pub async fn service_create(
    State(state): State<AppState>,
    user: CurrentUser,
    Form(form): Form<ServiceForm>,
) -> Response {
    if let Err(rejection) = check_csrf(&user, &form.csrf) {
        return rejection.into_response();
    }

    let mut client = match state.api.services().await {
        Ok(client) => client,
        Err(status) => return grpc_error(status),
    };

    let result = client
        .create(CreateServiceRequest {
            mutation: Some(mutation(
                &user,
                &MutationFlags {
                    allow_latency_critical: form.allow_latency_critical.clone(),
                },
            )),
            service: Some(form.to_service()),
        })
        .await;

    match result {
        Ok(response) => {
            Redirect::to(&format!("/services/{}", response.into_inner().id)).into_response()
        }
        Err(status) => grpc_error(status),
    }
}

pub async fn service_update(
    State(state): State<AppState>,
    Path(id): Path<String>,
    user: CurrentUser,
    Form(form): Form<ServiceForm>,
) -> Response {
    if let Err(rejection) = check_csrf(&user, &form.csrf) {
        return rejection.into_response();
    }

    let mut client = match state.api.services().await {
        Ok(client) => client,
        Err(status) => return grpc_error(status),
    };

    let result = client
        .update(UpdateServiceRequest {
            mutation: Some(mutation(
                &user,
                &MutationFlags {
                    allow_latency_critical: form.allow_latency_critical.clone(),
                },
            )),
            id: id.clone(),
            service: Some(form.to_service()),
            // The form carries every field, so the whole message applies.
            update_mask: vec![],
        })
        .await;

    match result {
        Ok(_) => Redirect::to(&format!("/services/{id}")).into_response(),
        Err(status) => grpc_error(status),
    }
}

#[derive(Debug, serde::Deserialize)]
pub struct ServiceDeleteForm {
    pub csrf: String,
    #[serde(default)]
    pub stop_and_disable_unit: Option<String>,
    #[serde(default)]
    pub remove_release_dir: Option<String>,
    #[serde(default)]
    pub allow_latency_critical: Option<String>,
}

pub async fn service_delete(
    State(state): State<AppState>,
    Path(id): Path<String>,
    user: CurrentUser,
    Form(form): Form<ServiceDeleteForm>,
) -> Response {
    if let Err(rejection) = check_csrf(&user, &form.csrf) {
        return rejection.into_response();
    }

    let mut client = match state.api.services().await {
        Ok(client) => client,
        Err(status) => return grpc_error(status),
    };

    match client
        .delete(DeleteServiceRequest {
            mutation: Some(mutation(
                &user,
                &MutationFlags {
                    allow_latency_critical: form.allow_latency_critical,
                },
            )),
            id,
            stop_and_disable_unit: form.stop_and_disable_unit.is_some(),
            remove_release_dir: form.remove_release_dir.is_some(),
        })
        .await
    {
        Ok(_) => Redirect::to("/services").into_response(),
        Err(status) => grpc_error(status),
    }
}

#[derive(Debug, serde::Deserialize)]
pub struct UnitActionForm {
    pub action: String,
    pub csrf: String,
    #[serde(default)]
    pub allow_latency_critical: Option<String>,
}

pub async fn service_unit_action(
    State(state): State<AppState>,
    Path(id): Path<String>,
    user: CurrentUser,
    Form(form): Form<UnitActionForm>,
) -> Response {
    if let Err(rejection) = check_csrf(&user, &form.csrf) {
        return rejection.into_response();
    }

    let action = match form.action.as_str() {
        "start" => unit_action_request::Action::Start,
        "stop" => unit_action_request::Action::Stop,
        "restart" => unit_action_request::Action::Restart,
        "reload" => unit_action_request::Action::Reload,
        "enable" => unit_action_request::Action::Enable,
        "disable" => unit_action_request::Action::Disable,
        other => {
            return grpc_error(tonic::Status::invalid_argument(format!(
                "unknown action: {other}"
            )));
        }
    };

    let mut client = match state.api.services().await {
        Ok(client) => client,
        Err(status) => return grpc_error(status),
    };

    match client
        .unit_action(UnitActionRequest {
            mutation: Some(mutation(
                &user,
                &MutationFlags {
                    allow_latency_critical: form.allow_latency_critical,
                },
            )),
            service_id: id.clone(),
            action: action as i32,
        })
        .await
    {
        Ok(_) => Redirect::to(&format!("/services/{id}")).into_response(),
        Err(status) => grpc_error(status),
    }
}

/// Loads a service together with its target.
async fn load_service_and_target(
    state: &AppState,
    id: &str,
) -> Result<(Service, Target), tonic::Status> {
    let mut services = state.api.services().await?;
    let service = services
        .get(GetServiceRequest { id: id.to_string() })
        .await?
        .into_inner();

    let mut targets = state.api.targets().await?;
    let target = targets
        .get(GetTargetRequest {
            id: service.target_id.clone(),
        })
        .await?
        .into_inner();

    Ok((service, target))
}

/// One service's unit state, falling back to "unknown" when unreadable.
async fn single_status(state: &AppState, service_id: &str) -> UnitStatus {
    let fallback = UnitStatus {
        service_id: service_id.to_string(),
        active_state: "unknown".to_string(),
        ..Default::default()
    };

    match state.api.services().await {
        Ok(mut client) => client
            .get_unit_status(GetUnitStatusRequest {
                service_id: service_id.to_string(),
            })
            .await
            .map(|response| response.into_inner())
            .unwrap_or(fallback),
        Err(_) => fallback,
    }
}

/// Live unit status for the services list.
///
/// Holds the `WatchUnitStatus` stream server-side and pushes the rendered table
/// on the same fold-fast/render-slow tick as the other live views — so a status
/// change appears without a reload, and a target with many services cannot make
/// the browser repaint per service.
pub async fn services_stream(
    State(state): State<AppState>,
    _user: CurrentUser,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let stream = async_stream::stream! {
        let Ok(mut client) = state.api.services().await else {
            return;
        };
        let Ok(response) = client
            .watch_unit_status(ListServicesRequest {
                page_size: 200,
                ..Default::default()
            })
            .await
        else {
            return;
        };
        let mut upstream = response.into_inner();

        // The upstream sends one message per service per tick, so they are folded
        // into a snapshot and the table is rendered once per interval rather than
        // once per service.
        let mut statuses: std::collections::HashMap<String, UnitStatus> =
            std::collections::HashMap::new();
        let mut dirty = false;
        let mut ticker = tokio::time::interval(Duration::from_secs(2));

        loop {
            tokio::select! {
                biased;

                _ = ticker.tick() => {
                    if !dirty {
                        continue;
                    }
                    dirty = false;

                    // Re-read the definitions so a service added or removed since
                    // the page loaded is reflected too.
                    let services = state.api.list_services("").await;
                    let targets = state.api.list_targets().await;
                    let html = render::services_rows(&services, &targets, &statuses, true)
                        .into_string();
                    yield Ok(Event::default().event("rows").data(html));
                }

                frame = upstream.next() => {
                    match frame {
                        Some(Ok(status)) => {
                            statuses.insert(status.service_id.clone(), status);
                            dirty = true;
                        }
                        // The stream ended; htmx reconnects on its own.
                        _ => break,
                    }
                }
            }
        }
    };

    Sse::new(stream).keep_alive(KeepAlive::default())
}
