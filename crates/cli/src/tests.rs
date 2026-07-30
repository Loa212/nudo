use super::*;

#[test]
fn the_command_tree_is_well_formed() {
    // Catches duplicate flags, conflicting shorts and bad defaults at test
    // time rather than on a user's first run.
    use clap::CommandFactory;
    Cli::command().debug_assert();
}

#[test]
fn labels_parse_from_key_equals_value() {
    let parsed =
        parse_labels(&["env=prod".to_string(), " role = indexer ".to_string()]).expect("parse");
    assert_eq!(parsed.get("env").map(String::as_str), Some("prod"));
    assert_eq!(parsed.get("role").map(String::as_str), Some("indexer"));
}

#[test]
fn a_label_without_an_equals_sign_is_rejected_with_advice() {
    let error = parse_labels(&["justakey".to_string()]).expect_err("must fail");
    assert!(error.to_string().contains("key=value"), "got: {error}");
    assert!(parse_labels(&["=value".to_string()]).is_err());
}

#[test]
fn a_mutation_carries_a_human_actor_and_the_global_flags() {
    let cli = Cli::parse_from([
        "nudo",
        "--dry-run",
        "--allow-latency-critical",
        "--idempotency-key",
        "ci-42",
        "targets",
        "list",
    ]);
    let mutation = mutation(&cli);

    assert!(mutation.dry_run);
    assert!(mutation.allow_latency_critical);
    assert_eq!(mutation.idempotency_key, "ci-42");

    let actor = mutation.actor.expect("actor");
    assert_eq!(actor.kind, actor::Kind::Human as i32);
    // The label identifies who and from where, for the audit log.
    assert!(actor.label.contains("(cli)"));
}

#[test]
fn the_guardrail_flags_default_to_off() {
    // A deploy must not touch a latency-critical box unless asked.
    let cli = Cli::parse_from(["nudo", "targets", "list"]);
    let mutation = mutation(&cli);
    assert!(!mutation.dry_run);
    assert!(!mutation.allow_latency_critical);
    assert!(mutation.idempotency_key.is_empty());
}

#[test]
fn an_unreachable_control_plane_still_says_what_to_check() {
    // The channel connects lazily now, so this failure arrives as a transport
    // status reading "transport error". The advice has to survive that.
    let cli = Cli::parse_from(["nudo", "--endpoint", "http://box:50051", "targets", "list"]);
    let rendered = explain(&cli, tonic::Status::unavailable("transport error").into());

    assert!(rendered.contains("http://box:50051"), "got: {rendered}");
    assert!(
        rendered.contains("is nudo-server running?"),
        "got: {rendered}"
    );
}

#[test]
fn an_error_the_server_meant_for_a_human_is_left_alone() {
    let cli = Cli::parse_from(["nudo", "targets", "list"]);
    let rendered = explain(
        &cli,
        tonic::Status::failed_precondition("target hft is latency-critical").into(),
    );

    assert_eq!(rendered, "target hft is latency-critical");
}

#[test]
fn the_dry_run_prefix_marks_output_that_did_not_happen() {
    let dry = Cli::parse_from(["nudo", "--dry-run", "targets", "list"]);
    assert!(dry_run_prefix(&dry).contains("would"));

    let real = Cli::parse_from(["nudo", "targets", "list"]);
    assert!(dry_run_prefix(&real).is_empty());
}

#[test]
fn terminal_size_falls_back_to_a_conventional_default() {
    // Read from the environment, so an unset COLUMNS must not yield zero.
    let (cols, rows) = terminal_size();
    assert!(cols > 0);
    assert!(rows > 0);
}

#[test]
fn unit_states_get_a_badge_and_a_label() {
    let status = |active: &str, sub: &str| UnitStatus {
        active_state: active.to_string(),
        sub_state: sub.to_string(),
        ..Default::default()
    };

    assert_eq!(format_status_badge(&status("active", "running")), "[ok]");
    assert_eq!(format_status_badge(&status("failed", "failed")), "[!!]");
    assert_eq!(format_status_badge(&status("inactive", "dead")), "[--]");
    assert_eq!(format_status_badge(&status("unknown", "")), "[??]");

    assert_eq!(units_label(&status("active", "running")), "running");
    assert_eq!(units_label(&status("unknown", "")), "unreachable");
}

#[test]
fn a_unit_status_line_includes_the_operational_numbers() {
    let line = format::unit_status_line(&UnitStatus {
        active_state: "active".to_string(),
        sub_state: "running".to_string(),
        pid: 4242,
        memory_bytes: 52_428_800,
        restart_count: 3,
        ..Default::default()
    });

    assert!(line.contains("[ok]"));
    assert!(line.contains("running"));
    assert!(line.contains("pid 4242"));
    assert!(line.contains("50.0 MiB"));
    assert!(line.contains("restarts 3"));
}

