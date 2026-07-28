use super::*;

// ---- manifest ----

#[test]
fn the_manifest_points_every_url_at_this_control_plane() {
    let manifest = build_manifest("nudo-prod", "https://nudo.example.com/");
    assert_eq!(manifest.url, "https://nudo.example.com");
    assert_eq!(
        manifest.hook_attributes.url,
        "https://nudo.example.com/webhooks/github"
    );
    assert_eq!(
        manifest.redirect_url,
        "https://nudo.example.com/sources/github/callback"
    );
    assert!(manifest.hook_attributes.active);
}

#[test]
fn the_manifest_requests_only_the_permissions_this_tool_uses() {
    let manifest = build_manifest("nudo", "https://n.example.com");
    assert_eq!(
        manifest
            .default_permissions
            .get("contents")
            .map(String::as_str),
        Some("read")
    );
    assert_eq!(
        manifest
            .default_permissions
            .get("metadata")
            .map(String::as_str),
        Some("read")
    );
    // Needed to report a deploy's outcome on the commit.
    assert_eq!(
        manifest
            .default_permissions
            .get("statuses")
            .map(String::as_str),
        Some("write")
    );
    // Nothing here administers a repository.
    assert!(!manifest.default_permissions.contains_key("administration"));

    assert!(manifest.default_events.contains(&"push".to_string()));
    // Private, and no OAuth flow, since nothing acts as a GitHub user.
    assert!(!manifest.public);
    assert!(!manifest.request_oauth_on_install);
}

#[test]
fn the_manifest_serializes_with_the_field_names_github_expects() {
    let json =
        serde_json::to_value(build_manifest("nudo", "https://n.example.com")).expect("serialize");
    for field in [
        "name",
        "url",
        "hook_attributes",
        "redirect_url",
        "callback_urls",
        "setup_url",
        "public",
        "request_oauth_on_install",
        "setup_on_update",
        "default_permissions",
        "default_events",
    ] {
        assert!(json.get(field).is_some(), "missing {field}");
    }
    assert!(json["hook_attributes"]["active"].as_bool().expect("bool"));
}

#[test]
fn personal_and_organization_accounts_post_the_manifest_to_different_urls() {
    assert_eq!(
        manifest_post_url("https://github.com", "", "st4te"),
        "https://github.com/settings/apps/new?state=st4te"
    );
    assert_eq!(
        manifest_post_url("https://github.com/", "acme-corp", "st4te"),
        "https://github.com/organizations/acme-corp/settings/apps/new?state=st4te"
    );
}

#[test]
fn a_state_with_url_unsafe_characters_is_encoded() {
    let url = manifest_post_url("https://github.com", "", "a+b/c=d");
    assert!(!url.contains("a+b/c=d"));
    assert!(url.contains("a%2Bb%2Fc%3Dd"));
}

#[test]
fn the_installation_url_names_the_app_by_slug() {
    assert_eq!(
        installation_url("https://github.com", "my-nudo-app"),
        "https://github.com/apps/my-nudo-app/installations/new"
    );
}

// ---- api url derivation ----

#[test]
fn the_api_url_is_derived_for_each_kind_of_github_host() {
    assert_eq!(
        api_url_from_html_url("https://github.com"),
        "https://api.github.com"
    );
    assert_eq!(
        api_url_from_html_url("https://github.com/"),
        "https://api.github.com"
    );
    // Enterprise Cloud puts the API on a subdomain.
    assert_eq!(
        api_url_from_html_url("https://octocorp.ghe.com"),
        "https://api.octocorp.ghe.com"
    );
    // Enterprise Server puts it under a path.
    assert_eq!(
        api_url_from_html_url("https://git.internal.example.com"),
        "https://git.internal.example.com/api/v3"
    );
}

#[test]
fn host_matching_for_api_derivation_is_case_insensitive() {
    assert_eq!(
        api_url_from_html_url("https://GitHub.com"),
        "https://api.github.com"
    );
}

// ---- jwt ----

// A throwaway 2048-bit RSA key, generated for this test only.
const TEST_KEY: &str = include_str!("../../tests/data/test_app_key.pem");

#[test]
fn an_app_jwt_carries_the_claims_github_requires() {
    let token = sign_app_jwt(TEST_KEY, 123456).expect("sign");

    // Header must say RS256.
    let header = jsonwebtoken::decode_header(&token).expect("header");
    assert_eq!(header.alg, jsonwebtoken::Algorithm::RS256);

    // Decode the payload without verifying, to inspect the claims.
    let payload = token.split('.').nth(1).expect("payload");
    let decoded =
        base64::Engine::decode(&base64::engine::general_purpose::URL_SAFE_NO_PAD, payload)
            .expect("base64");
    let claims: serde_json::Value = serde_json::from_slice(&decoded).expect("json");

    // `iss` is the App id, as a string.
    assert_eq!(claims["iss"], "123456");

    let now = chrono::Utc::now().timestamp();
    let iat = claims["iat"].as_i64().expect("iat");
    let exp = claims["exp"].as_i64().expect("exp");

    // Back-dated, so a fast clock here is not rejected as future-dated.
    assert!(iat <= now - JWT_BACKDATE_SECONDS + 2 && iat >= now - JWT_BACKDATE_SECONDS - 5);
    // Inside GitHub's ten-minute ceiling.
    assert!(
        exp - iat <= 600,
        "exp must be within GitHub's 10 minute limit"
    );
    assert!(exp > now);
}

