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
        git_build_host_id: String::new(),
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
        domain: String::new(),
        port: String::new(),
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
        domain: String::new(),
        port: String::new(),
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
        git_build_host_id: String::new(),
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
        domain: String::new(),
        port: String::new(),
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
        git_build_host_id: String::new(),
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

#[test]
fn a_public_key_pasted_as_a_private_one_is_caught_by_name() {
    // The single most likely mistake: id_ed25519.pub instead of id_ed25519.
    // Stored, it is accepted silently and surfaces much later as a failed
    // connection to a host nobody has reason to doubt.
    assert_eq!(
        looks_like_private_key(
            "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAINbQLN3OR4KHUki7vfmdITOI3q operator@laptop",
        ),
        Err("key-public")
    );

    // The other key types ssh-keygen emits.
    assert_eq!(
        looks_like_private_key("ssh-rsa AAAAB3Nza"),
        Err("key-public")
    );
    assert_eq!(
        looks_like_private_key("ecdsa-sha2-nistp256 AAAAE2V"),
        Err("key-public")
    );
}

#[test]
fn a_truncated_key_is_caught_rather_than_stored() {
    // A paste that lost its last line looks fine until a deploy fails.
    assert_eq!(
        looks_like_private_key("-----BEGIN OPENSSH PRIVATE KEY-----\nb3BlbnNzaC1rZXktdjEAAAAA\n"),
        Err("key-truncated")
    );
}

#[test]
fn something_that_is_not_a_key_at_all_says_what_was_expected() {
    assert_eq!(looks_like_private_key("hunter2"), Err("key-shape"));
    assert_eq!(looks_like_private_key("   "), Err("key-empty"));
}

#[test]
fn the_key_formats_nudo_actually_accepts_are_allowed_through() {
    // A shape check, not a parse: refusing a format nudo supports would be
    // worse than storing something odd, so anything with the right envelope
    // passes.
    for key in [
        "-----BEGIN OPENSSH PRIVATE KEY-----\nb3Blbn\n-----END OPENSSH PRIVATE KEY-----",
        "-----BEGIN RSA PRIVATE KEY-----\nMIIEow\n-----END RSA PRIVATE KEY-----",
        "-----BEGIN EC PRIVATE KEY-----\nMHcCAQ\n-----END EC PRIVATE KEY-----",
        // Surrounding whitespace is a normal consequence of pasting.
        "\n  -----BEGIN OPENSSH PRIVATE KEY-----\nb3Blbn\n-----END OPENSSH PRIVATE KEY-----  \n",
    ] {
        assert!(
            looks_like_private_key(key).is_ok(),
            "should have been accepted: {key:?}"
        );
    }
}
