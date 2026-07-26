//! HTTP routes.
//!
//! Read handlers fetch over gRPC and render maud. Mutating handlers check CSRF,
//! call the same gRPC API the CLI and the MCP server use, and redirect. Live
//! views hold a gRPC stream server-side and push rendered HTML fragments over
//! SSE, so the browser holds no gRPC connection and receives only HTML.

use std::convert::Infallible;
use std::time::Duration;

use axum::extract::{Form, Path, Query, State};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{Html, IntoResponse, Redirect, Response};
use futures_util::stream::Stream;
use maud::{Markup, html};
use nudo_proto::*;
use tokio_stream::StreamExt;

use crate::auth::{CurrentUser, check_csrf};
use crate::render::{self, Nav};
use crate::{AppState, terminal};

/// How often a live view repaints, at most.
///
/// The upstream gRPC stream can tick far faster than a browser paints, so frames
/// are folded into "the latest" and rendered on this interval — fold fast, render
/// slow. Without it a log burst pins the browser.
const RENDER_INTERVAL: Duration = Duration::from_millis(120);

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Wraps rendered content in the page shell.
fn page(title: &str, nav: Nav, body: Markup) -> Response {
    Html(render::page(title, nav, body).into_string()).into_response()
}

/// Renders a gRPC failure as a readable page rather than a bare 500.
fn grpc_error(status: tonic::Status) -> Response {
    let code = match status.code() {
        tonic::Code::NotFound => 404,
        tonic::Code::PermissionDenied => 403,
        tonic::Code::Unauthenticated => 401,
        tonic::Code::InvalidArgument | tonic::Code::FailedPrecondition => 400,
        tonic::Code::Unavailable => 503,
        _ => 500,
    };

    let http = axum::http::StatusCode::from_u16(code)
        .unwrap_or(axum::http::StatusCode::INTERNAL_SERVER_ERROR);
    (
        http,
        Html(render::error_page(code, status.message()).into_string()),
    )
        .into_response()
}

/// The mutation envelope for a dashboard action.
fn mutation(user: &CurrentUser, form: &MutationFlags) -> Mutation {
    Mutation {
        actor: Some(Actor::human(
            user.id.clone(),
            // The audit log shows this, so it names the person rather than "web".
            if user.display_name.trim().is_empty() {
                user.email.clone()
            } else {
                format!("{} ({})", user.display_name, user.email)
            },
        )),
        dry_run: false,
        // Opting in is a deliberate act, so it comes from a checkbox on the form
        // rather than being implied by the dashboard being logged in.
        allow_latency_critical: form.allow_latency_critical.is_some(),
        idempotency_key: String::new(),
    }
}

/// The guardrail checkbox, present on every form that can touch a target.
#[derive(Debug, Default, serde::Deserialize)]
pub struct MutationFlags {
    /// `Some` when the checkbox was ticked; HTML omits unchecked boxes entirely.
    pub allow_latency_critical: Option<String>,
}

// ---------------------------------------------------------------------------
// Auth
// ---------------------------------------------------------------------------

#[derive(Debug, serde::Deserialize)]
pub struct LoginForm {
    pub email: String,
    pub password: String,
    pub csrf: String,
}

#[derive(Debug, serde::Deserialize)]
pub struct SetupForm {
    pub email: String,
    pub password: String,
    pub display_name: String,
    pub csrf: String,
}

/// The login page, or the first-run setup page when no user exists yet.
pub async fn login_page(State(state): State<AppState>) -> Response {
    let has_users = state.store.has_users().await.unwrap_or(true);

    // A CSRF token for a form submitted before any session exists. Bound to a
    // cookie would be circular here, so the pre-auth token is a fixed value and
    // the real defence for these two forms is that they create rather than
    // change state, plus the same-site cookie on everything after.
    let csrf = crate::PRE_AUTH_CSRF;

    if !has_users && state.allow_setup {
        return Html(
            render::page(
                "Set up nudo",
                Nav::Dashboard,
                render::setup_page(None, csrf),
            )
            .into_string(),
        )
        .into_response();
    }

    Html(render::page("Sign in", Nav::Dashboard, render::login_page(None, csrf)).into_string())
        .into_response()
}

