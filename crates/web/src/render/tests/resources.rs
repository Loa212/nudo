use super::*;

#[test]
fn the_dashboard_counts_running_and_failed_units() {
    let services = [
        a_service(),
        Service {
            id: "svc_2".to_string(),
            target_id: "tgt_1".to_string(),
            name: "sidecar".to_string(),
            ..Default::default()
        },
        Service {
            id: "svc_3".to_string(),
            target_id: "tgt_1".to_string(),
            name: "idle".to_string(),
            ..Default::default()
        },
    ];
    let mut statuses = HashMap::new();
    statuses.insert("svc_1".to_string(), running());
    statuses.insert(
        "svc_2".to_string(),
        UnitStatus {
            active_state: "failed".to_string(),
            sub_state: "failed".to_string(),
            ..Default::default()
        },
    );
    // svc_3 has no status: not counted either way.

    let rendered = s(dashboard(&[a_target()], &services, &statuses, &[]));
    assert!(rendered.contains("class=\"stats\""));
    // The failed count is the only one coloured, because it is the only one
    // that means someone has to do something.
    assert!(rendered.contains("class=\"stat is-bad\""));
    assert!(rendered.contains("<div class=\"stat-value\">1</div>"));
    assert!(
        rendered.contains("<div class=\"stat-value\">3</div>"),
        "3 services"
    );
    // A target with a failed unit is flagged on its tile.
    assert!(rendered.contains("class=\"tile alert\""));
    assert!(rendered.contains("1 failed"));
}

#[test]
fn an_unreachable_target_tile_is_flagged() {
    let mut target = a_target();
    target.status = target::Status::Unreachable as i32;
    let rendered = s(dashboard(&[target], &[], &HashMap::new(), &[]));
    assert!(rendered.contains("class=\"tile alert\""));
    assert!(rendered.contains("unreachable"));
}

#[test]
fn a_reachable_target_tile_is_not_flagged() {
    let rendered = s(dashboard(&[a_target()], &[], &HashMap::new(), &[]));
    assert!(rendered.contains("class=\"tile\""));
    assert!(!rendered.contains("tile alert"));
}

#[test]
fn the_targets_listing_shows_the_address_and_calls_agentless_out() {
    let mut with_agent = a_target();
    with_agent.agent_version = "0.3.1".to_string();
    with_agent
        .labels
        .insert("env".to_string(), "prod".to_string());
    with_agent
        .labels
        .insert("role".to_string(), "bot".to_string());

    let rendered = s(targets_list(&[a_target(), with_agent]));
    assert!(rendered.contains("deploy@10.0.0.4:22"));
    // Agentless is a supported mode, not a missing field.
    assert!(rendered.contains("agentless"));
    assert!(rendered.contains("0.3.1"));
    // Labels render in a stable order regardless of map iteration.
    assert!(rendered.contains("env=prod, role=bot"));
}

#[test]
fn target_detail_shows_the_key_reference_and_the_check_results() {
    let checks = CheckTargetResponse {
        ok: false,
        checks: vec![
            check_target_response::Check {
                name: "ssh".to_string(),
                ok: true,
                detail: "connected in 42ms".to_string(),
            },
            check_target_response::Check {
                name: "sudo".to_string(),
                ok: false,
                detail: "deploy is not in sudoers".to_string(),
            },
        ],
    };
    let rendered = s(target_detail(
        &a_target(),
        &[],
        &HashMap::new(),
        Some(&checks),
    ));

    assert!(rendered.contains("Preflight checks"));
    assert!(rendered.contains("problems found"));
    assert!(rendered.contains("deploy is not in sudoers"));
    // The key is a reference into the store.
    assert!(rendered.contains("sec_key"));
    assert!(rendered.contains("SSH key"));
}

#[test]
fn target_detail_omits_the_check_card_when_no_check_has_run() {
    let rendered = s(target_detail(&a_target(), &[], &HashMap::new(), None));
    assert!(!rendered.contains("Preflight checks"));
}