#[test]
fn a_stopped_unit_omits_the_numbers_that_do_not_apply() {
    let line = format::unit_status_line(&UnitStatus {
        active_state: "inactive".to_string(),
        sub_state: "dead".to_string(),
        ..Default::default()
    });
    assert!(line.contains("stopped"));
    assert!(!line.contains("pid"));
    assert!(!line.contains("mem"));
}

#[test]
fn the_json_shape_for_secrets_has_no_field_that_could_hold_a_value() {
    let json = serde_json::to_value(JsonSecrets::from(&vec![Secret {
        id: "sec_1".to_string(),
        name: "API_KEY".to_string(),
        digest: "abc".to_string(),
        ..Default::default()
    }]))
    .expect("serialize");

    let secret = &json["secrets"][0];
    assert!(secret.get("value").is_none());
    assert!(secret.get("digest").is_some());
    let keys: Vec<&String> = secret.as_object().expect("object").keys().collect();
    assert_eq!(keys.len(), 4, "id, name, scope, digest — and nothing else");
}

#[test]
fn json_output_renders_enums_as_names_rather_than_numbers() {
    // A script should not have to know that 2 means reachable.
    let json = serde_json::to_value(JsonTargets::from(&vec![Target {
        id: "tgt_1".to_string(),
        status: target::Status::Reachable as i32,
        ..Default::default()
    }]))
    .expect("serialize");
    assert_eq!(json["targets"][0]["status"], "reachable");
}

#[test]
fn a_build_hosts_json_output_renders_enums_as_names() {
    let json = serde_json::to_value(JsonBuildHosts::from(&vec![BuildHost {
        id: "bh_1".to_string(),
        status: build_host::Status::Reachable as i32,
        workspace_root: "/var/lib/nudo/builds".to_string(),
        ..Default::default()
    }]))
    .expect("serialize");
    assert_eq!(json["build_hosts"][0]["status"], "reachable");
    assert_eq!(
        json["build_hosts"][0]["workspace_root"],
        "/var/lib/nudo/builds"
    );
}

#[test]
fn a_build_host_check_carries_its_warnings_without_failing() {
    // The decision on this issue: a latency-critical build host is allowed and
    // warned about. A script gating on `ok` must not be tripped by the warning.
    let json = serde_json::to_value(JsonChecks::from(&CheckBuildHostResponse {
        ok: true,
        checks: vec![check_build_host_response::Check {
            name: "git".to_string(),
            ok: true,
            detail: "git version 2.43.0".to_string(),
        }],
        warnings: vec!["This build host is marked latency-critical.".to_string()],
    }))
    .expect("serialize");

    assert_eq!(json["ok"], true);
    assert_eq!(
        json["warnings"][0],
        "This build host is marked latency-critical."
    );
}

#[test]
fn a_target_check_reports_no_warnings_field_at_all() {
    // The warnings field is skipped when empty, so the target output is
    // byte-identical to what it was before build hosts existed.
    let json = serde_json::to_value(JsonChecks::from(&CheckTargetResponse {
        ok: true,
        checks: Vec::new(),
    }))
    .expect("serialize");
    assert!(json.get("warnings").is_none(), "got: {json}");
}

#[test]
fn the_build_host_default_accepts_a_host_or_local_but_not_both() {
    // They are mutually exclusive: `--local` is the sentinel that pins the
    // control plane, so naming a host alongside it is contradictory.
    assert!(
        Cli::try_parse_from(["nudo", "build-hosts", "default", "bh_1", "--local"]).is_err(),
        "an id and --local must not be accepted together"
    );
    assert!(Cli::try_parse_from(["nudo", "build-hosts", "default", "bh_1"]).is_ok());
    assert!(Cli::try_parse_from(["nudo", "build-hosts", "default", "--local"]).is_ok());
}

#[test]
fn a_build_hosts_host_key_cannot_be_accepted_and_forgotten_at_once() {
    assert!(
        Cli::try_parse_from([
            "nudo",
            "build-hosts",
            "host-key",
            "bh_1",
            "--accept",
            "SHA256:x",
            "--forget",
        ])
        .is_err()
    );
}

#[tokio::test]
async fn a_local_artifact_is_served_over_loopback_at_an_unguessable_path() {
    // This is how `--artifact` reaches the control plane: no upload RPC, and
    // the binary is never staged anywhere on the way.
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("bot");
    std::fs::write(&path, b"ELF fake binary").expect("write");

    let server = ArtifactServer::start(&path).await.expect("start");
    assert_eq!(server.size, 15);
    assert_eq!(server.name, "bot");
    // Loopback only, so nothing off this machine can reach it.
    assert!(
        server.url.starts_with("http://127.0.0.1:"),
        "got: {}",
        server.url
    );

    let fetched = reqwest::get(&server.url).await.expect("fetch");
    assert!(fetched.status().is_success());
    assert_eq!(
        fetched.bytes().await.expect("body").as_ref(),
        b"ELF fake binary"
    );

    // Another local process cannot guess the path.
    let base = server.url.rsplit_once('/').expect("split").0;
    let guessed = reqwest::get(format!("{base}/artifact"))
        .await
        .expect("fetch");
    assert_eq!(guessed.status().as_u16(), 404);
}