/// Handles a login.
pub async fn login(
    State(state): State<AppState>,
    jar: axum_extra::extract::CookieJar,
    Form(form): Form<LoginForm>,
) -> Response {
    if form.csrf != crate::PRE_AUTH_CSRF {
        return crate::auth::CsrfRejected.into_response();
    }

    let user = match state.store.authenticate(&form.email, &form.password).await {
        Ok(Some(user)) => user,
        Ok(None) => {
            // One message for both a missing account and a wrong password, so
            // the response cannot be used to enumerate users.
            return Html(
                render::page(
                    "Sign in",
                    Nav::Dashboard,
                    render::login_page(
                        Some("That email and password do not match an account."),
                        crate::PRE_AUTH_CSRF,
                    ),
                )
                .into_string(),
            )
            .into_response();
        }
        Err(error) => {
            tracing::error!(%error, "authentication failed");
            return grpc_error(tonic::Status::internal("could not sign in"));
        }
    };

    let (cookie, _) = match state.store.create_session(&user.id).await {
        Ok(session) => session,
        Err(error) => {
            tracing::error!(%error, "creating the session failed");
            return grpc_error(tonic::Status::internal("could not sign in"));
        }
    };

    let jar = jar.add(crate::auth::session_cookie(
        cookie,
        crate::auth::is_https(&state.base_url),
    ));
    (jar, Redirect::to("/")).into_response()
}

/// Creates the first admin.
pub async fn setup(
    State(state): State<AppState>,
    jar: axum_extra::extract::CookieJar,
    Form(form): Form<SetupForm>,
) -> Response {
    if form.csrf != crate::PRE_AUTH_CSRF {
        return crate::auth::CsrfRejected.into_response();
    }

    // Closed once an account exists, so this cannot be used to add a second
    // admin without authenticating.
    match state.store.has_users().await {
        Ok(true) => return Redirect::to("/login").into_response(),
        Ok(false) => {}
        Err(error) => {
            tracing::error!(%error, "checking for existing users failed");
            return grpc_error(tonic::Status::internal("could not complete setup"));
        }
    }
    if !state.allow_setup {
        return grpc_error(tonic::Status::permission_denied(
            "first-run setup is disabled on this instance",
        ));
    }

    let user = match state
        .store
        .create_user(&form.email, &form.password, &form.display_name)
        .await
    {
        Ok(user) => user,
        Err(error) => {
            return Html(
                render::page(
                    "Set up nudo",
                    Nav::Dashboard,
                    render::setup_page(Some(&format!("{error:#}")), crate::PRE_AUTH_CSRF),
                )
                .into_string(),
            )
            .into_response();
        }
    };

    let (cookie, _) = match state.store.create_session(&user.id).await {
        Ok(session) => session,
        Err(error) => {
            tracing::error!(%error, "creating the session failed");
            return Redirect::to("/login").into_response();
        }
    };

    let jar = jar.add(crate::auth::session_cookie(
        cookie,
        crate::auth::is_https(&state.base_url),
    ));
    (jar, Redirect::to("/")).into_response()
}

/// Ends the session.
pub async fn logout(
    State(state): State<AppState>,
    jar: axum_extra::extract::CookieJar,
    parts: axum::http::request::Parts,
) -> Response {
    if let Some(cookie) = crate::auth::cookie_value(&parts) {
        let _ = state.store.delete_session(&cookie).await;
    }
    let jar = jar.add(crate::auth::clear_cookie(crate::auth::is_https(
        &state.base_url,
    )));
    (jar, Redirect::to("/login")).into_response()
}

// ---------------------------------------------------------------------------
// Dashboard
// ---------------------------------------------------------------------------