#[test]
fn a_passing_check_set_reads_as_passing() {
    let checks = CheckTargetResponse {
        ok: true,
        checks: vec![check_target_response::Check {
            name: "systemd".to_string(),
            ok: true,
            detail: String::new(),
        }],
    };
    let rendered = s(target_detail(
        &a_target(),
        &[],
        &HashMap::new(),
        Some(&checks),
    ));
    assert!(rendered.contains("all passed"));
}

#[test]
fn the_services_listing_names_the_target_and_the_source() {
    let mut statuses = HashMap::new();
    statuses.insert("svc_1".to_string(), running());
    let rendered = s(services_list(&[a_service()], &[a_target()], &statuses));

    assert!(
        rendered.contains("hft-box"),
        "the target's name, not its id"
    );
    assert!(rendered.contains("git:owner/bot@main"));
    assert!(rendered.contains("bot.service"));
    assert!(rendered.contains("64.0 MiB"));
    assert!(rendered.contains("badge ok"));
}

#[test]
fn a_service_with_no_reported_status_says_so_rather_than_guessing() {
    let rendered = s(services_list(
        &[a_service()],
        &[a_target()],
        &HashMap::new(),
    ));
    assert!(rendered.contains("no data"));
}

#[test]
fn a_never_deployed_service_says_so_rather_than_showing_an_empty_cell() {
    let mut service = a_service();
    service.current_release_id = String::new();
    let rendered = s(services_list(&[service], &[a_target()], &HashMap::new()));
    assert!(rendered.contains("never deployed"));
}

#[test]
fn a_service_with_no_health_check_says_it_will_not_roll_back() {
    let mut service = a_service();
    service.health_check = None;
    let rendered = s(service_detail(
        &service,
        &a_target(),
        &running(),
        &[],
        &[],
        "t",
    ));
    assert!(rendered.contains("never rolled back automatically"));
}

#[test]
fn each_health_check_kind_is_described() {
    let with = |kind: Option<health_check::Kind>| {
        let mut service = a_service();
        service.health_check = Some(HealthCheck {
            kind,
            timeout_seconds: 5,
            retries: 2,
            initial_delay_seconds: 1,
        });
        s(service_detail(
            &service,
            &a_target(),
            &running(),
            &[],
            &[],
            "t",
        ))
    };

    assert!(with(Some(health_check::Kind::HttpUrl("http://x/z".to_string()))).contains("GET "));
    assert!(with(Some(health_check::Kind::Command("/bin/check".to_string()))).contains("command "));
    assert!(with(Some(health_check::Kind::SystemdActive(true))).contains("is-active only"));
}

#[test]
fn each_artifact_kind_is_described_on_the_detail_page() {
    let with = |kind: artifact_source::Kind| {
        let mut service = a_service();
        service.artifact = Some(ArtifactSource { kind: Some(kind) });
        s(service_detail(
            &service,
            &a_target(),
            &running(),
            &[],
            &[],
            "t",
        ))
    };

    assert!(
        with(artifact_source::Kind::Url("https://x/bot".to_string())).contains("https://x/bot")
    );
    assert!(with(artifact_source::Kind::DirectUpload(true)).contains("pushed by the CLI"));

    let git = with(artifact_source::Kind::Git(GitSource {
        repo: "owner/bot".to_string(),
        branch: "main".to_string(),
        build_command: "cargo build".to_string(),
        auto_deploy_on_push: true,
        ..Default::default()
    }));
    assert!(git.contains("owner/bot@main"));
    assert!(git.contains("cargo build"));
    assert!(git.contains("auto-deploy on push"));
}

#[test]
fn a_failed_unit_gets_a_callout_not_just_a_badge() {
    let failed = UnitStatus {
        active_state: "failed".to_string(),
        sub_state: "failed".to_string(),
        restart_count: 7,
        ..Default::default()
    };
    let rendered = s(service_detail(
        &a_service(),
        &a_target(),
        &failed,
        &[],
        &[],
        "t",
    ));
    assert!(rendered.contains("callout bad"));
    assert!(rendered.contains("Unit is failed"));
}