#[test]
fn a_key_that_is_not_an_rsa_pem_is_rejected_with_a_useful_message() {
    let error =
        sign_app_jwt("-----BEGIN OPENSSH PRIVATE KEY-----\nnope\n", 1).expect_err("must fail");
    assert!(error.to_string().contains("PEM"), "got: {error}");
}

// ---- repo parsing ----

#[test]
fn owner_and_name_are_split_from_a_repository_string() {
    assert_eq!(split_repo("owner/name").expect("split"), ("owner", "name"));
    assert_eq!(
        split_repo(" owner/name ").expect("split"),
        ("owner", "name")
    );
    // A .git suffix is what a clone URL carries.
    assert_eq!(
        split_repo("owner/name.git").expect("split"),
        ("owner", "name")
    );
    assert_eq!(split_repo("owner/name/").expect("split"), ("owner", "name"));
    assert_eq!(
        split_repo("my-org/my_repo.v2").expect("split"),
        ("my-org", "my_repo.v2")
    );
}

#[test]
fn a_repository_that_could_traverse_a_path_is_rejected() {
    // These end up in a URL path and on a command line.
    for hostile in [
        "owner/../../etc/passwd",
        "../owner/name",
        "owner/name/../other",
        "owner/..",
        "../..",
        "owner/na me",
        "owner/name;rm -rf /",
        "owner/$(id)",
        "noslash",
        "/name",
        "owner/",
        "",
    ] {
        assert!(
            split_repo(hostile).is_err(),
            "{hostile:?} should be rejected"
        );
    }
}

// ---- branches ----

#[test]
fn branches_are_ordered_with_the_default_ones_first() {
    let sorted = sort_branches(vec![
        "zebra".to_string(),
        "master".to_string(),
        "alpha".to_string(),
        "main".to_string(),
    ]);
    assert_eq!(sorted, vec!["main", "master", "alpha", "zebra"]);
}

#[test]
fn a_branch_is_extracted_only_from_a_heads_ref() {
    assert_eq!(branch_from_ref("refs/heads/main"), Some("main"));
    assert_eq!(
        branch_from_ref("refs/heads/feature/nested/name"),
        Some("feature/nested/name")
    );

    // A tag push must not be mistaken for a branch push.
    assert_eq!(branch_from_ref("refs/tags/v1.0.0"), None);
    assert_eq!(branch_from_ref("refs/heads/"), None);
    assert_eq!(branch_from_ref("main"), None);
    assert_eq!(branch_from_ref(""), None);
}

// ---- skip markers ----

#[test]
fn a_push_is_skipped_only_when_every_commit_asks_to_skip() {
    assert!(should_skip_deploy(&["chore: docs [skip ci]".to_string()]));
    assert!(should_skip_deploy(&[
        "a [skip ci]".to_string(),
        "b [skip cd]".to_string()
    ]));
    // One real commit means the push should deploy.
    assert!(!should_skip_deploy(&[
        "feat: the actual change".to_string(),
        "chore: docs [skip ci]".to_string()
    ]));
}

#[test]
fn skip_markers_are_recognized_regardless_of_case_and_spelling() {
    assert!(should_skip_deploy(&["docs [SKIP CI]".to_string()]));
    assert!(should_skip_deploy(&["docs [Skip Cd]".to_string()]));
    assert!(should_skip_deploy(&["docs [ci skip]".to_string()]));
}

#[test]
fn a_push_with_no_commit_messages_is_not_skipped() {
    // An empty list must not read as "everything asked to skip".
    assert!(!should_skip_deploy(&[]));
    assert!(!should_skip_deploy(&["".to_string(), "   ".to_string()]));
}

// ---- commit status mapping ----

#[test]
fn deployment_statuses_map_onto_commit_statuses() {
    use nudo_proto::deployment::Status;

    assert_eq!(
        CommitStatus::from_deployment(Status::Queued),
        Some(CommitStatus::Pending)
    );
    assert_eq!(
        CommitStatus::from_deployment(Status::HealthChecking),
        Some(CommitStatus::Pending)
    );
    assert_eq!(
        CommitStatus::from_deployment(Status::Succeeded),
        Some(CommitStatus::Success)
    );
    assert_eq!(
        CommitStatus::from_deployment(Status::Failed),
        Some(CommitStatus::Failure)
    );
    // A rollback means the change did not hold.
    assert_eq!(
        CommitStatus::from_deployment(Status::RolledBack),
        Some(CommitStatus::Failure)
    );
    assert_eq!(
        CommitStatus::from_deployment(Status::Cancelled),
        Some(CommitStatus::Error)
    );
    assert_eq!(CommitStatus::from_deployment(Status::Unspecified), None);
}

#[test]
fn commit_status_names_match_githubs_vocabulary() {
    assert_eq!(CommitStatus::Pending.as_str(), "pending");
    assert_eq!(CommitStatus::Success.as_str(), "success");
    assert_eq!(CommitStatus::Failure.as_str(), "failure");
    assert_eq!(CommitStatus::Error.as_str(), "error");
}
