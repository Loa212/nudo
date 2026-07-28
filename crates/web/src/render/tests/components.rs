use super::*;

#[test]
fn every_badge_kind_maps_to_its_class() {
    assert!(s(badge("x", BadgeKind::Neutral)).contains("class=\"badge\""));
    assert!(s(badge("x", BadgeKind::Ok)).contains("class=\"badge ok\""));
    assert!(s(badge("x", BadgeKind::Warn)).contains("class=\"badge warn\""));
    assert!(s(badge("x", BadgeKind::Bad)).contains("class=\"badge bad\""));
    assert!(s(badge("x", BadgeKind::Info)).contains("class=\"badge info\""));
    assert!(s(badge("x", BadgeKind::Hot)).contains("class=\"badge hot\""));
}

#[test]
fn a_badge_label_is_escaped() {
    // Status text can come from systemd, which is not our code.
    let rendered = s(badge("<b>x</b>", BadgeKind::Ok));
    assert!(rendered.contains("&lt;b&gt;"));
    assert!(!rendered.contains("<b>x</b>"));
}

#[test]
fn unit_states_collapse_to_the_right_word_and_colour() {
    let with = |active: &str, sub: &str| {
        s(unit_badge(&UnitStatus {
            active_state: active.to_string(),
            sub_state: sub.to_string(),
            ..Default::default()
        }))
    };

    let running = with("active", "running");
    assert!(running.contains("badge ok") && running.contains("running"));

    let starting = with("activating", "start-pre");
    assert!(starting.contains("badge warn") && starting.contains("starting"));

    let stopping = with("deactivating", "stop");
    assert!(stopping.contains("badge warn") && stopping.contains("stopping"));

    let failed = with("failed", "failed");
    assert!(failed.contains("badge bad") && failed.contains("failed"));

    let stopped = with("inactive", "dead");
    assert!(stopped.contains("class=\"badge\"") && stopped.contains("stopped"));

    // Our own blindness, not a claim about the unit.
    let unreachable = with("unknown", "");
    assert!(unreachable.contains("badge warn") && unreachable.contains("unreachable"));

    let nonsense = with("reloading-sideways", "");
    assert!(nonsense.contains("class=\"badge\"") && nonsense.contains("unknown"));
}

#[test]
fn an_active_but_not_running_unit_shows_its_sub_state() {
    // A oneshot that completed is `active/exited`, which is fine, but
    // calling it "running" would be a lie.
    let rendered = s(unit_badge(&UnitStatus {
        active_state: "active".to_string(),
        sub_state: "exited".to_string(),
        ..Default::default()
    }));
    assert!(rendered.contains("badge ok"));
    assert!(rendered.contains("exited"));
}

#[test]
fn every_target_status_variant_maps_to_a_badge() {
    let reachable = s(target_badge(target::Status::Reachable as i32));
    assert!(reachable.contains("badge ok") && reachable.contains("reachable"));

    let unreachable = s(target_badge(target::Status::Unreachable as i32));
    assert!(unreachable.contains("badge bad") && unreachable.contains("unreachable"));

    for status in [
        target::Status::Unknown as i32,
        target::Status::Unspecified as i32,
        // An enum value from a newer server than this build.
        9_999,
    ] {
        let rendered = s(target_badge(status));
        assert!(
            rendered.contains("class=\"badge\""),
            "{status} should be neutral"
        );
        assert!(rendered.contains("unknown"));
    }
}

#[test]
fn every_deployment_status_variant_maps_to_a_badge() {
    use deployment::Status as S;

    let cases: [(S, &str, &str); 9] = [
        (S::Queued, "badge info", "queued"),
        (S::Building, "badge info", "building"),
        (S::Uploading, "badge info", "uploading"),
        (S::Activating, "badge info", "activating"),
        (S::HealthChecking, "badge info", "health_checking"),
        (S::Succeeded, "badge ok", "succeeded"),
        (S::Failed, "badge bad", "failed"),
        (S::RolledBack, "badge warn", "rolled back"),
        (S::Cancelled, "class=\"badge\"", "cancelled"),
    ];

    for (status, class, label) in cases {
        let rendered = s(deployment_badge(status as i32));
        assert!(
            rendered.contains(class),
            "{label} wanted {class}: {rendered}"
        );
        assert!(rendered.contains(label), "{label} missing: {rendered}");
    }

    // Unspecified and unknown wire values fall back rather than panicking.
    assert!(s(deployment_badge(0)).contains("unspecified"));
    assert!(s(deployment_badge(9_999)).contains("unspecified"));
}

// -- empty states ------------------------------------------------------

#[test]
fn an_empty_state_carries_its_next_action() {
    let rendered = s(empty_state(
        "Nothing",
        "Add one.",
        Some(("Add target", "/targets/new")),
    ));
    assert!(rendered.contains("class=\"empty\""));
    assert!(rendered.contains("href=\"/targets/new\""));
    assert!(rendered.contains("Add target"));
}

#[test]
fn an_empty_state_without_an_action_renders_no_link() {
    let rendered = s(empty_state("Nothing", "Nothing to do.", None));
    assert!(!rendered.contains("<a "));
}

#[test]
fn every_empty_listing_offers_the_action_that_fills_it() {
    let cases = [
        ("targets", s(targets_list(&[])), "/targets/new"),
        (
            "services",
            s(services_list(&[], &[], &HashMap::new())),
            "/services/new",
        ),
        ("deployments", s(deployments_list(&[], &[])), "/services"),
        (
            "dashboard",
            s(dashboard(&[], &[], &HashMap::new(), &[])),
            "/targets/new",
        ),
    ];
    for (what, rendered, href) in cases {
        // What matters is that an empty page offers the next action, not
        // which component carries it — the dashboard's comes from the
        // first-run checklist rather than an `.empty` card.
        assert!(
            rendered.contains("class=\"empty\"") || rendered.contains("class=\"steps\""),
            "{what} says it is empty without offering a way to fill it"
        );
        assert!(
            rendered.contains(&format!("href=\"{href}\"")),
            "{what} does not link to {href}"
        );
    }
}

#[test]
fn a_target_with_no_services_is_offered_one_scoped_to_it() {
    let rendered = s(target_detail(&a_target(), &[], &HashMap::new(), None));
    assert!(rendered.contains("/services/new?target=tgt_1"));
}

// -- SSE fragments -----------------------------------------------------
