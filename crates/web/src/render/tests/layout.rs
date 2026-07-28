use super::*;

#[test]
fn the_page_shell_has_a_head_and_the_three_assets() {
    let rendered = s(page("Overview", Nav::Dashboard, html! { div { "body" } }));
    assert!(rendered.starts_with("<!DOCTYPE html>"));
    assert!(rendered.contains("<meta charset=\"utf-8\">"));
    assert!(rendered.contains("name=\"viewport\""));
    assert!(rendered.contains("<title>Overview · nudo</title>"));
    // Fingerprinted, so the URL carries a `?v=` the page shell does not
    // spell out. What matters is that each asset is referenced and that the
    // reference can be invalidated by a rebuild.
    for asset in ["app.css", "htmx.min.js", "sse.js"] {
        assert!(
            rendered.contains(&format!("/assets/{asset}?v=")),
            "{asset} is missing or unfingerprinted"
        );
    }
    assert!(rendered.contains("class=\"shell\""));
    assert!(rendered.contains("class=\"rail\""));
    assert!(rendered.contains("class=\"main\""));
    assert!(rendered.contains("<div>body</div>"));
}

#[test]
fn a_page_title_is_escaped() {
    let rendered = s(page("</title><script>x</script>", Nav::Dashboard, html! {}));
    assert!(!rendered.contains("<script>x</script>"));
    assert!(rendered.contains("&lt;/title&gt;"));
}

#[test]
fn exactly_one_rail_item_is_active_and_it_is_the_requested_one() {
    for (nav, _, href, _) in Nav::items() {
        let rendered = s(page("t", nav, html! {}));
        assert_eq!(
            rendered.matches("class=\"nav active\"").count(),
            1,
            "{nav:?} should mark exactly one item"
        );
        let active = rendered
            .split("class=\"nav active\"")
            .nth(1)
            .expect("the active item");
        assert!(
            active.starts_with(&format!(" href=\"{href}\"")),
            "{nav:?} marked the wrong item: {}",
            &active[..active.len().min(60)]
        );
    }
}

#[test]
fn tabs_and_submenus_mark_the_active_item() {
    let rendered = s(tabs(&[("One", "/one", false), ("Two", "/two", true)]));
    assert!(
        rendered.contains("class=\"\" href=\"/one\">One</a>"),
        "{rendered}"
    );
    assert!(rendered.contains("class=\"active\" href=\"/two\""));

    let rendered = s(submenu(&[("A", "/a", true), ("B", "/b", false)]));
    assert!(rendered.contains("class=\"submenu\""));
    assert!(rendered.contains("class=\"active\" href=\"/a\""));
}

#[test]
fn a_topbar_omits_the_subtitle_when_there_is_none() {
    let with = s(topbar("T", Some("sub"), html! {}));
    assert!(with.contains("class=\"subtitle\">sub<"));

    let without = s(topbar("T", None, html! {}));
    assert!(!without.contains("subtitle"));
    assert!(without.contains("<h1>T</h1>"));
}

// -- destructive actions -----------------------------------------------

#[test]
fn every_destructive_button_confirms_and_names_the_consequence() {
    let target = a_target();
    let service = a_service();
    let secret = Secret {
        id: "sec_1".to_string(),
        name: "API_KEY".to_string(),
        ..Default::default()
    };
    let source = Source {
        id: "src_1".to_string(),
        name: "app".to_string(),
        ..Default::default()
    };
    let release = Release {
        id: "rel_1".to_string(),
        service_id: "svc_1".to_string(),
        ..Default::default()
    };
    let token_view = TokenView {
        id: "tok_1".to_string(),
        name: "laptop".to_string(),
        scopes: "admin".to_string(),
        last_used: None,
        revoked: false,
        created: chrono::Utc::now(),
    };

    let screens = [
        (
            "target_form(edit)",
            s(target_form(
                Some(&target),
                std::slice::from_ref(&secret),
                "t",
            )),
        ),
        (
            "service_form(edit)",
            s(service_form(
                Some(&service),
                std::slice::from_ref(&target),
                &[],
                &[],
                "t",
            )),
        ),
        (
            "service_detail",
            s(service_detail(
                &service,
                &target,
                &running(),
                &[release],
                &[],
                "t",
            )),
        ),
        ("secrets_list", s(secrets_list(&[secret], &[], &[], "t"))),
        ("sources_list", s(sources_list(&[source], "t"))),
        (
            "settings_page",
            s(settings_page(
                &[token_view],
                "a@b.c",
                &SettingsPrefs::default(),
                "t",
            )),
        ),
    ];

    for (what, rendered) in screens {
        for chunk in rendered.split("btn small danger").skip(1) {
            let tag = chunk.split('>').next().unwrap_or(chunk);
            assert!(
                tag.contains("onclick"),
                "{what}: danger button without confirm"
            );
        }
        for chunk in rendered.split("class=\"btn danger\"").skip(1) {
            let tag = chunk.split('>').next().unwrap_or(chunk);
            assert!(
                tag.contains("onclick"),
                "{what}: danger button without confirm"
            );
        }
        assert!(
            rendered.contains("return confirm("),
            "{what} has a destructive form and no confirm at all"
        );
    }
}

#[test]
fn a_confirm_message_with_a_quote_in_the_name_stays_one_javascript_string() {
    // A service named `bo't` would otherwise close the JS literal.
    let mut service = a_service();
    service.name = "bo't".to_string();
    let release = Release {
        id: "rel_1".to_string(),
        service_id: "svc_1".to_string(),
        ..Default::default()
    };
    let rendered = s(service_detail(
        &service,
        &a_target(),
        &running(),
        &[release],
        &[],
        "t",
    ));
    // maud escapes the attribute for HTML but leaves `'` alone, since the
    // attribute is double-quoted. `js_text` is what keeps the apostrophe
    // from closing the JavaScript literal inside it.
    assert!(
        rendered.contains("confirm('Roll bo\\'t back to release rel_1?"),
        "the apostrophe must be backslash-escaped: {rendered}"
    );
}

#[test]
fn a_running_service_offers_stop_and_a_stopped_one_offers_start() {
    let running_page = s(service_detail(
        &a_service(),
        &a_target(),
        &running(),
        &[],
        &[],
        "t",
    ));
    assert!(running_page.contains(">Stop<"));
    assert!(!running_page.contains(">Start<"));

    let stopped = UnitStatus {
        active_state: "inactive".to_string(),
        sub_state: "dead".to_string(),
        ..Default::default()
    };
    let stopped_page = s(service_detail(
        &a_service(),
        &a_target(),
        &stopped,
        &[],
        &[],
        "t",
    ));
    assert!(stopped_page.contains(">Start<"));
    // Starting a stopped unit is what the operator came for; no confirm.
    assert!(!stopped_page.contains(">Stop<"));
}

#[test]
fn the_current_release_has_no_rollback_button() {
    let releases = [
        Release {
            id: "rel_2".to_string(),
            service_id: "svc_1".to_string(),
            ..Default::default()
        },
        Release {
            id: "rel_1".to_string(),
            service_id: "svc_1".to_string(),
            ..Default::default()
        },
    ];
    // a_service()'s current release is rel_2.
    let rendered = s(service_detail(
        &a_service(),
        &a_target(),
        &running(),
        &releases,
        &[],
        "t",
    ));
    assert_eq!(rendered.matches(">Rollback<").count(), 1);
    assert!(
        rendered.contains("release rel_1"),
        "the confirm names the target release"
    );
    assert!(rendered.contains(">current<"));
}

// -- listings and detail -----------------------------------------------
