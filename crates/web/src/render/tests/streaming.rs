use super::*;

#[test]
fn the_deployment_fragment_is_lines_only_with_no_wrapper() {
    // Appended into #deploy-log on every event: a wrapper would nest a new
    // log box each time.
    let now = chrono::Utc::now();
    let rendered = s(deployment_log_lines(&[(
        now,
        false,
        "compiling".to_string(),
    )]));
    assert!(rendered.starts_with("<div class=\"line\">"), "{rendered}");
    assert!(!rendered.contains("class=\"logs"));
    assert!(!rendered.contains("id=\"deploy-log\""));
}

#[test]
fn deployment_output_marks_stderr_and_step_markers() {
    let now = chrono::Utc::now();
    let rendered = s(deployment_log_lines(&[
        (now, false, "compiling bot v0.1.0".to_string()),
        (now, true, "warning: unused import".to_string()),
        (now, false, "--- uploading artifact".to_string()),
        // A step marker on stderr is still a step marker.
        (now, true, "--- restarting unit".to_string()),
    ]));

    let lines: Vec<&str> = rendered.matches("<div class=\"line").collect();
    assert_eq!(lines.len(), 4);
    assert!(rendered.contains("<div class=\"line\"><span class=\"at\""));
    assert!(rendered.contains("class=\"line err\""));
    assert_eq!(rendered.matches("class=\"line cmd\"").count(), 2);
}

#[test]
fn deployment_output_containing_markup_is_escaped() {
    // Build output is arbitrary text from a compiler or a remote shell.
    let now = chrono::Utc::now();
    let rendered = s(deployment_log_lines(&[(
        now,
        true,
        "error: <script>alert('x')</script> & <img onerror=y>".to_string(),
    )]));

    assert!(!rendered.contains("<script>"));
    assert!(!rendered.contains("<img"));
    assert!(rendered.contains("&lt;script&gt;"));
    assert!(rendered.contains("&amp;"));
}

#[test]
fn an_empty_deployment_fragment_says_it_is_waiting() {
    let rendered = s(deployment_log_lines(&[]));
    assert!(rendered.contains("placeholder"));
    assert!(rendered.contains("Waiting for output"));
}

#[test]
fn a_live_deployment_subscribes_and_a_finished_one_does_not() {
    let service = a_service();
    let mut deployment = Deployment {
        id: "dep_1".to_string(),
        service_id: "svc_1".to_string(),
        status: deployment::Status::Building as i32,
        started_at: Some(nudo_proto::to_timestamp(chrono::Utc::now())),
        ..Default::default()
    };

    let live = s(deployment_detail(&deployment, &service, &[], true, "tok"));
    assert!(live.contains("hx-ext=\"sse\""));
    assert!(live.contains("sse-connect=\"/deployments/dep_1/stream\""));
    assert!(live.contains("id=\"deploy-log\""));
    assert!(live.contains("sse-swap=\"log\""));
    // Cancellable while running, and it names the consequence.
    assert!(live.contains("Cancel"));
    assert!(live.contains("confirm('Cancel this deployment?"));

    deployment.status = deployment::Status::Succeeded as i32;
    deployment.finished_at = Some(nudo_proto::to_timestamp(chrono::Utc::now()));
    let done = s(deployment_detail(&deployment, &service, &[], false, "tok"));
    assert!(!done.contains("sse-connect"), "nothing to subscribe to");
    assert!(!done.contains(">Cancel<"), "and nothing to cancel");
    assert!(done.contains("id=\"deploy-log\""), "the pane still exists");
}

#[test]
fn a_failed_deployment_shows_its_whole_error() {
    let deployment = Deployment {
        id: "dep_1".to_string(),
        service_id: "svc_1".to_string(),
        status: deployment::Status::Failed as i32,
        error: "health check failed after 3 retries\nlast body: 503".to_string(),
        ..Default::default()
    };
    let rendered = s(deployment_detail(
        &deployment,
        &a_service(),
        &[],
        false,
        "t",
    ));
    assert!(rendered.contains("health check failed after 3 retries"));
    assert!(rendered.contains("last body: 503"), "not truncated here");
}

#[test]
fn the_log_fragment_is_lines_only_with_no_wrapper() {
    let rendered = s(log_lines(&[LogLine {
        at: Some(nudo_proto::to_timestamp(chrono::Utc::now())),
        message: "started".to_string(),
        priority: "6".to_string(),
        ..Default::default()
    }]));
    assert!(rendered.starts_with("<div class=\"line\">"), "{rendered}");
    assert!(!rendered.contains("class=\"logs"));
    assert!(!rendered.contains("id=\"log-pane\""));
}

