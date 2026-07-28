use super::*;

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

/// How to upgrade this instance, for the way it is actually installed.
pub async fn upgrade(State(state): State<AppState>, _user: CurrentUser) -> Response {
    use nudo_server::updates::InstallKind;

    let status = state.updates.cached_status().await;
    let install = match InstallKind::detect() {
        InstallKind::Container => render::UpgradeInstall::Container { image: NUDO_IMAGE },
        InstallKind::Binary => render::UpgradeInstall::Binary,
    };

    page(
        "Upgrading nudo",
        Nav::Settings,
        render::upgrade_page(&render::UpgradeView {
            current: status.current,
            latest: if status.latest.is_empty() {
                nudo_server::updates::current_version().to_string()
            } else {
                status.latest
            },
            available: status.available,
            breaking: status.breaking,
            install,
        }),
    )
}

/// Where the published image lives. Used only to print an exact `docker pull`.
const NUDO_IMAGE: &str = "ghcr.io/loa212/nudo";

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