#[tokio::test]
async fn an_empty_or_missing_artifact_is_refused_before_a_deploy_is_queued() {
    let dir = tempfile::tempdir().expect("tempdir");

    let empty = dir.path().join("empty");
    std::fs::write(&empty, b"").expect("write");
    assert!(ArtifactServer::start(&empty).await.is_err());

    assert!(
        ArtifactServer::start(&dir.path().join("nonexistent"))
            .await
            .is_err()
    );
}

#[test]
fn the_two_artifact_sources_are_mutually_exclusive() {
    // Passing both would leave which one wins ambiguous.
    assert!(
        Cli::try_parse_from([
            "nudo",
            "deploy",
            "svc_1",
            "--artifact",
            "./bot",
            "--artifact-url",
            "https://example.com/bot",
        ])
        .is_err()
    );

    // Either alone parses.
    assert!(Cli::try_parse_from(["nudo", "deploy", "svc_1", "--artifact", "./bot"]).is_ok());
    assert!(
        Cli::try_parse_from(["nudo", "deploy", "svc_1", "--artifact-url", "https://x/bot"]).is_ok()
    );
}

#[test]
fn deploy_defaults_to_rolling_back_on_a_failed_health_check() {
    // Parsing check: the flag that makes unattended CI deploys safe is not
    // something the user has to remember.
    let cli = Cli::parse_from(["nudo", "deploy", "svc_1"]);
    match cli.command {
        Command::Deploy {
            skip_health_check,
            wait,
            ..
        } => {
            assert!(!skip_health_check);
            assert!(!wait, "--wait is opt-in");
        }
        _ => panic!("expected deploy"),
    }
}

#[test]
fn every_subcommand_group_parses() {
    // A smoke test over the surface the README documents.
    for args in [
        vec!["nudo", "init"],
        vec!["nudo", "targets", "list"],
        vec![
            "nudo",
            "targets",
            "add",
            "box",
            "--host",
            "h",
            "--ssh-key",
            "sec_1",
        ],
        vec!["nudo", "targets", "check", "tgt_1"],
        vec!["nudo", "build-hosts", "list"],
        vec![
            "nudo",
            "build-hosts",
            "add",
            "builder",
            "--host",
            "h",
            "--ssh-key",
            "sec_1",
        ],
        vec!["nudo", "build-hosts", "check", "bh_1"],
        vec!["nudo", "build-hosts", "host-key", "bh_1"],
        vec!["nudo", "build-hosts", "default"],
        vec!["nudo", "build-hosts", "default", "bh_1"],
        vec!["nudo", "build-hosts", "default", "--local"],
        vec!["nudo", "services", "list"],
        vec!["nudo", "services", "unit", "svc_1"],
        vec!["nudo", "services", "restart", "svc_1"],
        vec!["nudo", "services", "releases", "svc_1"],
        vec!["nudo", "deploy", "svc_1", "--wait"],
        vec!["nudo", "rollback", "svc_1"],
        vec!["nudo", "logs", "svc_1", "-f"],
        vec!["nudo", "exec", "tgt_1", "uptime"],
        vec!["nudo", "terminal", "tgt_1"],
        vec!["nudo", "secrets", "list"],
        vec!["nudo", "secrets", "set", "KEY", "--value", "v"],
        vec!["nudo", "secrets", "rotate", "KEY", "--value", "v"],
        vec!["nudo", "audit"],
        vec!["nudo", "sources"],
    ] {
        Cli::try_parse_from(&args).unwrap_or_else(|e| panic!("{args:?} failed to parse: {e}"));
    }
}

#[test]
fn global_flags_apply_to_exec_when_given_before_the_subcommand() {
    // `exec` takes trailing var args, so anything after the target belongs
    // to the remote command — including `--allow-latency-critical`. This is
    // the command where that flag matters most, so the position dependence
    // is pinned here rather than discovered against a production host.
    let cli = Cli::parse_from([
        "nudo",
        "--allow-latency-critical",
        "exec",
        "tgt_1",
        "systemctl",
        "restart",
        "bot",
    ]);
    assert!(mutation(&cli).allow_latency_critical);
    match cli.command {
        Command::Exec { command, .. } => {
            assert_eq!(command, vec!["systemctl", "restart", "bot"]);
        }
        _ => panic!("expected exec"),
    }

    // After the target, the same text is part of the remote command.
    let swallowed = Cli::parse_from([
        "nudo",
        "exec",
        "tgt_1",
        "uptime",
        "--allow-latency-critical",
    ]);
    assert!(
        !mutation(&swallowed).allow_latency_critical,
        "a flag after the target is part of the remote command, not nudo's"
    );
    match swallowed.command {
        Command::Exec { command, .. } => {
            assert_eq!(command, vec!["uptime", "--allow-latency-critical"]);
        }
        _ => panic!("expected exec"),
    }
}

