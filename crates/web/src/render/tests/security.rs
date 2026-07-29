use super::*;

// -- secrets: the property the whole module exists to preserve ----------

#[test]
fn a_secret_listing_shows_a_digest_and_never_the_value() {
    // There is no parameter that could carry a value, so the test asserts on
    // the whole page: nothing anywhere in it resembles a plaintext secret.
    let secret = Secret {
        id: "sec_1".to_string(),
        name: "EXCHANGE_API_KEY".to_string(),
        digest: "9f86d081884c7d659a2feaa0c55ad015".to_string(),
        updated_at: Some(nudo_proto::to_timestamp(chrono::Utc::now())),
        ..Default::default()
    };
    let rendered = s(secrets_list(
        &[secret],
        &[a_target()],
        &[a_service()],
        SecretNotice::None,
        "tok",
    ));

    assert!(rendered.contains("EXCHANGE_API_KEY"), "the name is shown");
    assert!(
        rendered.contains("9f86d081884c"),
        "a digest prefix is shown"
    );
    // Not the whole digest either — a prefix is all that drift detection needs.
    assert!(!rendered.contains("9f86d081884c7d659a2feaa0c55ad015"));
    assert!(rendered.contains("global"), "the scope is shown");
}

#[test]
fn the_secret_value_input_never_carries_a_value_attribute() {
    // The add form is the one place a value field exists. It must render
    // empty every time, including when redisplayed after a failed submit.
    let rendered = s(secrets_list(&[], &[], &[], SecretNotice::None, "tok"));
    let field = rendered
        .split("id=\"value\"")
        .nth(1)
        .expect("the value input")
        .split('>')
        .next()
        .expect("the end of the tag");
    assert!(
        !field.contains("value="),
        "the write-only field must have no value attribute: {field}"
    );
    assert!(
        field.contains("type=\"password\"") || rendered.contains("type=\"password\" id=\"value\"")
    );
}

#[test]
fn a_secret_row_has_no_element_that_could_reveal_a_value() {
    let secret = Secret {
        id: "sec_1".to_string(),
        name: "TOKEN".to_string(),
        digest: "deadbeefcafe0000".to_string(),
        ..Default::default()
    };
    let rendered = s(secrets_list(&[secret], &[], &[], SecretNotice::None, "tok"));
    // No "reveal"/"show" affordance to click, and no <code> holding a value.
    assert!(!rendered.to_lowercase().contains("reveal"));
    assert!(!rendered.contains("Show value"));
}

#[test]
fn a_services_secret_selection_is_by_id_and_shows_no_values() {
    let secret = Secret {
        id: "sec_1".to_string(),
        name: "API_KEY".to_string(),
        digest: "abc123abc123".to_string(),
        ..Default::default()
    };
    let rendered = s(service_form(
        None,
        &[a_target()],
        &[],
        &[secret],
        &[],
        "tok",
    ));
    assert!(rendered.contains("name=\"secret_ids\""), "selected by id");
    assert!(rendered.contains("value=\"sec_1\""));
    assert!(rendered.contains("API_KEY"));
    // No text input that could accept or display a value in this section.
    assert!(!rendered.contains("name=\"secret_value\""));
}

#[test]
fn a_service_detail_lists_secret_ids_but_not_values() {
    let mut service = a_service();
    service.secret_ids = vec!["sec_1".to_string(), "sec_2".to_string()];
    let rendered = s(service_detail(
        &service,
        &a_target(),
        &running(),
        &[],
        &[],
        "tok",
    ));
    assert!(rendered.contains("sec_1, sec_2"));
    assert!(rendered.contains("values are written on the target"));
}

#[test]
fn a_targets_ssh_key_is_chosen_from_the_store_not_typed() {
    // A key pasted into a form is logged by everything on the way in, so the
    // form must only ever offer a reference.
    let secret = Secret {
        id: "sec_key".to_string(),
        name: "deploy-key".to_string(),
        ..Default::default()
    };
    let rendered = s(target_form(None, &[secret], "tok"));
    assert!(rendered.contains("<select id=\"ssh_key_id\" name=\"ssh_key_id\""));
    assert!(rendered.contains("value=\"sec_key\""));
    // Not a textarea or text input that could hold key material.
    assert!(!rendered.contains("name=\"ssh_private_key\""));
    assert!(!rendered.contains("<textarea id=\"ssh_key_id\""));
}