pub async fn dashboard(State(state): State<AppState>, user: CurrentUser) -> Response {
    let targets = state.api.list_targets().await;
    let services = state.api.list_services("").await;
    let statuses = state.api.unit_statuses(&services).await;
    let recent = state.api.list_deployments("", 8).await;

    // Both banners read state the control plane has already collected, so the
    // dashboard renders at the same speed whether or not either is shown.
    let update = update_banner_state(&state).await;
    let show_support = crate::support::should_prompt(&state.store, &user.id).await;

    page(
        "Dashboard",
        Nav::Dashboard,
        html! {
            (render::update_banner(&update))
            @if show_support {
                (render::support_banner(&user.csrf_token, support_links()))
            }
            (render::dashboard(&targets, &services, &statuses, &recent))
        },
    )
}

/// The links the support banner points at.
fn support_links() -> render::SupportLinkView<'static> {
    use crate::support::SupportLinks;
    render::SupportLinkView {
        sponsor: SupportLinks::SPONSOR,
        repository: SupportLinks::REPOSITORY,
        issues: SupportLinks::ISSUES,
        discussions: SupportLinks::DISCUSSIONS,
    }
}

/// Reads the last release check out of the database.
///
/// Never fetches: the background check owns the network, so a slow or
/// unreachable manifest cannot hold up a page render.
async fn update_banner_state(state: &AppState) -> render::UpdateBanner {
    let status = state.updates.cached_status().await;
    render::UpdateBanner {
        current: status.current,
        latest: status.latest,
        available: status.available,
        breaking: status.breaking,
        url: status.url,
    }
}

/// The "What's new" page.
pub async fn changelog(State(state): State<AppState>, _user: CurrentUser) -> Response {
    let running = nudo_server::updates::current_version();
    let entries = state
        .updates
        .changelog()
        .await
        .into_iter()
        .map(|release| render::ChangelogEntry {
            current: release.version.trim_start_matches('v') == running,
            version: release.version,
            published_at: release.published_at,
            notes: release.notes,
            url: release.url,
            breaking: release.breaking,
        })
        .collect::<Vec<_>>();

    page(
        "What's new",
        Nav::Settings,
        render::changelog_page(&entries, running),
    )
}

/// Dismisses the support banner until the following calendar month.
pub async fn support_dismiss(
    State(state): State<AppState>,
    user: CurrentUser,
    Form(form): Form<CsrfOnlyForm>,
) -> Response {
    if let Err(rejection) = check_csrf(&user, &form.csrf) {
        return rejection.into_response();
    }

    // A failure here means the banner shows again next time, which is a mild
    // annoyance rather than something worth an error page.
    if let Err(error) = state.store.dismiss_support_prompt(&user.id).await {
        tracing::warn!(%error, "could not record the support-prompt dismissal");
    }

    Redirect::to("/").into_response()
}

#[derive(Debug, serde::Deserialize)]
pub struct CsrfOnlyForm {
    pub csrf: String,
}

/// A settings checkbox: present when ticked, absent when not.
#[derive(Debug, serde::Deserialize)]
pub struct ToggleForm {
    #[serde(default)]
    pub enabled: Option<String>,
    pub csrf: String,
}

/// Turns the release check on or off for this instance.
pub async fn settings_updates(
    State(state): State<AppState>,
    user: CurrentUser,
    Form(form): Form<ToggleForm>,
) -> Response {
    if let Err(rejection) = check_csrf(&user, &form.csrf) {
        return rejection.into_response();
    }

    let enabled = form.enabled.is_some();
    if let Err(error) = state.store.set_release_check_enabled(enabled).await {
        return grpc_error(tonic::Status::internal(format!("{error:#}")));
    }

    state
        .store
        .audit(nudo_server::store::NewAuditEntry {
            actor: Actor::human(user.id.clone(), user.email.clone()),
            action: "Settings.ReleaseCheck".to_string(),
            subject_id: "instance".to_string(),
            dry_run: false,
            summary: format!(
                "turned the release check {}",
                if enabled { "on" } else { "off" }
            ),
        })
        .await;

    Redirect::to("/settings#instance").into_response()
}

