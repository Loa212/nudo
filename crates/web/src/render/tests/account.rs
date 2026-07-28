use super::*;

#[test]
fn the_sources_page_offers_the_manifest_flow_and_the_existing_app_hint() {
    let rendered = s(sources_list(&[], "t"));
    assert!(rendered.contains("Create a GitHub App"));
    assert!(rendered.contains("name=\"name\""));
    assert!(rendered.contains("name=\"organization\""));
    assert!(rendered.contains("action=\"/sources/github\""));
    assert!(rendered.contains("Already have an App?"));
    // No place to paste a private key: GitHub hands the credentials back.
    assert!(!rendered.to_lowercase().contains("private key"));
}

#[test]
fn an_uninstalled_source_is_a_warning_because_it_cannot_clone() {
    let source = Source {
        id: "src_1".to_string(),
        name: "nudo-deploy".to_string(),
        kind: source::Kind::GithubApp as i32,
        app_slug: "nudo-deploy".to_string(),
        account_login: "acme".to_string(),
        installed: false,
        ..Default::default()
    };
    let rendered = s(sources_list(std::slice::from_ref(&source), "t"));
    assert!(rendered.contains("badge warn"));
    assert!(rendered.contains("not installed"));
    assert!(rendered.contains("github_app"));
    assert!(rendered.contains("acme"));

    let installed = Source {
        installed: true,
        ..source
    };
    let rendered = s(sources_list(&[installed], "t"));
    assert!(rendered.contains("badge ok"));
}

#[test]
fn the_github_handoff_posts_the_manifest_to_github_and_escapes_it() {
    let manifest = r#"{"name":"nudo","url":"https://x/</textarea>"}"#;
    let rendered = s(github_handoff(
        "https://github.com/settings/apps/new?state=abc",
        manifest,
    ));

    assert!(rendered.contains("action=\"https://github.com/settings/apps/new?state=abc\""));
    assert!(rendered.contains("name=\"manifest\""));
    // The manifest is data, not markup: a `</textarea>` inside it must not
    // close the element.
    assert!(!rendered.contains("</textarea>\"}"));
    assert!(rendered.contains("&lt;/textarea&gt;"));
    // It posts to GitHub, so it carries no token of ours; there is nothing
    // of ours to forge.
    assert!(!rendered.contains("name=\"csrf\""));
    assert!(
        rendered.contains("Create the App on GitHub"),
        "and a manual fallback"
    );
}

#[test]
fn a_created_token_is_shown_once_with_that_said_and_not_in_an_input() {
    let rendered = s(token_created("laptop-cli", "nudo_pat_abc123"));

    assert!(rendered.contains("nudo_pat_abc123"));
    assert!(rendered.contains("Copy this now"));
    assert!(rendered.contains("cannot be shown again"));
    // Not an input: a browser restoring the form on a back-navigation would
    // re-send the value.
    assert!(!rendered.contains("<input"));
    assert!(rendered.contains("class=\"unit\""));
}

#[test]
fn a_token_value_containing_markup_is_escaped() {
    let rendered = s(token_created("x", "<script>alert(1)</script>"));
    assert!(!rendered.contains("<script>alert(1)"));
    assert!(rendered.contains("&lt;script&gt;"));
}

#[test]
fn the_audit_log_distinguishes_actor_kinds_and_marks_dry_runs() {
    let entries = [
        AuditEntry {
            id: "aud_1".to_string(),
            at: Some(nudo_proto::to_timestamp(chrono::Utc::now())),
            actor: Some(Actor::human("usr_1", "alice")),
            action: "Deployments.Deploy".to_string(),
            subject_id: "svc_1".to_string(),
            dry_run: false,
            summary: "deployed bot to hft-box".to_string(),
        },
        AuditEntry {
            id: "aud_2".to_string(),
            at: Some(nudo_proto::to_timestamp(chrono::Utc::now())),
            actor: Some(Actor::agent("sess_1", "claude")),
            action: "Deployments.Deploy".to_string(),
            subject_id: "svc_1".to_string(),
            dry_run: true,
            summary: "would deploy bot".to_string(),
        },
    ];
    let rendered = s(audit_list(&entries));

    assert!(rendered.contains("alice"));
    assert!(rendered.contains("claude"));
    assert!(rendered.contains("Deployments.Deploy"));
    // A dry run changed nothing and must not read like a real change.
    assert!(rendered.contains("dry run"));
    assert!(rendered.contains("applied"));
}