// -- CSRF --------------------------------------------------------------

/// Every rendered POST form has a hidden csrf input.
fn assert_every_post_form_has_csrf(rendered: &str, token: &str, what: &str) {
    let forms: Vec<&str> = rendered.split("<form").skip(1).collect();
    let posts: Vec<&&str> = forms
        .iter()
        .filter(|f| {
            f.split("</form>")
                .next()
                .unwrap_or(f)
                .contains("method=\"post\"")
        })
        .collect();
    assert!(!posts.is_empty(), "{what} renders no POST form to check");
    for form in posts {
        let body = form.split("</form>").next().unwrap_or(form);
        assert!(
            body.contains(&format!("name=\"csrf\" value=\"{token}\"")),
            "{what} has a POST form without a csrf input: {body}"
        );
    }
}

#[test]
fn every_post_form_on_every_screen_carries_a_csrf_token() {
    let token = "csrf-token-abc";
    let target = a_target();
    let service = a_service();
    let secret = Secret {
        id: "sec_1".to_string(),
        name: "API_KEY".to_string(),
        digest: "abc123abc123".to_string(),
        ..Default::default()
    };
    let source = Source {
        id: "src_1".to_string(),
        name: "nudo-deploy".to_string(),
        kind: source::Kind::GithubApp as i32,
        installed: true,
        ..Default::default()
    };
    let release = Release {
        id: "rel_1".to_string(),
        service_id: "svc_1".to_string(),
        ..Default::default()
    };
    let deployment = Deployment {
        id: "dep_1".to_string(),
        service_id: "svc_1".to_string(),
        status: deployment::Status::Building as i32,
        ..Default::default()
    };
    let token_view = TokenView {
        id: "tok_1".to_string(),
        name: "laptop".to_string(),
        scopes: "deploy".to_string(),
        last_used: None,
        revoked: false,
        created: chrono::Utc::now(),
    };

    let screens: Vec<(&str, String)> = vec![
        (
            "target_form(new)",
            s(target_form(None, std::slice::from_ref(&secret), token)),
        ),
        (
            "target_form(edit)",
            s(target_form(
                Some(&target),
                std::slice::from_ref(&secret),
                token,
            )),
        ),
        (
            "service_form(new)",
            s(service_form(
                None,
                std::slice::from_ref(&target),
                std::slice::from_ref(&source),
                std::slice::from_ref(&secret),
                &[],
                token,
            )),
        ),
        (
            "service_form(edit)",
            s(service_form(
                Some(&service),
                std::slice::from_ref(&target),
                std::slice::from_ref(&source),
                std::slice::from_ref(&secret),
                &[],
                token,
            )),
        ),
        (
            "service_detail",
            s(service_detail(
                &service,
                &target,
                &running(),
                &[release],
                std::slice::from_ref(&deployment),
                token,
            )),
        ),
        (
            "deployment_detail",
            s(deployment_detail(&deployment, &service, &[], true, token)),
        ),
        (
            "secrets_list",
            s(secrets_list(
                &[secret],
                std::slice::from_ref(&target),
                std::slice::from_ref(&service),
                SecretNotice::None,
                token,
            )),
        ),
        ("sources_list", s(sources_list(&[source], token))),
        (
            "settings_page",
            s(settings_page(
                &[token_view],
                "a@example.com",
                &SettingsPrefs::default(),
                token,
            )),
        ),
        ("login_page", s(login_page(None, token))),
        ("setup_page", s(setup_page(None, token))),
        // The ingress card posts to enable, reload and disable.
        (
            "target_detail",
            s(target_detail(&target, &[], &HashMap::new(), None, token)),
        ),
        // And once ingress is on, the reload and disable forms replace the
        // enable one — a different set of forms, all of which need a token.
        (
            "target_detail_with_ingress",
            s(target_detail(
                &a_target_with_ingress(),
                &[],
                &HashMap::new(),
                None,
                token,
            )),
        ),
    ];

    for (what, rendered) in screens {
        assert_every_post_form_has_csrf(&rendered, token, what);
    }
}