#[test]
fn the_unit_preview_says_it_is_a_preview() {
    let unit_file = "[Unit]\nDescription=bot\n\n[Service]\nExecStart=/opt/bot/current/bot\n";
    let rendered = s(service_unit(&a_service(), unit_file));
    assert!(rendered.contains("class=\"unit\""));
    assert!(rendered.contains("ExecStart=/opt/bot/current/bot"));
    assert!(rendered.contains("This is a preview"));
    assert!(rendered.contains("/etc/systemd/system/bot.service"));
}

#[test]
fn a_unit_file_containing_markup_is_escaped() {
    let rendered = s(service_unit(&a_service(), "ExecStart=/bin/x --html '<b>'"));
    assert!(rendered.contains("&lt;b&gt;"));
    assert!(!rendered.contains("<b>'"));
}

#[test]
fn the_service_form_carries_every_systemd_and_latency_field() {
    let rendered = s(service_form(None, &[a_target()], &[], &[], "t"));
    for field in [
        "name=\"name\"",
        "name=\"target_id\"",
        "name=\"release_root\"",
        "name=\"keep_releases\"",
        "name=\"unit_name\"",
        "name=\"description\"",
        "name=\"exec_args\"",
        "name=\"working_directory\"",
        "name=\"unit_user\"",
        "name=\"unit_group\"",
        "name=\"restart\"",
        "name=\"restart_sec\"",
        "name=\"after\"",
        "name=\"cpu_affinity\"",
        "name=\"nice\"",
        "name=\"io_scheduling_class\"",
        "name=\"extra_directives\"",
        "name=\"env\"",
        "name=\"check_kind\"",
        "name=\"check_http_url\"",
        "name=\"check_command\"",
        "name=\"check_timeout\"",
        "name=\"check_retries\"",
        "name=\"check_initial_delay\"",
        "name=\"artifact_kind\"",
        "name=\"artifact_url\"",
        "name=\"source_id\"",
        "name=\"repo\"",
        "name=\"branch\"",
        "name=\"build_command\"",
        "name=\"artifact_path\"",
        "name=\"auto_deploy_on_push\"",
    ] {
        assert!(
            rendered.contains(field),
            "the service form is missing {field}"
        );
    }
}

#[test]
fn editing_a_service_preselects_its_existing_configuration() {
    let mut service = a_service();
    service.unit = Some(SystemdUnit {
        unit_name: "bot.service".to_string(),
        cpu_affinity: "2-5".to_string(),
        io_scheduling_class: "realtime".to_string(),
        restart: "on-failure".to_string(),
        after: vec![
            "network-online.target".to_string(),
            "redis.service".to_string(),
        ],
        extra_directives: HashMap::from([
            ("LimitNOFILE".to_string(), "65535".to_string()),
            ("LimitMEMLOCK".to_string(), "infinity".to_string()),
        ]),
        ..Default::default()
    });
    service.env = HashMap::from([("RUST_LOG".to_string(), "info".to_string())]);

    let rendered = s(service_form(Some(&service), &[a_target()], &[], &[], "t"));
    assert!(rendered.contains("value=\"2-5\""));
    assert!(rendered.contains("value=\"realtime\" selected"));
    assert!(rendered.contains("value=\"on-failure\" selected"));
    assert!(rendered.contains("value=\"network-online.target,redis.service\""));
    // Extra directives in a stable order, one per line.
    assert!(rendered.contains("LimitMEMLOCK=infinity\nLimitNOFILE=65535"));
    assert!(rendered.contains("RUST_LOG=info"));
    // The existing target is preselected.
    assert!(rendered.contains("value=\"tgt_1\" selected"));
}