#[test]
fn journald_priorities_map_to_line_classes() {
    let with = |priority: &str| {
        s(log_lines(&[LogLine {
            message: "m".to_string(),
            priority: priority.to_string(),
            ..Default::default()
        }]))
    };

    // 0 emerg, 1 alert, 2 crit, 3 err.
    for priority in ["0", "1", "2", "3"] {
        assert!(
            with(priority).contains("class=\"line err\""),
            "priority {priority} should be an error"
        );
    }
    assert!(with("4").contains("class=\"line warn\""));
    // 5 notice, 6 info, 7 debug and anything unparseable are ordinary.
    for priority in ["5", "6", "7", "", "not-a-number"] {
        assert!(
            with(priority).contains("class=\"line\""),
            "priority {priority:?} should be ordinary"
        );
    }
}

#[test]
fn log_text_containing_markup_is_escaped() {
    // A service that logs a request body can log anything at all.
    let rendered = s(log_lines(&[LogLine {
        message: "GET /?q=<script>alert(1)</script> \"quoted\" & more".to_string(),
        priority: "3".to_string(),
        ..Default::default()
    }]));

    assert!(!rendered.contains("<script>"));
    assert!(rendered.contains("&lt;script&gt;alert(1)&lt;/script&gt;"));
    assert!(rendered.contains("&amp;"));
    assert!(rendered.contains("class=\"line err\""));
}

#[test]
fn a_log_line_with_no_timestamp_still_renders() {
    let rendered = s(log_lines(&[LogLine {
        message: "no clock".to_string(),
        ..Default::default()
    }]));
    assert!(rendered.contains("--:--:--"));
    assert!(rendered.contains("no clock"));
}

#[test]
fn an_empty_log_says_no_matches_rather_than_nothing() {
    let rendered = s(log_lines(&[]));
    assert!(rendered.contains("No matching log lines"));
}

#[test]
fn following_logs_subscribes_and_carries_the_filter_into_the_stream() {
    let following = s(logs_view(&a_service(), &[], "panic at", true));
    assert!(following.contains("hx-ext=\"sse\""));
    assert!(following.contains("/services/svc_1/logs/stream?grep=panic%20at"));
    assert!(following.contains("id=\"log-pane\""));
    assert!(following.contains("sse-swap=\"log\""));
    assert!(following.contains("Stop following"));

    let static_view = s(logs_view(&a_service(), &[], "", false));
    assert!(!static_view.contains("sse-connect"));
    assert!(static_view.contains("Follow"));
    // The grep box drives the server, not a client-side filter.
    assert!(static_view.contains("hx-get=\"/services/svc_1/logs\""));
    assert!(static_view.contains("hx-target=\"#log-pane\""));
}

#[test]
fn a_grep_value_is_escaped_back_into_its_input() {
    let rendered = s(logs_view(&a_service(), &[], "\"><script>x</script>", false));
    assert!(!rendered.contains("<script>x</script>"));
    assert!(rendered.contains("&lt;script&gt;"));
}

// -- terminal ----------------------------------------------------------

#[test]
fn the_terminal_page_embeds_the_grant_as_json_and_names_no_host() {
    // The browser gets a session id and a token. It must not learn the
    // address of the machine — the server already knows which target the
    // grant is for.
    let target = a_target();
    let rendered = s(terminal_page(&target, "sess_1", "tok_secret"));

    assert!(
        rendered.contains(r#"window.nudoTerminal = {"sessionId":"sess_1","token":"tok_secret"};"#)
    );
    assert!(!rendered.contains("10.0.0.4"), "no host");
    assert!(!rendered.contains(":22"), "no port");
    assert!(!rendered.contains("deploy@"), "no ssh user@host");

    // And the pieces terminal.js needs.
    assert!(rendered.contains("class=\"term-wrap\""));
    assert!(rendered.contains("id=\"terminal\""));
    assert!(rendered.contains("id=\"term-status\""));
    for asset in [
        "/assets/xterm.css",
        "/assets/xterm.js",
        "/assets/xterm-addon-fit.js",
        "/assets/terminal.js",
    ] {
        assert!(rendered.contains(asset), "missing {asset}");
    }
}

#[test]
fn a_token_containing_script_syntax_cannot_break_out_of_the_script_element() {
    // An HTML parser ends a script element at the first literal `</`, which
    // serde_json does not escape, so `terminal_page` rewrites the sequence.
    let rendered = s(terminal_page(
        &a_target(),
        "s",
        "</script><script>alert(1)</script>",
    ));

    assert!(!rendered.contains("</script><script>alert(1)"));
    assert!(rendered.contains(r"<\/script><script>alert(1)<\/script>"));
    // Exactly the four closing tags we wrote: xterm, fit, config,
    // terminal.js. A fifth would mean the token closed one early. A literal
    // `<script` inside the JSON string is harmless — only `</` ends a script
    // element — so only the closing count is asserted.
    assert_eq!(rendered.matches("</script>").count(), 4);
}

#[test]
fn a_terminal_on_a_latency_critical_host_says_what_it_costs() {
    let mut hot = a_target();
    hot.latency_critical = true;
    let rendered = s(terminal_page(&hot, "s", "t"));
    assert!(rendered.contains("badge hot"));
    assert!(rendered.contains("audit log"));
}

// -- shell -------------------------------------------------------------