#[test]
fn a_read_only_screen_posts_nothing_and_so_needs_no_token() {
    // A screen with no POST form has nothing to forge.
    //
    // `target_detail` used to be in this list. It is not any more: the ingress
    // card posts to enable, reload and disable, so it is covered by
    // `every_post_form_on_every_screen_carries_a_csrf_token` instead. Running
    // the preflight checks is still a link, because it probes the target and
    // changes nothing.
    let rendered = s(target_detail(
        &a_target(),
        &[],
        &HashMap::new(),
        None,
        "csrf",
    ));
    assert!(rendered.contains("Run checks"));
    assert!(rendered.contains("href=\"/targets/tgt_1?check=1\""));

    for rendered in [
        s(deployments_list(&[], &[])),
        s(audit_list(&[])),
        s(targets_list(&[])),
        s(services_list(&[], &[], &HashMap::new())),
        s(service_unit(&a_service(), "[Unit]")),
    ] {
        assert!(!rendered.contains("method=\"post\""));
    }
}

#[test]
fn the_log_filter_form_is_a_get_and_so_needs_no_token() {
    // A read-only form with a token would suggest the token is decorative.
    let rendered = s(logs_view(&a_service(), &[], "", false));
    assert!(rendered.contains("method=\"get\""));
    assert!(!rendered.contains("name=\"csrf\""));
}

// -- latency-critical --------------------------------------------------

#[test]
fn the_latency_critical_badge_is_loud_and_says_what_it_is() {
    let rendered = s(latency_critical_badge());
    assert!(rendered.contains("class=\"badge hot\""));
    assert!(rendered.contains("latency-critical"));
}

#[test]
fn a_latency_critical_target_is_marked_everywhere_it_appears() {
    let mut hot = a_target();
    hot.latency_critical = true;
    let cold = Target {
        id: "tgt_2".to_string(),
        name: "spare".to_string(),
        ..a_target()
    };

    let listing = s(targets_list(&[hot.clone()]));
    assert!(listing.contains("latency-critical"));
    assert!(listing.contains("badge hot"));

    let detail = s(target_detail(&hot, &[], &HashMap::new(), None, "csrf"));
    assert!(detail.contains("badge hot"));
    assert!(
        detail.contains("Latency-critical host"),
        "and an explanation"
    );

    let overview = s(dashboard(&[hot.clone()], &[], &HashMap::new(), &[]));
    assert!(overview.contains("badge hot"));

    // And absent for an ordinary host, on every one of those screens.
    assert!(!s(targets_list(std::slice::from_ref(&cold))).contains("badge hot"));
    assert!(!s(target_detail(&cold, &[], &HashMap::new(), None, "csrf")).contains("badge hot"));
    assert!(!s(dashboard(&[cold], &[], &HashMap::new(), &[])).contains("badge hot"));
}

#[test]
fn deploying_to_a_latency_critical_host_confirms_with_that_named() {
    let mut hot = a_target();
    hot.latency_critical = true;
    let rendered = s(service_detail(
        &a_service(),
        &hot,
        &running(),
        &[],
        &[],
        "tok",
    ));
    assert!(
        rendered.contains("LATENCY-CRITICAL"),
        "the confirm names it"
    );

    let ordinary = s(service_detail(
        &a_service(),
        &a_target(),
        &running(),
        &[],
        &[],
        "tok",
    ));
    assert!(!ordinary.contains("LATENCY-CRITICAL"));
}

#[test]
fn latency_knobs_are_shown_prominently_only_when_set() {
    let mut pinned = a_service();
    pinned.unit = Some(SystemdUnit {
        unit_name: "bot.service".to_string(),
        cpu_affinity: "2-5".to_string(),
        nice: "-10".to_string(),
        io_scheduling_class: "realtime".to_string(),
        ..Default::default()
    });

    let rendered = s(service_detail(
        &pinned,
        &a_target(),
        &running(),
        &[],
        &[],
        "t",
    ));
    assert!(rendered.contains("Latency configuration"));
    assert!(rendered.contains("CPUAffinity"));
    assert!(rendered.contains("2-5"));
    assert!(rendered.contains("IOSchedulingClass"));

    let plain = s(service_detail(
        &a_service(),
        &a_target(),
        &running(),
        &[],
        &[],
        "t",
    ));
    assert!(!plain.contains("Latency configuration"));
}

// -- badges ------------------------------------------------------------

