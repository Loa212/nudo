use super::*;

fn an_upgrade(install: UpgradeInstall) -> UpgradeView {
    UpgradeView {
        current: "0.1.0".to_string(),
        latest: "0.2.0".to_string(),
        available: true,
        breaking: false,
        install,
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
    let rendered = s(upgrade_page(&an_upgrade(UpgradeInstall::Binary)));
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
        UpgradeInstall::Binary,
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
    let mut view = an_upgrade(UpgradeInstall::Binary);
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
    let mut binary = an_upgrade(UpgradeInstall::Binary);
    binary.available = false;
    let rendered = s(upgrade_page(&binary));
    assert!(rendered.contains("version=X.Y.Z"));
    assert!(!rendered.contains("version=latest"));
}

#[test]
fn an_up_to_date_instance_still_gets_the_instructions() {
    // Reached from a bookmark or the nav rather than the banner. Saying
    // "nothing to do" and hiding the steps would be a dead end.
    let mut view = an_upgrade(UpgradeInstall::Binary);
    view.available = false;
    let rendered = s(upgrade_page(&view));
    assert!(rendered.contains("You are up to date"));
    assert!(rendered.contains("sha256sum -c"), "the steps are hidden");
}

#[test]
fn the_upgrade_page_never_offers_to_do_it_for_you() {
    // nudo holds the SSH keys for every machine it manages. A button here
    // that ran an upgrade would be a much larger thing to trust than a page
    // that tells you what to type, and this test is what keeps it a page.
    for install in [
        UpgradeInstall::Container {
            image: "ghcr.io/loa212/nudo",
        },
        UpgradeInstall::Binary,
    ] {
        let rendered = s(upgrade_page(&an_upgrade(install)));
        assert!(
            !rendered.contains("<form"),
            "the upgrade page has a form, so something is being submitted"
        );
        assert!(
            !rendered.contains("curl -fsSL") && !rendered.contains("| sh"),
            "the page pipes a downloaded script into a shell"
        );
    }
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