/// Turns the support prompt on or off for this instance.
pub async fn settings_support(
    State(state): State<AppState>,
    user: CurrentUser,
    Form(form): Form<ToggleForm>,
) -> Response {
    if let Err(rejection) = check_csrf(&user, &form.csrf) {
        return rejection.into_response();
    }

    let enabled = form.enabled.is_some();
    if let Err(error) = state.store.set_support_prompt_enabled(enabled).await {
        return grpc_error(tonic::Status::internal(format!("{error:#}")));
    }

    Redirect::to("/settings#instance").into_response()
}

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
    _user: CurrentUser,
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
        render::target_detail(&target, &services, &statuses, None),
    )
}

/// Runs the four-part readiness check and re-renders the page with its results.
pub async fn target_check(
    State(state): State<AppState>,
    Path(id): Path<String>,
    _user: CurrentUser,
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

    let services = state.api.list_services(&id).await;
    let statuses = state.api.unit_statuses(&services).await;

    page(
        &target.name,
        Nav::Targets,
        render::target_detail(&target, &services, &statuses, Some(&checks)),
    )
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
fn parse_labels(raw: &str) -> std::collections::HashMap<String, String> {
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
    page(
        "Add a service",
        Nav::Services,
        render::service_form(None, &targets, &sources, &secrets, &user.csrf_token),
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

    page(
        &format!("Edit {}", service.name),
        Nav::Services,
        render::service_form(
            Some(&service),
            &targets,
            &sources,
            &secrets,
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
    fn to_service(&self) -> Service {
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

    let mut client = match state.api.deployments().await {
        Ok(client) => client,
        Err(status) => return grpc_error(status),
    };

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

    let mut client = match state.api.deployments().await {
        Ok(client) => client,
        Err(status) => return grpc_error(status),
    };

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
    let mut client = match state.api.deployments().await {
        Ok(client) => client,
        Err(status) => return grpc_error(status),
    };

    let deployment = match client.get(GetDeploymentRequest { id: id.clone() }).await {
        Ok(response) => response.into_inner(),
        Err(status) => return grpc_error(status),
    };

    let service = match state.api.services().await {
        Ok(mut services) => services
            .get(GetServiceRequest {
                id: deployment.service_id.clone(),
            })
            .await
            .map(|response| response.into_inner())
            .unwrap_or_default(),
        Err(_) => Service::default(),
    };

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

    let mut client = match state.api.deployments().await {
        Ok(client) => client,
        Err(status) => return grpc_error(status),
    };

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
    let Ok(mut client) = state.api.deployments().await else {
        return Vec::new();
    };

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
        let Ok(mut client) = state.api.deployments().await else {
            return;
        };
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

// ---------------------------------------------------------------------------
// Logs
// ---------------------------------------------------------------------------

#[derive(Debug, Default, serde::Deserialize)]
pub struct LogsQuery {
    #[serde(default)]
    pub grep: Option<String>,
    #[serde(default)]
    pub lines: Option<u32>,
    #[serde(default)]
    pub follow: Option<String>,
}

pub async fn logs_view(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(query): Query<LogsQuery>,
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

    let grep = query.grep.clone().unwrap_or_default();
    let follow = query.follow.is_some();
    let tail = query.lines.unwrap_or(200);

    // A one-shot read for the initial paint; following is the SSE stream's job.
    let lines = read_logs_once(&state, &id, tail, &grep).await;

    page(
        &format!("{} — logs", service.name),
        Nav::Services,
        render::logs_view(&service, &lines, &grep, follow),
    )
}

/// Reads a bounded batch of log lines.
async fn read_logs_once(state: &AppState, service_id: &str, tail: u32, grep: &str) -> Vec<LogLine> {
    let Ok(mut client) = state.api.logs().await else {
        return Vec::new();
    };

    let Ok(response) = client
        .stream(StreamLogsRequest {
            service_id: service_id.to_string(),
            follow: false,
            tail_lines: tail,
            since_cursor: String::new(),
            since: None,
            grep: grep.to_string(),
        })
        .await
    else {
        return Vec::new();
    };

    let mut stream = response.into_inner();
    let mut lines = Vec::new();

    // A non-following read ends on its own, but a slow or wedged target should
    // not hold the page load, so the wait is bounded.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            break;
        }
        match tokio::time::timeout(remaining, stream.next()).await {
            Ok(Some(Ok(line))) => {
                lines.push(line);
                if lines.len() >= tail.max(1) as usize {
                    break;
                }
            }
            // End of stream, an error, or the deadline.
            _ => break,
        }
    }

    lines
}

/// The live log stream, folded and rendered on a tick.
pub async fn logs_stream(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(query): Query<LogsQuery>,
    _user: CurrentUser,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let grep = query.grep.unwrap_or_default();
    let tail = query.lines.unwrap_or(200);

    let stream = async_stream::stream! {
        let Ok(mut client) = state.api.logs().await else {
            return;
        };
        let Ok(response) = client
            .stream(StreamLogsRequest {
                service_id: id.clone(),
                follow: true,
                tail_lines: tail,
                since_cursor: String::new(),
                since: None,
                grep,
            })
            .await
        else {
            return;
        };
        let mut upstream = response.into_inner();

        // A window rather than everything: a service running for a week must not
        // grow this task's memory without bound.
        const WINDOW: usize = 1_000;
        let mut lines: std::collections::VecDeque<LogLine> = std::collections::VecDeque::new();
        let mut dirty = false;
        let mut ticker = tokio::time::interval(RENDER_INTERVAL);

        loop {
            tokio::select! {
                biased;

                _ = ticker.tick() => {
                    if dirty {
                        dirty = false;
                        let snapshot: Vec<LogLine> = lines.iter().cloned().collect();
                        let html = render::log_lines(&snapshot).into_string();
                        yield Ok(Event::default().event("log").data(html));
                    }
                }

                frame = upstream.next() => {
                    match frame {
                        Some(Ok(line)) => {
                            if lines.len() >= WINDOW {
                                lines.pop_front();
                            }
                            lines.push_back(line);
                            dirty = true;
                        }
                        _ => break,
                    }
                }
            }
        }
    };

    Sse::new(stream).keep_alive(KeepAlive::default())
}

// ---------------------------------------------------------------------------
// Terminal
// ---------------------------------------------------------------------------

/// Mints a terminal grant and renders the page that will spend it.
pub async fn terminal_page(
    State(state): State<AppState>,
    Path(id): Path<String>,
    user: CurrentUser,
) -> Response {
    let mut targets = match state.api.targets().await {
        Ok(client) => client,
        Err(status) => return grpc_error(status),
    };
    let target = match targets.get(GetTargetRequest { id: id.clone() }).await {
        Ok(response) => response.into_inner(),
        Err(status) => return grpc_error(status),
    };

    let mut client = match state.api.terminals().await {
        Ok(client) => client,
        Err(status) => return grpc_error(status),
    };

    let session = match client
        .create_session(CreateTerminalSessionRequest {
            mutation: Some(mutation(
                &user,
                // Opening a shell on a latency-critical box is a deliberate act,
                // so it is allowed here — the operator navigated to this page
                // for that target on purpose, and the grant is audited.
                &MutationFlags {
                    allow_latency_critical: target.latency_critical.then(|| "on".to_string()),
                },
            )),
            target_id: id,
            initial_command: String::new(),
            cols: 120,
            rows: 32,
        })
        .await
    {
        Ok(response) => response.into_inner(),
        Err(status) => return grpc_error(status),
    };

    page(
        &format!("{} — terminal", target.name),
        Nav::Terminal,
        render::terminal_page(&target, &session.id, &session.token),
    )
}

/// The target chooser, when no target was named.
pub async fn terminal_index(State(state): State<AppState>, _user: CurrentUser) -> Response {
    let targets = state.api.list_targets().await;
    page("Terminal", Nav::Terminal, render::targets_list(&targets))
}

pub use terminal::websocket as terminal_websocket;

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

// ---------------------------------------------------------------------------
// Audit and settings
// ---------------------------------------------------------------------------

#[derive(Debug, Default, serde::Deserialize)]
pub struct AuditQuery {
    #[serde(default)]
    pub subject: Option<String>,
}

pub async fn audit_list(
    State(state): State<AppState>,
    Query(query): Query<AuditQuery>,
    _user: CurrentUser,
) -> Response {
    let mut client = match state.api.audit().await {
        Ok(client) => client,
        Err(status) => return grpc_error(status),
    };

    let entries = client
        .list(ListAuditRequest {
            subject_id: query.subject.unwrap_or_default(),
            actor_kind: actor::Kind::Unspecified as i32,
            page_size: 100,
            page_token: String::new(),
        })
        .await
        .map(|response| response.into_inner().entries)
        .unwrap_or_default();

    page("Audit log", Nav::Audit, render::audit_list(&entries))
}

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

// ---------------------------------------------------------------------------
// Assets and errors
// ---------------------------------------------------------------------------

pub async fn asset(Path(name): Path<String>) -> Response {
    crate::assets::serve(&name).await
}

pub async fn not_found() -> Response {
    (
        axum::http::StatusCode::NOT_FOUND,
        Html(render::error_page(404, "That page does not exist.").into_string()),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn labels_parse_one_per_line_and_ignore_junk() {
        let parsed = parse_labels("env=prod\nrole = indexer \n\nnonsense\n=empty-key\n");
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed.get("env").map(String::as_str), Some("prod"));
        assert_eq!(parsed.get("role").map(String::as_str), Some("indexer"));
    }

    #[test]
    fn a_label_value_may_contain_an_equals_sign() {
        let parsed = parse_labels("connection=host=db;port=5432");
        assert_eq!(
            parsed.get("connection").map(String::as_str),
            Some("host=db;port=5432")
        );
    }

    #[test]
    fn an_unticked_checkbox_does_not_grant_the_latency_critical_override() {
        // HTML omits unchecked boxes entirely, so absence must mean "no".
        let user = CurrentUser {
            id: "usr_1".to_string(),
            email: "a@b.com".to_string(),
            display_name: "Alice".to_string(),
            csrf_token: "csrf".to_string(),
        };

        let without = mutation(&user, &MutationFlags::default());
        assert!(!without.allow_latency_critical);

        let with = mutation(
            &user,
            &MutationFlags {
                allow_latency_critical: Some("on".to_string()),
            },
        );
        assert!(with.allow_latency_critical);
    }

    #[test]
    fn a_dashboard_mutation_is_attributed_to_the_signed_in_person() {
        let user = CurrentUser {
            id: "usr_1".to_string(),
            email: "alice@example.com".to_string(),
            display_name: "Alice".to_string(),
            csrf_token: "csrf".to_string(),
        };

        let envelope = mutation(&user, &MutationFlags::default());
        let actor = envelope.actor.expect("actor");
        assert_eq!(actor.kind, actor::Kind::Human as i32);
        assert_eq!(actor.id, "usr_1");
        // The audit log has to identify a person, not "the dashboard".
        assert!(actor.label.contains("Alice"));
        assert!(actor.label.contains("alice@example.com"));
        // The dashboard never dry-runs; it acts.
        assert!(!envelope.dry_run);
    }

    #[test]
    fn a_user_with_no_display_name_is_identified_by_email() {
        let user = CurrentUser {
            id: "usr_1".to_string(),
            email: "alice@example.com".to_string(),
            display_name: "  ".to_string(),
            csrf_token: "csrf".to_string(),
        };
        let actor = mutation(&user, &MutationFlags::default())
            .actor
            .expect("actor");
        assert_eq!(actor.label, "alice@example.com");
    }

    #[test]
    fn grpc_codes_map_onto_meaningful_http_statuses() {
        let cases = [
            (tonic::Code::NotFound, 404),
            (tonic::Code::PermissionDenied, 403),
            (tonic::Code::Unauthenticated, 401),
            (tonic::Code::InvalidArgument, 400),
            // The latency-critical refusal arrives as FailedPrecondition, and a
            // 400 tells the operator it is their request that needs changing.
            (tonic::Code::FailedPrecondition, 400),
            (tonic::Code::Unavailable, 503),
            (tonic::Code::Internal, 500),
        ];

        for (code, expected) in cases {
            let response = grpc_error(tonic::Status::new(code, "message"));
            assert_eq!(response.status().as_u16(), expected, "{code:?}");
        }
    }

    #[test]
    fn a_service_form_builds_each_artifact_kind() {
        let base = || ServiceForm {
            name: "bot".to_string(),
            target_id: "tgt_1".to_string(),
            release_root: String::new(),
            keep_releases: String::new(),
            artifact_kind: String::new(),
            artifact_url: String::new(),
            git_source_id: String::new(),
            git_repo: String::new(),
            git_branch: String::new(),
            git_build_command: String::new(),
            git_artifact_path: String::new(),
            git_auto_deploy: None,
            unit_name: String::new(),
            description: String::new(),
            exec_args: String::new(),
            working_directory: String::new(),
            unit_user: String::new(),
            unit_group: String::new(),
            restart: String::new(),
            restart_sec: String::new(),
            after: String::new(),
            cpu_affinity: String::new(),
            nice: String::new(),
            io_scheduling_class: String::new(),
            extra_directives: String::new(),
            health_kind: String::new(),
            health_http_url: String::new(),
            health_command: String::new(),
            health_timeout_seconds: String::new(),
            health_retries: String::new(),
            health_initial_delay_seconds: String::new(),
            env: String::new(),
            secret_ids: Vec::new(),
            allow_latency_critical: None,
            csrf: "csrf".to_string(),
        };

        // URL.
        let url = ServiceForm {
            artifact_kind: "url".to_string(),
            artifact_url: " https://example.com/bot ".to_string(),
            ..base()
        }
        .to_service();
        assert!(matches!(
            url.artifact.expect("artifact").kind,
            Some(artifact_source::Kind::Url(u)) if u == "https://example.com/bot"
        ));

        // Git, with auto-deploy from a ticked checkbox.
        let git = ServiceForm {
            artifact_kind: "git".to_string(),
            git_repo: "owner/bot".to_string(),
            git_branch: "main".to_string(),
            git_auto_deploy: Some("on".to_string()),
            ..base()
        }
        .to_service();
        match git.artifact.expect("artifact").kind {
            Some(artifact_source::Kind::Git(source)) => {
                assert_eq!(source.repo, "owner/bot");
                assert!(source.auto_deploy_on_push);
            }
            other => panic!("expected git, got {other:?}"),
        }

        // Anything else means the CLI will push a binary.
        let upload = base().to_service();
        assert!(matches!(
            upload.artifact.expect("artifact").kind,
            Some(artifact_source::Kind::DirectUpload(true))
        ));
    }

    #[test]
    fn a_service_form_builds_each_health_check_kind_with_defaults() {
        let mut form = ServiceForm {
            name: "bot".to_string(),
            target_id: "tgt_1".to_string(),
            health_kind: "http".to_string(),
            health_http_url: "http://127.0.0.1:9000/healthz".to_string(),
            csrf: "csrf".to_string(),
            release_root: String::new(),
            keep_releases: String::new(),
            artifact_kind: String::new(),
            artifact_url: String::new(),
            git_source_id: String::new(),
            git_repo: String::new(),
            git_branch: String::new(),
            git_build_command: String::new(),
            git_artifact_path: String::new(),
            git_auto_deploy: None,
            unit_name: String::new(),
            description: String::new(),
            exec_args: String::new(),
            working_directory: String::new(),
            unit_user: String::new(),
            unit_group: String::new(),
            restart: String::new(),
            restart_sec: String::new(),
            after: String::new(),
            cpu_affinity: String::new(),
            nice: String::new(),
            io_scheduling_class: String::new(),
            extra_directives: String::new(),
            health_command: String::new(),
            health_timeout_seconds: String::new(),
            health_retries: String::new(),
            health_initial_delay_seconds: String::new(),
            env: String::new(),
            secret_ids: Vec::new(),
            allow_latency_critical: None,
        };

        let http = form.to_service().health_check.expect("health");
        assert!(matches!(http.kind, Some(health_check::Kind::HttpUrl(_))));
        // Blank numeric fields become working defaults, not zeros.
        assert_eq!(http.timeout_seconds, 10);
        assert_eq!(http.retries, 3);

        form.health_kind = "command".to_string();
        form.health_command = "/usr/bin/true".to_string();
        assert!(matches!(
            form.to_service().health_check.expect("health").kind,
            Some(health_check::Kind::Command(c)) if c == "/usr/bin/true"
        ));

        form.health_kind = "systemd".to_string();
        assert!(matches!(
            form.to_service().health_check.expect("health").kind,
            Some(health_check::Kind::SystemdActive(true))
        ));
    }

    #[test]
    fn a_service_form_carries_the_latency_knobs_and_the_unit_shape() {
        let form = ServiceForm {
            name: "hft".to_string(),
            target_id: "tgt_1".to_string(),
            cpu_affinity: " 2-5 ".to_string(),
            nice: "-15".to_string(),
            io_scheduling_class: "realtime".to_string(),
            extra_directives: "LimitNOFILE=1048576\nMemoryMax=8G".to_string(),
            after: "postgresql.service\n\n".to_string(),
            restart: "on-failure".to_string(),
            restart_sec: "30".to_string(),
            env: "LOG_LEVEL=info".to_string(),
            secret_ids: vec!["sec_1".to_string(), "  ".to_string()],
            keep_releases: "9".to_string(),
            csrf: "csrf".to_string(),
            release_root: String::new(),
            artifact_kind: String::new(),
            artifact_url: String::new(),
            git_source_id: String::new(),
            git_repo: String::new(),
            git_branch: String::new(),
            git_build_command: String::new(),
            git_artifact_path: String::new(),
            git_auto_deploy: None,
            unit_name: String::new(),
            description: String::new(),
            exec_args: String::new(),
            working_directory: String::new(),
            unit_user: String::new(),
            unit_group: String::new(),
            health_kind: String::new(),
            health_http_url: String::new(),
            health_command: String::new(),
            health_timeout_seconds: String::new(),
            health_retries: String::new(),
            health_initial_delay_seconds: String::new(),
            allow_latency_critical: None,
        };

        let service = form.to_service();
        let unit = service.unit.expect("unit");

        assert_eq!(unit.cpu_affinity, "2-5", "whitespace is trimmed");
        assert_eq!(unit.nice, "-15");
        assert_eq!(unit.io_scheduling_class, "realtime");
        assert_eq!(unit.restart, "on-failure");
        assert_eq!(unit.restart_sec, 30);
        assert_eq!(unit.after, vec!["postgresql.service".to_string()]);
        assert_eq!(
            unit.extra_directives.get("LimitNOFILE").map(String::as_str),
            Some("1048576")
        );

        assert_eq!(service.keep_releases, 9);
        assert_eq!(
            service.env.get("LOG_LEVEL").map(String::as_str),
            Some("info")
        );
        // Blank checkbox values are dropped rather than stored as empty ids.
        assert_eq!(service.secret_ids, vec!["sec_1".to_string()]);
    }
}