#[test]
fn the_ssh_key_textarea_renders_empty_and_is_never_prefilled() {
    // The same write-only property as the value input, in the element a
    // multi-line key needs. A textarea holds its content between its tags
    // rather than in an attribute, so this is a different way to get it wrong.
    let rendered = s(secrets_list(&[], &[], &[], SecretNotice::None, "tok"));

    let textarea = rendered
        .split("id=\"key_value\"")
        .nth(1)
        .expect("the key textarea")
        .split("</textarea>")
        .next()
        .expect("the closing tag");

    let content = textarea.split('>').nth(1).unwrap_or("");
    assert!(
        content.trim().is_empty(),
        "the key textarea must render empty: {content:?}"
    );
}

#[test]
fn the_ssh_key_form_does_not_offer_a_scope() {
    // A key opens the connection, so scoping it to a target it is needed to
    // reach is circular. Offering the field would invite that mistake.
    let rendered = s(secrets_list(&[], &[], &[], SecretNotice::None, "tok"));
    let form = rendered
        .split("id=\"ssh-key\"")
        .nth(1)
        .expect("the ssh key form")
        .split("</form>")
        .next()
        .expect("the closing tag");

    assert!(!form.contains("scope_target_id"), "got: {form}");
    assert!(!form.contains("scope_service_id"), "got: {form}");
    // And it posts somewhere that will not silently apply one.
    assert!(rendered.contains("action=\"/secrets/ssh-key\""));
}

#[test]
fn both_secret_forms_carry_a_csrf_token() {
    // Two forms now write to the store, and a new one must not be able to
    // arrive without one.
    let rendered = s(secrets_list(&[], &[], &[], SecretNotice::None, "tok-abc"));
    assert_eq!(
        rendered.matches("name=\"csrf\" value=\"tok-abc\"").count(),
        2,
        "the env-var form and the ssh key form each need a token"
    );
}

#[test]
fn a_taken_name_is_reported_on_the_page_and_points_at_rotate() {
    // The refusal has to say what to do instead, or it is just a wall.
    let rendered = s(secrets_list(
        &[],
        &[],
        &[],
        SecretNotice::from_key("taken", "DEPLOY_KEY"),
        "tok",
    ));
    assert!(rendered.contains("already taken"));
    assert!(rendered.contains("DEPLOY_KEY"));
    assert!(rendered.contains("Rotate"), "it must name the way forward");
}

#[test]
fn an_unrecognised_notice_key_renders_nothing() {
    // The key comes from a query string, so a crafted link must not be able to
    // put words in a banner on somebody's own dashboard.
    let rendered = s(secrets_list(
        &[],
        &[],
        &[],
        SecretNotice::from_key("<script>alert(1)</script>", "x"),
        "tok",
    ));
    assert!(!rendered.contains("alert(1)"));
    assert!(!rendered.contains("callout bad"), "no banner at all");
}

#[test]
fn the_page_no_longer_promises_to_replace_an_existing_value() {
    // The old copy said writing an existing name replaced its value, which was
    // both true and alarming. Neither should be the case now.
    let rendered = s(secrets_list(&[], &[], &[], SecretNotice::None, "tok"));
    assert!(
        !rendered.contains("replaces its value"),
        "the overwrite promise must be gone"
    );
    assert!(rendered.contains("refused rather than"));
}

#[test]
fn every_stored_secret_offers_a_rotate_form_carrying_its_scope() {
    // The scope identifies which secret a write lands on: two can share a name
    // under different scopes, and rotating the wrong one is the exact accident
    // this whole change exists to prevent.
    let secret = Secret {
        id: "sec_1".to_string(),
        name: "DATABASE_URL".to_string(),
        scope_target_id: "tgt_9".to_string(),
        digest: "abc123abc123".to_string(),
        ..Default::default()
    };
    let rendered = s(secrets_list(
        std::slice::from_ref(&secret),
        &[],
        &[],
        SecretNotice::None,
        "tok",
    ));

    assert!(rendered.contains("action=\"/secrets/rotate\""));
    assert!(rendered.contains("name=\"scope_target_id\" value=\"tgt_9\""));
    // And it still carries no value to prefill, like every other field here.
    let panel = rendered
        .split("action=\"/secrets/rotate\"")
        .nth(1)
        .expect("the rotate form");
    let textarea = panel
        .split("</textarea>")
        .next()
        .expect("a textarea")
        .split('>')
        .next_back()
        .unwrap_or("");
    assert!(textarea.trim().is_empty(), "got: {textarea:?}");
}
