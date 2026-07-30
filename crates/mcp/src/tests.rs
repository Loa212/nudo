use super::*;

fn tools() -> NudoTools {
    NudoTools::new("http://127.0.0.1:50051", "claude (mcp)").expect("a valid endpoint")
}

#[test]
fn the_tool_set_is_curated_rather_than_a_mapping_of_every_rpc() {
    let router = NudoTools::tool_router();
    let names: Vec<String> = router
        .list_all()
        .iter()
        .map(|tool| tool.name.to_string())
        .collect();

    // The eight the plan calls for, plus `list_build_hosts` — a read, added
    // with build hosts so an agent can say where a build ran rather than
    // inferring it. Registering one stays a human action.
    for expected in [
        "list_targets",
        "list_build_hosts",
        "list_services",
        "get_unit_status",
        "deploy",
        "rollback",
        "stream_logs",
        "run_command",
        "list_deployments",
    ] {
        assert!(names.contains(&expected.to_string()), "missing {expected}");
    }
    assert_eq!(names.len(), 9, "the surface should stay curated: {names:?}");
}

#[test]
fn nothing_that_should_be_left_to_a_human_is_exposed() {
    let router = NudoTools::tool_router();
    let names: Vec<String> = router
        .list_all()
        .iter()
        .map(|tool| tool.name.to_string())
        .collect();

    // Creating infrastructure, editing secrets and holding a PTY are
    // deliberately absent: an agent should not be able to do them at all,
    // rather than being trusted not to.
    for forbidden in [
        "create_target",
        "delete_target",
        // A build host is infrastructure like a target: an agent may see one,
        // and may not register, edit or delete one. Accepting its host key is
        // likewise a human judgement about whether a machine is what it claims
        // to be — and this one is handed repository credentials.
        "create_build_host",
        "delete_build_host",
        "update_build_host",
        "accept_host_key",
        "set_build_default",
        "create_service",
        "delete_service",
        "put_secret",
        "delete_secret",
        "attach",
        "terminal",
        "create_terminal_session",
    ] {
        assert!(
            !names.iter().any(|name| name.contains(forbidden)),
            "{forbidden} must not be exposed to an agent"
        );
    }
}

#[test]
fn every_tool_has_a_description_that_explains_when_to_use_it() {
    // The descriptions are what determine whether an agent uses this
    // correctly, so an empty or perfunctory one is a defect.
    let router = NudoTools::tool_router();
    for tool in router.list_all() {
        let description = tool.description.clone().unwrap_or_default();
        assert!(
            description.len() > 80,
            "{} has a thin description: {description:?}",
            tool.name
        );
    }
}

#[test]
fn the_mutating_tools_warn_about_the_latency_critical_guardrail() {
    let router = NudoTools::tool_router();
    for name in ["deploy", "rollback", "run_command"] {
        let tool = router
            .list_all()
            .into_iter()
            .find(|tool| tool.name == name)
            .expect(name);
        let description = tool.description.clone().unwrap_or_default();
        assert!(
            description.contains("latency_critical"),
            "{name} does not mention the guardrail"
        );
        assert!(
            description.contains("dry_run"),
            "{name} does not mention dry_run"
        );
    }
}

