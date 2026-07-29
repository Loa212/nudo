use super::*;

fn an_upgrade(install: UpgradeInstall) -> UpgradeView {
    UpgradeView {
        current: "0.1.0".to_string(),
        latest: "0.2.0".to_string(),
        available: true,
        breaking: false,
        install,
        csrf: "token".to_string(),
    }
}

/// A managed install with both opt-ins given and nothing in flight.
fn a_managed_status() -> SelfUpgradeView {
    SelfUpgradeView {
        state: "idle".to_string(),
        allowed_by_config: true,
        enabled_in_settings: true,
        eligible: true,
        ..SelfUpgradeView::default()
    }
}

#[test]
fn the_banner_points_at_the_instructions() {
    let rendered = s(update_banner(&an_update(true, false)));
    assert!(rendered.contains(r#"href="/upgrade""#));
}

#[test]
fn a_containerised_instance_is_told_to_pull_an_image() {
    let rendered = s(upgrade_page(&an_upgrade(UpgradeInstall::Container {
        image: "ghcr.io/loa212/nudo",
    })));
    assert!(rendered.contains("docker pull ghcr.io/loa212/nudo:0.2.0"));
    assert!(rendered.contains("docker compose pull"));
    // Instructions for the other kind would be actively misleading here.
    assert!(
        !rendered.contains("systemctl stop nudo"),
        "a container was told to restart a systemd unit"
    );
}

#[test]
fn a_binary_instance_is_told_to_verify_what_it_downloaded() {
    let rendered = s(upgrade_page(&an_upgrade(UpgradeInstall::BinaryLegacy)));
    assert!(rendered.contains("sha256sum -c"), "no checksum step");
    assert!(rendered.contains("systemctl stop nudo"));
    assert!(rendered.contains("systemctl start nudo"));
    assert!(
        !rendered.contains("docker pull"),
        "a host install was told to pull an image"
    );
}

#[test]
fn the_page_answers_whether_upgrading_loses_anything() {
    // The first question anyone has. Both install kinds must answer it.
    for install in [
        UpgradeInstall::Container {
            image: "ghcr.io/loa212/nudo",
        },
        UpgradeInstall::BinaryLegacy,
    ] {
        let rendered = s(upgrade_page(&an_upgrade(install)));
        assert!(
            rendered.contains("Your data is not touched"),
            "the page does not say whether data survives"
        );
        // The generated-key trap, which is the one way someone can actually
        // lose something.
        assert!(rendered.contains("secret key"));
        // And what to do when it goes wrong.
        assert!(rendered.contains("If it goes wrong"));
    }
}

#[test]
fn a_breaking_release_is_called_out_on_the_upgrade_page_too() {
    let mut view = an_upgrade(UpgradeInstall::BinaryLegacy);
    view.breaking = true;
    let rendered = s(upgrade_page(&view));
    assert!(rendered.contains("needs manual steps"));
}

#[test]
fn an_up_to_date_instance_is_not_told_to_pull_the_version_it_runs() {
    // `docker pull ...:0.1.0` while running 0.1.0 is a no-op dressed up as
    // an instruction.
    let mut view = an_upgrade(UpgradeInstall::Container {
        image: "ghcr.io/loa212/nudo",
    });
    view.available = false;
    let rendered = s(upgrade_page(&view));
    assert!(rendered.contains("docker pull ghcr.io/loa212/nudo:latest"));
    assert!(!rendered.contains(":0.1.0"));

    // The binary snippet becomes an illustration rather than something to
    // paste, and says which version to substitute.
    let mut binary = an_upgrade(UpgradeInstall::BinaryLegacy);
    binary.available = false;
    let rendered = s(upgrade_page(&binary));
    assert!(rendered.contains("version=X.Y.Z"));
    assert!(!rendered.contains("version=latest"));
}

#[test]
fn an_up_to_date_instance_still_gets_the_instructions() {
    // Reached from a bookmark or the nav rather than the banner. Saying
    // "nothing to do" and hiding the steps would be a dead end.
    let mut view = an_upgrade(UpgradeInstall::BinaryLegacy);
    view.available = false;
    let rendered = s(upgrade_page(&view));
    assert!(rendered.contains("You are up to date"));
    assert!(rendered.contains("sha256sum -c"), "the steps are hidden");
}

#[test]
fn the_upgrade_page_never_pipes_anything_into_a_shell() {
    // The property that survives the self-upgrade button: whatever the page
    // offers, it never fetches a script and never pipes into a shell. The
    // tool this was modelled on curls upgrade.sh from a CDN and runs it as
    // root; the managed path here downloads an artifact, verifies it against
    // the digest committed to the repository, and execs it — no shell at any
    // point, and this test is what keeps it that way.
    for install in [
        UpgradeInstall::Container {
            image: "ghcr.io/loa212/nudo",
        },
        UpgradeInstall::BinaryLegacy,
        UpgradeInstall::BinaryManaged {
            status: a_managed_status(),
        },
    ] {
        let rendered = s(upgrade_page(&an_upgrade(install)));
        assert!(
            !rendered.contains("curl -fsSL") && !rendered.contains("| sh"),
            "the page pipes a downloaded script into a shell"
        );
        assert!(
            !rendered.contains("install.sh"),
            "the page references a fetched install script"
        );
    }
}

#[test]
fn only_a_fully_opted_in_managed_install_gets_the_button() {
    // A form on this page is a big deal — it used to be banned outright. It
    // appears in exactly one configuration: managed layout, config flag on,
    // settings toggle on, newer release available. Everything else stays a
    // page of instructions.
    let with_button = s(upgrade_page(&an_upgrade(UpgradeInstall::BinaryManaged {
        status: a_managed_status(),
    })));
    assert_eq!(
        with_button.matches("<form").count(),
        1,
        "exactly one form: the upgrade button"
    );
    assert!(with_button.contains(r#"action="/upgrade/start""#));
    assert!(
        with_button.contains(r#"value="0.2.0""#),
        "the form authorises the version the page showed"
    );

    // Container and legacy installs: no form at all.
    for install in [
        UpgradeInstall::Container {
            image: "ghcr.io/loa212/nudo",
        },
        UpgradeInstall::BinaryLegacy,
    ] {
        let rendered = s(upgrade_page(&an_upgrade(install)));
        assert!(
            !rendered.contains("<form"),
            "only the managed install may offer to act"
        );
    }

    // Managed but a gate closed: no form, and it says which gate.
    let mut flag_off = a_managed_status();
    flag_off.allowed_by_config = false;
    let rendered = s(upgrade_page(&an_upgrade(UpgradeInstall::BinaryManaged {
        status: flag_off,
    })));
    assert!(!rendered.contains("<form"));
    assert!(rendered.contains("NUDO_ALLOW_SELF_UPGRADE"));

    let mut toggle_off = a_managed_status();
    toggle_off.enabled_in_settings = false;
    let rendered = s(upgrade_page(&an_upgrade(UpgradeInstall::BinaryManaged {
        status: toggle_off,
    })));
    assert!(!rendered.contains("<form"));
    assert!(rendered.contains("settings"));

    // Managed, gates open, but nothing newer: no form either.
    let mut current = an_upgrade(UpgradeInstall::BinaryManaged {
        status: a_managed_status(),
    });
    current.available = false;
    let rendered = s(upgrade_page(&current));
    assert!(!rendered.contains("<form"));
}

#[test]
fn a_managed_upgrade_in_flight_polls_instead_of_offering_the_button_again() {
    let mut status = a_managed_status();
    status.state = "downloading".to_string();
    status.to_version = "0.2.0".to_string();
    let rendered = s(upgrade_page(&an_upgrade(UpgradeInstall::BinaryManaged {
        status,
    })));
    assert!(
        rendered.contains(r#"hx-get="/upgrade/status""#),
        "an in-flight upgrade is watched by polling"
    );
    assert!(
        !rendered.contains("<form"),
        "no second upgrade can be asked for while one runs"
    );
}

#[test]
fn the_managed_card_documents_the_rollback_and_the_snapshot() {
    let rendered = s(upgrade_page(&an_upgrade(UpgradeInstall::BinaryManaged {
        status: a_managed_status(),
    })));
    assert!(
        rendered.contains("boot guard"),
        "the crash-loop story is told"
    );
    assert!(
        rendered.contains("db-pre-upgrade.sqlite"),
        "the snapshot is named so someone can find it"
    );
    assert!(
        rendered.contains("deliberately manual"),
        "the page says the database restore is a human decision"
    );
}

#[test]
fn the_status_fragment_reports_each_phase_and_the_restart() {
    let mut status = a_managed_status();
    status.to_version = "0.2.0".to_string();

    // Every phase names all five steps and marks exactly one as current, so
    // progress reads as a position in a sequence rather than a spinner.
    for (state, expectation) in [
        ("downloading", "Downloading the release"),
        ("verifying", "Verifying the digest"),
        ("staging", "Staging the new version"),
        ("staged", "Snapshotting the database"),
        ("swapped", "Restarting into the new version"),
    ] {
        status.state = state.to_string();
        let rendered = s(self_upgrade_status_fragment(&status));
        assert!(rendered.contains(expectation), "{state}: {rendered}");
        assert_eq!(
            rendered.matches("upgrade-step current").count(),
            1,
            "{state}: exactly one step is the current one"
        );
    }

    // Steps before the current one are ticked off.
    status.state = "swapped".to_string();
    let rendered = s(self_upgrade_status_fragment(&status));
    assert_eq!(
        rendered.matches("upgrade-step done").count(),
        4,
        "the four earlier steps are done"
    );

    // The step being worked spins, and only that one: between two polls a
    // static numeral cannot distinguish "working" from "stuck".
    status.state = "staging".to_string();
    let rendered = s(self_upgrade_status_fragment(&status));
    assert_eq!(
        rendered.matches("step-mark spinner").count(),
        1,
        "exactly the current step spins"
    );
    assert!(
        !rendered.contains(r#"spinner">3<"#),
        "the spinning step shows the spinner rather than its number"
    );
    // The restart fragment is the same story from the other side.
    assert!(s(self_upgrade_restarting()).contains("step-mark spinner"));

    status.state = "confirmed".to_string();
    let rendered = s(self_upgrade_status_fragment(&status));
    assert!(rendered.contains("Now running 0.2.0"));

    // A rollback is reported as such, and says the old version is fine.
    status.state = "exec-failed".to_string();
    status.error = "exec of the new binary failed: Exec format error".to_string();
    let rendered = s(self_upgrade_status_fragment(&status));
    assert!(rendered.contains("rolled back"));
    assert!(rendered.contains("nothing was lost"));

    // The unreachable-control-plane render: calm, not an error.
    let rendered = s(self_upgrade_restarting());
    assert!(rendered.contains("Restarting"));
    assert!(!rendered.to_lowercase().contains("error"));
}

// -- the update dialog ------------------------------------------------

fn a_dialog(self_upgrade: Option<SelfUpgradeView>) -> UpdateDialog {
    UpdateDialog {
        current: "0.1.0".to_string(),
        latest: "0.2.0".to_string(),
        available: true,
        breaking: false,
        notes: "- Custom domains\n- Faster builds".to_string(),
        url: "https://github.com/Loa212/nudo/releases/tag/v0.2.0".to_string(),
        csrf: "tok".to_string(),
        self_upgrade,
    }
}

#[test]
fn a_current_instance_gets_no_dialog_at_all() {
    let mut view = a_dialog(None);
    view.available = false;
    assert_eq!(s(update_dialog(&view)), "");
}

#[test]
fn the_dialog_shows_the_changelog_and_three_ways_out() {
    let rendered = s(update_dialog(&a_dialog(Some(a_managed_status()))));

    // What changed, without a network call or leaving the page.
    assert!(rendered.contains("Custom domains"));
    assert!(rendered.contains("Faster builds"));
    assert!(rendered.contains("releases/tag/v0.2.0"));

    // The three decisions, each meaning something different.
    assert!(rendered.contains("Update now"));
    assert!(rendered.contains(r#"action="/upgrade/start""#));
    assert!(rendered.contains("Skip this version"));
    assert!(rendered.contains(r#"action="/upgrade/skip""#));
    assert!(rendered.contains("Close"));

    // Skipping carries the version, so it decides about this release only.
    assert!(rendered.contains(r#"name="version" value="0.2.0""#));
}

#[test]
fn the_dialog_offers_instructions_rather_than_a_button_when_it_cannot_act() {
    // A container, or an unreachable control plane: no self-upgrade status.
    let rendered = s(update_dialog(&a_dialog(None)));
    assert!(!rendered.contains(r#"action="/upgrade/start""#));
    assert!(rendered.contains("How to update"));
    // Skipping still works — it is a decision, not an action on the host.
    assert!(rendered.contains(r#"action="/upgrade/skip""#));

    // A managed install with a gate closed is the same story.
    let mut gated = a_managed_status();
    gated.enabled_in_settings = false;
    let rendered = s(update_dialog(&a_dialog(Some(gated))));
    assert!(!rendered.contains(r#"action="/upgrade/start""#));
    assert!(rendered.contains("How to update"));
}

#[test]
fn the_dialog_turns_into_progress_once_the_upgrade_starts() {
    let mut status = a_managed_status();
    status.state = "staging".to_string();
    status.to_version = "0.2.0".to_string();
    let rendered = s(update_dialog(&a_dialog(Some(status))));

    assert!(rendered.contains("Staging the new version"));
    assert!(
        rendered.contains(r#"hx-get="/upgrade/status""#),
        "progress is polled, so it rides through the restart"
    );
    // No second start, and no skipping something already underway.
    assert!(!rendered.contains(r#"action="/upgrade/start""#));
    assert!(!rendered.contains(r#"action="/upgrade/skip""#));
}

#[test]
fn a_breaking_release_is_called_out_in_the_dialog() {
    let mut view = a_dialog(Some(a_managed_status()));
    view.breaking = true;
    let rendered = s(update_dialog(&view));
    assert!(rendered.contains("needs manual steps"));
    // And the action is still offered: knowing is the point, not blocking.
    assert!(rendered.contains("Update now"));
}

#[test]
fn a_legacy_install_is_shown_the_path_to_the_managed_layout() {
    let rendered = s(upgrade_page(&an_upgrade(UpgradeInstall::BinaryLegacy)));
    assert!(
        rendered.contains("/var/lib/nudo/self"),
        "the migration target is named"
    );
    assert!(
        rendered.contains("NUDO_ALLOW_SELF_UPGRADE"),
        "the opt-in is explained alongside"
    );
    assert!(
        rendered.contains("overwrite its own binaries"),
        "the trade-off is stated, not buried"
    );
}

// -- the first-run checklist ------------------------------------------

fn a_finished_deployment() -> Deployment {
    Deployment {
        id: "dep_1".to_string(),
        service_id: "svc_1".to_string(),
        status: deployment::Status::Succeeded as i32,
        ..Default::default()
    }
}

#[test]
fn a_brand_new_instance_is_told_what_the_first_step_is() {
    // Four zeroes and an empty table say nothing about what to do. The
    // checklist names the sequence, so the order is visible rather than
    // inferred.
    let rendered = s(dashboard(&[], &[], &HashMap::new(), &[]));
    assert!(rendered.contains("Getting to a first deploy"));
    assert!(rendered.contains("0 of 3 done"));
    // Exactly one thing to click: the step being asked for.
    assert!(rendered.contains(r#"href="/targets/new""#));
}

#[test]
fn the_checklist_advances_as_each_step_is_finished() {
    let with_target = s(dashboard(&[a_target()], &[], &HashMap::new(), &[]));
    assert!(with_target.contains("1 of 3 done"));
    assert!(
        with_target.contains(r#"href="/services/new""#),
        "having a target, the next ask is a service"
    );

    let with_both = s(dashboard(
        &[a_target()],
        &[a_service()],
        &HashMap::new(),
        &[],
    ));
    assert!(with_both.contains("2 of 3 done"));
}

#[test]
fn the_checklist_stops_appearing_once_something_has_been_deployed() {
    // A checklist that outlives its usefulness becomes furniture.
    let rendered = s(dashboard(
        &[a_target()],
        &[a_service()],
        &HashMap::new(),
        &[a_finished_deployment()],
    ));
    assert!(!rendered.contains("Getting to a first deploy"));
}

#[test]
fn only_the_current_step_offers_a_button() {
    // Three buttons at once is three decisions; the point of the list is
    // that there is one thing to do next.
    let rendered = s(dashboard(&[], &[], &HashMap::new(), &[]));
    let checklist = rendered
        .split("Getting to a first deploy")
        .nth(1)
        .expect("the checklist is rendered")
        .split("</div></div>")
        .next()
        .unwrap_or_default();
    assert_eq!(
        checklist.matches("btn small primary").count(),
        1,
        "more than one step is asking to be clicked"
    );
}

// -- sources, audit, settings, auth -----------------------------------