#[test]
fn exec_captures_trailing_arguments_including_flags() {
    // `nudo exec tgt systemctl status --no-pager` must not have --no-pager
    // interpreted as a nudo flag.
    let cli = Cli::parse_from(["nudo", "exec", "tgt_1", "systemctl", "status", "--no-pager"]);
    match cli.command {
        Command::Exec {
            target, command, ..
        } => {
            assert_eq!(target, "tgt_1");
            assert_eq!(command, vec!["systemctl", "status", "--no-pager"]);
        }
        _ => panic!("expected exec"),
    }
}

#[test]
fn storing_and_rotating_a_secret_are_different_commands() {
    // A stored value cannot be read back, so replacing one destroys something
    // unrecoverable. Separate verbs mean a `set` re-run from shell history
    // cannot do it — there is no flag to leave behind.
    let cli = Cli::parse_from(["nudo", "secrets", "set", "API_KEY", "--value", "v"]);
    assert!(matches!(
        cli.command,
        Command::Secrets(SecretCommand::Set { .. })
    ));

    let cli = Cli::parse_from(["nudo", "secrets", "rotate", "API_KEY", "--value", "v"]);
    assert!(matches!(
        cli.command,
        Command::Secrets(SecretCommand::Rotate { .. })
    ));

    // The flag that used to do this is gone, so a script carrying it fails
    // loudly rather than silently doing nothing.
    assert!(
        Cli::try_parse_from([
            "nudo",
            "secrets",
            "set",
            "API_KEY",
            "--value",
            "v",
            "--replace"
        ])
        .is_err(),
        "--replace must not be silently accepted"
    );
}

// ---- route parsing ----

#[test]
fn a_route_is_parsed_from_domain_and_port() {
    let route = crate::service_commands::parse_route("api.example.com:8080", false).expect("parse");
    assert_eq!(route.domain, "api.example.com");
    assert_eq!(route.port, 8080);
    assert!(route.path.is_empty());
    assert_eq!(route.protocol_or_default(), route::Protocol::Unspecified);
}

#[test]
fn a_route_can_carry_a_path() {
    let route = crate::service_commands::parse_route("example.com/api:9090", false).expect("parse");
    assert_eq!(route.domain, "example.com");
    assert_eq!(route.path, "/api");
    assert_eq!(route.port, 9090);
}

#[test]
fn a_pasted_url_is_accepted_rather_than_refused() {
    // The obvious mistake, and its meaning is unambiguous.
    for raw in [
        "https://api.example.com:8080",
        "http://api.example.com:8080",
    ] {
        let route = crate::service_commands::parse_route(raw, false).expect("parse");
        assert_eq!(route.domain, "api.example.com");
        assert_eq!(route.port, 8080);
    }
}

#[test]
fn the_grpc_flag_sets_the_protocol() {
    let route =
        crate::service_commands::parse_route("grpc.example.com:50051", true).expect("parse");
    assert_eq!(
        route.protocol_or_default(),
        route::Protocol::H2c,
        "gRPC needs HTTP/2 end to end, so the protocol has to reach the server"
    );
}

#[test]
fn a_route_without_a_port_is_refused_where_the_argument_is() {
    // Refused in the CLI so the message names the argument, rather than
    // arriving as a gRPC status about a field.
    let error = crate::service_commands::parse_route("api.example.com", false)
        .expect_err("a port is required");
    assert!(format!("{error:#}").contains("needs a port"), "{error:#}");
}

#[test]
fn a_route_with_a_bad_domain_is_refused_before_it_is_sent() {
    assert!(crate::service_commands::parse_route("not a domain:8080", false).is_err());
    assert!(crate::service_commands::parse_route("localhost:8080", false).is_err());
    assert!(crate::service_commands::parse_route("api.example.com:abc", false).is_err());
    assert!(crate::service_commands::parse_route("api.example.com:0", false).is_err());
}

#[test]
fn a_route_that_could_inject_proxy_config_is_refused() {
    assert!(
        crate::service_commands::parse_route("evil.com {\n\trespond \"x\"\n}:8080", false).is_err()
    );
    assert!(
        crate::service_commands::parse_route("api.example.com/a b:8080", false).is_err(),
        "a path with whitespace must not reach the renderer"
    );
}