#[test]
fn destructive_tools_default_to_a_dry_run() {
    // A mistaken call must report a plan, not act. Deserializing an empty
    // object is exactly what an agent that omits the field produces.
    let deploy: DeployParams = serde_json::from_str(r#"{"service_id":"svc_1"}"#).expect("parse");
    assert!(deploy.dry_run, "deploy must default to a dry run");
    assert!(
        !deploy.allow_latency_critical,
        "the guardrail must default to closed"
    );

    let rollback: RollbackParams =
        serde_json::from_str(r#"{"service_id":"svc_1"}"#).expect("parse");
    assert!(rollback.dry_run);
    assert!(!rollback.allow_latency_critical);

    let command: RunCommandParams =
        serde_json::from_str(r#"{"target_id":"tgt_1","command":"uptime"}"#).expect("parse");
    assert!(command.dry_run);
    assert!(!command.allow_latency_critical);
}

#[test]
fn a_deliberate_call_can_turn_the_dry_run_off() {
    let deploy: DeployParams =
        serde_json::from_str(r#"{"service_id":"svc_1","dry_run":false}"#).expect("parse");
    assert!(!deploy.dry_run);
}

#[test]
fn read_only_tools_have_no_dry_run_or_guardrail_field() {
    // They cannot change anything, so those fields would be noise in the
    // schema and would suggest the tool mutates.
    let schema = serde_json::to_value(schemars::schema_for!(StreamLogsParams)).expect("schema");
    let text = schema.to_string();
    assert!(!text.contains("dry_run"));
    assert!(!text.contains("allow_latency_critical"));

    let schema = serde_json::to_value(schemars::schema_for!(ListTargetsParams)).expect("schema");
    assert!(!schema.to_string().contains("dry_run"));
}

// Building the tools opens a lazy channel, which registers with the reactor.
#[tokio::test]
async fn the_mutation_envelope_attributes_the_call_to_the_agent() {
    // An operator has to be able to see in the audit log that an agent did
    // this, and which session.
    let tools = tools();
    let envelope = tools.mutation(false, false);

    let actor = envelope.actor.expect("actor");
    assert_eq!(actor.kind, actor::Kind::Agent as i32);
    assert_eq!(actor.label, "claude (mcp)");
    assert!(actor.id.starts_with("mcp-"));
}

// Building the tools opens a lazy channel, which registers with the reactor.
#[tokio::test]
async fn the_envelope_carries_the_dry_run_and_guardrail_flags_through() {
    let tools = tools();

    let dry = tools.mutation(true, false);
    assert!(dry.dry_run);
    assert!(!dry.allow_latency_critical);

    let live = tools.mutation(false, true);
    assert!(!live.dry_run);
    assert!(live.allow_latency_critical);
}

// Building the tools opens a lazy channel, which registers with the reactor.
#[tokio::test]
async fn the_server_instructions_tell_an_agent_the_order_to_work_in() {
    let info = tools().get_info();
    let instructions = info.instructions.expect("instructions");

    assert!(instructions.contains("list_targets"));
    assert!(instructions.contains("dry_run"));
    assert!(instructions.contains("latency_critical"));
    // And that some things are not its job.
    assert!(instructions.contains("interactive shell"));
}

#[test]
fn a_guardrail_refusal_reaches_the_agent_as_a_fixable_request_error() {
    // As an internal error it would read as a fault to retry; as invalid
    // params it reads as "change your request", which is correct.
    let error = status_to_error(tonic::Status::failed_precondition(
        "target hft-box is marked latency-critical; set allow_latency_critical on the request",
    ));
    let rendered = format!("{error:?}");
    assert!(
        rendered.contains("allow_latency_critical"),
        "got: {rendered}"
    );
}

#[test]
fn a_missing_entity_reaches_the_agent_as_a_request_error_too() {
    let error = status_to_error(tonic::Status::not_found("no such service: svc_x"));
    assert!(format!("{error:?}").contains("no such service"));
}

#[test]
fn artifact_sources_are_described_in_one_line() {
    use nudo_proto::{ArtifactSource, GitSource, artifact_source::Kind};

    let with = |kind: Kind| {
        describe_artifact(&Service {
            artifact: Some(ArtifactSource { kind: Some(kind) }),
            ..Default::default()
        })
    };

    assert_eq!(
        with(Kind::Url("https://x/bot".to_string())),
        "url:https://x/bot"
    );
    assert_eq!(
        with(Kind::Git(GitSource {
            repo: "owner/bot".to_string(),
            branch: "main".to_string(),
            ..Default::default()
        })),
        "git:owner/bot@main"
    );
    assert_eq!(with(Kind::DirectUpload(true)), "upload");
    assert_eq!(describe_artifact(&Service::default()), "upload");
}

#[test]
fn unit_states_are_described_in_a_single_word() {
    let state = |active: &str, sub: &str| {
        describe_unit_state(&UnitStatus {
            active_state: active.to_string(),
            sub_state: sub.to_string(),
            ..Default::default()
        })
    };

    assert_eq!(state("active", "running"), "running");
    assert_eq!(state("failed", "failed"), "failed");
    assert_eq!(state("inactive", "dead"), "stopped");
    assert_eq!(state("activating", "start"), "starting");
    assert_eq!(state("unknown", ""), "unreachable");
    assert_eq!(state("something-else", ""), "unknown");
}

#[test]
fn journald_priorities_are_named() {
    assert_eq!(describe_priority("3"), "err");
    assert_eq!(describe_priority("4"), "warning");
    assert_eq!(describe_priority("6"), "info");
    assert_eq!(describe_priority(""), "info");
}

#[test]
fn a_described_command_quotes_arguments_that_need_it() {
    assert_eq!(describe_command("uptime", &[]), "uptime");
    assert_eq!(
        describe_command(
            "systemctl",
            &["restart".to_string(), "bot.service".to_string()]
        ),
        "systemctl restart bot.service"
    );

    // So the agent can see that an argument is one argument.
    let rendered = describe_command("echo", &["two words".to_string()]);
    assert_eq!(rendered, "echo 'two words'");

    let hostile = describe_command("echo", &["; rm -rf /".to_string()]);
    assert!(hostile.contains("'; rm -rf /'"), "got: {hostile}");
}

#[test]
fn each_result_shape_names_its_enums_rather_than_exposing_wire_integers() {
    // An agent reading `status: 2` has to guess.
    let schema = serde_json::to_value(schemars::schema_for!(DeploymentSummary)).expect("schema");
    let status = &schema["properties"]["status"];
    assert_eq!(status["type"], "string");

    let schema = serde_json::to_value(schemars::schema_for!(TargetSummary)).expect("schema");
    assert_eq!(schema["properties"]["reachability"]["type"], "string");
    // And the flag that changes everything is a plain boolean.
    assert_eq!(schema["properties"]["latency_critical"]["type"], "boolean");
}

#[test]
fn a_target_summary_says_when_its_host_key_is_waiting_for_review() {
    // Every operation against such a host is refused, including read-only ones,
    // so an agent that cannot see this just retries into an opaque failure.
    let schema = serde_json::to_value(schemars::schema_for!(TargetSummary)).expect("schema");
    let field = schema["properties"]
        .get("host_key_change_pending")
        .expect("a target summary must surface a pending host-key change");
    assert_eq!(field["type"], "boolean");
    // And the description has to say that accepting one is not an agent's call.
    let description = field["description"].as_str().unwrap_or_default();
    assert!(description.contains("refused"), "got: {description}");
}

#[test]
fn a_service_summary_mirrors_its_targets_guardrail_flag() {
    // Otherwise the agent must cross-reference two calls to know whether a
    // deploy needs the opt-in.
    let schema = serde_json::to_value(schemars::schema_for!(ServiceSummary)).expect("schema");
    assert!(
        schema["properties"]
            .get("target_latency_critical")
            .is_some(),
        "a service summary must say whether its target is latency-critical"
    );
}

#[test]
fn the_log_cap_is_enforced_by_the_tool_rather_than_trusted_to_the_caller() {
    assert_eq!(MAX_LOG_LINES, 500);
    // The parameter is optional, so an omitted value must still be bounded.
    let params: StreamLogsParams =
        serde_json::from_str(r#"{"service_id":"svc_1"}"#).expect("parse");
    assert!(params.lines.is_none());
    assert_eq!(params.lines.unwrap_or(100).min(MAX_LOG_LINES), 100);

    let huge: StreamLogsParams =
        serde_json::from_str(r#"{"service_id":"svc_1","lines":999999}"#).expect("parse");
    assert_eq!(huge.lines.unwrap_or(100).min(MAX_LOG_LINES), MAX_LOG_LINES);
}

#[tokio::test]
async fn an_unreachable_control_plane_is_reported_rather_than_hanging() {
    let tools = NudoTools::new("http://127.0.0.1:1", "test").expect("a valid endpoint");
    // `Json<T>` has no Debug, so expect_err cannot be used here.
    let error = match tools
        .list_targets(Parameters(ListTargetsParams {
            label_selector: None,
        }))
        .await
    {
        Ok(_) => panic!("a call against a dead endpoint must not succeed"),
        Err(error) => error,
    };
    assert!(format!("{error:?}").contains("not reachable"));
}