#[test]
fn a_refused_action_is_coloured_like_a_failure() {
    // A refusal is the latency-critical guardrail working, and it is what
    // someone reading the audit log is looking for.
    let entries = [AuditEntry {
        id: "aud_1".to_string(),
        at: Some(nudo_proto::to_timestamp(chrono::Utc::now())),
        actor: Some(Actor::agent("sess_1", "claude")),
        action: "Deployments.Deploy refused: latency_critical".to_string(),
        subject_id: "svc_1".to_string(),
        dry_run: false,
        summary: "allow_latency_critical was not set".to_string(),
    }];
    let rendered = s(audit_list(&entries));
    assert!(rendered.contains("class=\"badge bad\""));
    assert!(rendered.contains("refused"));
}

#[test]
fn an_audit_entry_with_no_actor_still_renders() {
    let entries = [AuditEntry {
        id: "aud_1".to_string(),
        action: "Secrets.Put".to_string(),
        ..Default::default()
    }];
    let rendered = s(audit_list(&entries));
    assert!(rendered.contains("Secrets.Put"));
    assert!(rendered.contains("unknown"));
}

#[test]
fn an_audit_summary_is_truncated_and_escaped() {
    let entries = [AuditEntry {
        id: "aud_1".to_string(),
        action: "Targets.Update".to_string(),
        summary: format!("<b>{}</b>", "y".repeat(200)),
        ..Default::default()
    }];
    let rendered = s(audit_list(&entries));
    assert!(!rendered.contains("<b>"));
    assert!(rendered.contains("&lt;b&gt;"));
    assert!(!rendered.contains(&"y".repeat(200)));
}

#[test]
fn settings_shows_token_state_and_never_a_token_secret() {
    let tokens = [
        TokenView {
            id: "tok_1".to_string(),
            name: "laptop".to_string(),
            scopes: "deploy".to_string(),
            last_used: Some(chrono::Utc::now() - chrono::Duration::hours(3)),
            revoked: false,
            created: chrono::Utc::now() - chrono::Duration::days(9),
        },
        TokenView {
            id: "tok_2".to_string(),
            name: "old-ci".to_string(),
            scopes: "admin".to_string(),
            last_used: None,
            revoked: true,
            created: chrono::Utc::now() - chrono::Duration::days(400),
        },
    ];
    let rendered = s(settings_page(
        &tokens,
        "alice@example.com",
        &SettingsPrefs::default(),
        "t",
    ));

    assert!(rendered.contains("alice@example.com"));
    assert!(rendered.contains("3h ago"));
    // Never used is a reason to revoke, so it is stated.
    assert!(rendered.contains("never"));
    assert!(rendered.contains("badge ok"));
    assert!(rendered.contains("badge bad"));
    // A revoked token has nothing left to revoke.
    assert_eq!(rendered.matches(">Revoke<").count(), 1);
    // The TokenView type has no secret field, so nothing can leak one.
    assert!(!rendered.to_lowercase().contains("nudo_pat_"));
}

#[test]
fn the_auth_pages_are_standalone_documents_with_no_rail() {
    for rendered in [s(login_page(None, "t")), s(setup_page(None, "t"))] {
        assert!(rendered.starts_with("<!DOCTYPE html>"));
        assert!(rendered.contains("class=\"auth-page\""));
        assert!(rendered.contains("class=\"auth-card\""));
        // No navigation for someone who is not signed in.
        assert!(!rendered.contains("class=\"rail\""));
        assert!(!rendered.contains("class=\"nav"));
    }
}

#[test]
fn an_auth_error_is_shown_as_a_callout_and_escaped() {
    let rendered = s(login_page(Some("Invalid <email> or password"), "t"));
    assert!(rendered.contains("callout bad"));
    assert!(rendered.contains("&lt;email&gt;"));
    assert!(!rendered.contains("<email>"));

    assert!(!s(login_page(None, "t")).contains("callout bad"));
}

#[test]
fn the_setup_page_says_what_the_first_account_controls() {
    let rendered = s(setup_page(None, "t"));
    assert!(rendered.contains("controls every target"));
    assert!(rendered.contains("name=\"password_confirm\""));
}

#[test]
fn an_error_page_states_the_code_without_leaking_internals() {
    let rendered = s(error_page(502, "The control plane is not responding."));
    assert!(rendered.contains("502"));
    assert!(rendered.contains("The control plane is not responding."));
    assert!(rendered.contains("href=\"/\""));
    assert!(rendered.contains("class=\"auth-page\""));
}

#[test]
fn an_error_message_is_escaped() {
    let rendered = s(error_page(500, "<script>alert(1)</script>"));
    assert!(!rendered.contains("<script>alert(1)"));
    assert!(rendered.contains("&lt;script&gt;"));
}

// -- formatting helpers ------------------------------------------------
