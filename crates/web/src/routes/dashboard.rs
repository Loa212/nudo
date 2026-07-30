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
    let dialog = update_dialog_state(&state, &update, &user).await;
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
            // Last in the document: it is a modal, and nothing below it in
            // source order should be able to sit above it.
            (render::update_dialog(&dialog))
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
    // A skipped release stops asking. Compared by version rather than held as
    // a flag, so the next release brings the banner back on its own.
    let skipped = state.store.skipped_version().await.unwrap_or_default();
    let available = status.available && skipped != status.latest;
    render::UpdateBanner {
        current: status.current,
        latest: status.latest,
        available,
        breaking: status.breaking,
        url: status.url,
    }
}

/// Everything the update dialog shows: the notes for the release being offered,
/// and whether this install can apply it itself.
async fn update_dialog_state(
    state: &AppState,
    banner: &render::UpdateBanner,
    user: &CurrentUser,
) -> render::UpdateDialog {
    if !banner.available {
        return render::UpdateDialog::default();
    }

    // The notes come from the manifest the control plane already recorded, so
    // opening the dialog costs no network call.
    let notes = state
        .updates
        .changelog()
        .await
        .into_iter()
        .find(|release| release.version == banner.latest)
        .map(|release| release.notes)
        .unwrap_or_default();

    // Only a host install can ever upgrade itself; asking the control plane
    // for a container's status would only produce a "not eligible" it already
    // knows.
    let self_upgrade = match nudo_server::updates::InstallKind::detect() {
        nudo_server::updates::InstallKind::Container => None,
        nudo_server::updates::InstallKind::Binary => {
            self_upgrade_status(state).await.map(to_self_upgrade_view)
        }
    };

    render::UpdateDialog {
        current: banner.current.clone(),
        latest: banner.latest.clone(),
        available: true,
        breaking: banner.breaking,
        notes,
        url: banner.url.clone(),
        csrf: user.csrf_token.clone(),
        self_upgrade,
    }
}

/// The form behind "Skip this version".
#[derive(Debug, serde::Deserialize)]
pub struct SkipVersionForm {
    pub version: String,
    pub csrf: String,
}

/// Records a release as decided-about, so the banner stops offering it.
///
/// Not the same as closing the dialog: this survives a reload, and only for
/// this one version — a newer release asks again.
pub async fn upgrade_skip(
    State(state): State<AppState>,
    user: CurrentUser,
    Form(form): Form<SkipVersionForm>,
) -> Response {
    if let Err(rejection) = check_csrf(&user, &form.csrf) {
        return rejection.into_response();
    }

    if let Err(error) = state.store.skip_version(&form.version).await {
        return grpc_error(tonic::Status::internal(format!("{error:#}")));
    }

    state
        .store
        .audit(nudo_server::store::NewAuditEntry {
            actor: Actor::human(user.id.clone(), user.email.clone()),
            action: "Updates.Skip".to_string(),
            subject_id: format!("release/{}", form.version),
            dry_run: false,
            summary: format!("skipped the update to {}", form.version),
        })
        .await;

    Redirect::to("/").into_response()
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
pub async fn upgrade(State(state): State<AppState>, user: CurrentUser) -> Response {
    use nudo_server::updates::InstallKind;

    let status = state.updates.cached_status().await;
    let install = match InstallKind::detect() {
        InstallKind::Container => render::UpgradeInstall::Container { image: NUDO_IMAGE },
        InstallKind::Binary => match self_upgrade_status(&state).await {
            // The control plane says this install runs from the managed
            // layout, so the page can offer to do the upgrade.
            Some(remote) if remote.eligible => render::UpgradeInstall::BinaryManaged {
                status: to_self_upgrade_view(remote),
            },
            // Either a legacy install, or the control plane is unreachable —
            // in which case the manual instructions are the safe thing to
            // show, and they harm nothing.
            _ => render::UpgradeInstall::BinaryLegacy,
        },
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
            csrf: user.csrf_token.clone(),
        }),
    )
}

/// Asks the control plane where self-upgrade stands. `None` when unreachable.
async fn self_upgrade_status(state: &AppState) -> Option<SelfUpgradeStatus> {
    let mut client = state.api.self_upgrade();
    Some(client.get_status(()).await.ok()?.into_inner())
}

fn to_self_upgrade_view(status: SelfUpgradeStatus) -> render::SelfUpgradeView {
    render::SelfUpgradeView {
        state: status.state,
        from_version: status.from_version,
        to_version: status.to_version,
        error: status.error,
        enabled_in_settings: status.enabled_in_settings,
        eligible: status.eligible,
    }
}

/// The form behind the one upgrade button.
#[derive(Debug, serde::Deserialize)]
pub struct UpgradeStartForm {
    pub target_version: String,
    pub csrf: String,
}

/// Starts a self-upgrade. The heavy checking — both opt-ins, eligibility, the
/// anti-rollback ladder, the digest — happens in the control plane; this
/// handler's job is the CSRF check and attributing the click to a person.
pub async fn upgrade_start(
    State(state): State<AppState>,
    user: CurrentUser,
    Form(form): Form<UpgradeStartForm>,
) -> Response {
    if let Err(rejection) = check_csrf(&user, &form.csrf) {
        return rejection.into_response();
    }

    let mutation = mutation(&user, &MutationFlags::default());
    let result = state
        .api
        .self_upgrade()
        .start(StartSelfUpgradeRequest {
            target_version: form.target_version.clone(),
            mutation: Some(mutation),
        })
        .await
        .map(|_| ());

    if let Err(status) = result {
        return grpc_error(status);
    }
    Redirect::to("/upgrade").into_response()
}

/// The polled status fragment.
///
/// An unreachable control plane renders as "restarting" rather than an error:
/// mid-upgrade that is exactly what is happening, and the poll that keeps
/// failing quietly is the poll that eventually reports the new version.
pub async fn upgrade_status(State(state): State<AppState>, _user: CurrentUser) -> Response {
    match self_upgrade_status(&state).await {
        Some(status) => {
            render::self_upgrade_status_fragment(&to_self_upgrade_view(status)).into_response()
        }
        None => render::self_upgrade_restarting().into_response(),
    }
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

/// Turns the self-upgrade opt-in on or off for this instance.
///
/// Audited like the release-check toggle, and more deserving of it: this is
/// half of the permission for the process to replace its own binaries.
pub async fn settings_self_upgrade(
    State(state): State<AppState>,
    user: CurrentUser,
    Form(form): Form<ToggleForm>,
) -> Response {
    if let Err(rejection) = check_csrf(&user, &form.csrf) {
        return rejection.into_response();
    }

    let enabled = form.enabled.is_some();
    if let Err(error) = state.store.set_self_upgrade_enabled(enabled).await {
        return grpc_error(tonic::Status::internal(format!("{error:#}")));
    }

    state
        .store
        .audit(nudo_server::store::NewAuditEntry {
            actor: Actor::human(user.id.clone(), user.email.clone()),
            action: "Settings.SelfUpgrade".to_string(),
            subject_id: "instance".to_string(),
            dry_run: false,
            summary: format!(
                "turned the self-upgrade opt-in {}",
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