#[test]
fn editing_a_target_preselects_its_key_and_flag() {
    let mut target = a_target();
    target.latency_critical = true;
    target.labels.insert("env".to_string(), "prod".to_string());
    let secret = Secret {
        id: "sec_key".to_string(),
        name: "deploy-key".to_string(),
        ..Default::default()
    };

    let rendered = s(target_form(Some(&target), &[secret], "t"));
    assert!(rendered.contains("value=\"sec_key\" selected"));
    assert!(rendered.contains("name=\"latency_critical\" value=\"1\" checked"));
    assert!(rendered.contains("value=\"env=prod\""));
    assert!(rendered.contains("Save target"));
    // Only the edit form offers deletion.
    assert!(rendered.contains("Delete target"));
    assert!(!s(target_form(None, &[], "t")).contains("Delete target"));
}

#[test]
fn the_new_target_form_defaults_the_ssh_port_to_22() {
    let rendered = s(target_form(None, &[], "t"));
    assert!(rendered.contains("name=\"port\" min=\"1\" max=\"65535\" value=\"22\""));
}

#[test]
fn the_deployment_listing_names_the_service_and_the_actor_kind() {
    let deployments = [
        Deployment {
            id: "dep_1".to_string(),
            service_id: "svc_1".to_string(),
            status: deployment::Status::Succeeded as i32,
            actor: Some(Actor::agent("sess_9", "claude")),
            started_at: Some(nudo_proto::to_timestamp(
                chrono::Utc::now() - chrono::Duration::minutes(5),
            )),
            finished_at: Some(nudo_proto::to_timestamp(
                chrono::Utc::now() - chrono::Duration::minutes(4),
            )),
            ..Default::default()
        },
        Deployment {
            id: "dep_2".to_string(),
            service_id: "svc_1".to_string(),
            status: deployment::Status::Failed as i32,
            error: "compile error:\n".to_string() + &"x".repeat(200),
            ..Default::default()
        },
    ];
    let rendered = s(deployments_list(&deployments, &[a_service()]));

    assert!(rendered.contains("bot"), "the service name, not its id");
    assert!(rendered.contains("claude"));
    assert!(rendered.contains("agent"));
    assert!(rendered.contains("succeeded"));
    // The long error is truncated to keep the row one line tall.
    assert!(rendered.contains('…'));
    assert!(!rendered.contains(&"x".repeat(200)));
}

#[test]
fn a_deployment_for_an_unknown_service_falls_back_to_the_id() {
    // The service list handed in may not contain every referenced service.
    let deployments = [Deployment {
        id: "dep_1".to_string(),
        service_id: "svc_gone".to_string(),
        status: deployment::Status::Succeeded as i32,
        ..Default::default()
    }];
    let rendered = s(deployments_list(&deployments, &[]));
    assert!(rendered.contains("svc_gone"));
}

// -- layout ------------------------------------------------------------

#[test]
fn stacked_cards_are_spaced_wherever_they_are_nested() {
    // The bug this pins: `.content > * + *` only reaches direct children of
    // `.content`, so pages that wrap their cards — the service form puts
    // six inside one `<form>`, settings nests them under `.split > div` —
    // rendered them butted together with no gap at all.
    //
    // The stylesheet is what fixes it, so the stylesheet is what is
    // asserted: without a rule keyed on the cards themselves, every one of
    // those pages regresses at once and nothing else would notice.
    let css = include_str!("../../assets/app.css");
    assert!(
        css.contains(".card + .card"),
        "nothing spaces one card from the next, so nested cards touch"
    );
}

#[test]
fn the_pages_that_stack_cards_still_do() {
    // Guards the other half: the rule above is only useful while these
    // pages actually render adjacent cards. If one is restructured, this
    // says so rather than leaving a stylesheet rule for a shape that no
    // longer exists.
    let form = s(service_form(None, &[a_target()], &[], &[], "t"));
    assert!(
        form.matches(r#"class="card""#).count() >= 2,
        "the service form no longer stacks cards"
    );

    let settings = s(settings_page(&[], "a@b.c", &SettingsPrefs::default(), "t"));
    assert!(
        settings.matches(r#"class="card""#).count() >= 2,
        "settings no longer stacks cards"
    );
}

// -- upgrading ---------------------------------------------------------
