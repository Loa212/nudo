//! GitHub integration tests against a mocked API.
//!
//! `wiremock` stands in for github.com so the manifest exchange, token minting,
//! expiry and refresh, and commit-status writeback are exercised against real
//! HTTP — including the request headers and bodies GitHub actually requires —
//! without a network or an account.

use nudo_server::crypto::SecretKey;
use nudo_server::github::{self, CommitStatus, GithubClient};
use nudo_server::store::Store;
use wiremock::matchers::{body_json, header, method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// A throwaway RSA key, the format GitHub issues for an App.
const TEST_KEY: &str = include_str!("data/test_app_key.pem");

/// A source with credentials attached, ready to mint tokens.
async fn configured_source(store: &Store, key: &SecretKey, api_url: &str) -> (String, i64) {
    let source = store
        .create_pending_github_source("test-app", "", api_url, "https://github.com")
        .await
        .expect("create the source");

    store
        .attach_github_credentials(
            key,
            &source.id,
            &nudo_server::store::GithubAppCredentials {
                app_id: 123456,
                slug: "test-app".to_string(),
                client_id: "Iv1.abc".to_string(),
                client_secret: "client-secret".to_string(),
                private_key: TEST_KEY.to_string(),
                webhook_secret: "webhook-secret".to_string(),
                html_url: "https://github.com/apps/test-app".to_string(),
            },
        )
        .await
        .expect("attach the credentials");

    store
        .set_installation(&source.id, 42, "acme-corp")
        .await
        .expect("record the installation");

    (source.id, 123456)
}

#[tokio::test]
async fn a_manifest_code_is_exchanged_for_the_apps_credentials() {
    let server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/app-manifests/temp-code-123/conversions"))
        // GitHub requires this Accept header for the conversion endpoint.
        .and(header("accept", "application/vnd.github+json"))
        .respond_with(ResponseTemplate::new(201).set_body_json(serde_json::json!({
            "id": 654321,
            "slug": "my-nudo-app",
            "client_id": "Iv1.deadbeef",
            "client_secret": "the-client-secret",
            "pem": TEST_KEY,
            "webhook_secret": "the-webhook-secret",
            "html_url": "https://github.com/apps/my-nudo-app",
        })))
        .expect(1)
        .mount(&server)
        .await;

    let client = GithubClient::new(&server.uri()).expect("client");
    let credentials = client
        .exchange_manifest_code("temp-code-123")
        .await
        .expect("exchange");

    assert_eq!(credentials.app_id, 654321);
    assert_eq!(credentials.slug, "my-nudo-app");
    assert_eq!(credentials.webhook_secret, "the-webhook-secret");
    assert!(credentials.private_key.contains("PRIVATE KEY"));
}

#[tokio::test]
async fn a_conversion_response_missing_the_webhook_secret_is_refused() {
    // Without it, deliveries could not be verified — so this must fail at setup
    // rather than silently produce a source whose webhooks are unauthenticated.
    let server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/app-manifests/code/conversions"))
        .respond_with(ResponseTemplate::new(201).set_body_json(serde_json::json!({
            "id": 1,
            "slug": "app",
            "pem": TEST_KEY,
            // No webhook_secret.
        })))
        .mount(&server)
        .await;

    let client = GithubClient::new(&server.uri()).expect("client");
    let error = client
        .exchange_manifest_code("code")
        .await
        .expect_err("must be refused");
    assert!(error.to_string().contains("webhook secret"), "got: {error}");
}

#[tokio::test]
async fn a_conversion_response_missing_the_private_key_is_refused() {
    let server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/app-manifests/code/conversions"))
        .respond_with(ResponseTemplate::new(201).set_body_json(serde_json::json!({
            "id": 1,
            "slug": "app",
            "pem": "",
            "webhook_secret": "s",
        })))
        .mount(&server)
        .await;

    let client = GithubClient::new(&server.uri()).expect("client");
    let error = client
        .exchange_manifest_code("code")
        .await
        .expect_err("must be refused");
    assert!(error.to_string().contains("private key"), "got: {error}");
}

#[tokio::test]
async fn a_rejected_exchange_reports_githubs_message() {
    let server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/app-manifests/expired/conversions"))
        .respond_with(ResponseTemplate::new(422).set_body_json(serde_json::json!({
            "message": "This code has already been used.",
        })))
        .mount(&server)
        .await;

    let client = GithubClient::new(&server.uri()).expect("client");
    let error = client
        .exchange_manifest_code("expired")
        .await
        .expect_err("must fail");

    let message = error.to_string();
    assert!(message.contains("422"), "the status should be reported");
    // GitHub's own explanation is what an operator needs to see.
    assert!(message.contains("already been used"), "got: {message}");
}

#[tokio::test]
async fn an_installation_token_is_minted_with_a_signed_app_jwt() {
    let server = MockServer::start().await;
    let expires_at = chrono::Utc::now() + chrono::Duration::hours(1);

    Mock::given(method("POST"))
        .and(path("/app/installations/42/access_tokens"))
        .and(header("accept", "application/vnd.github+json"))
        .and(header("x-github-api-version", "2022-11-28"))
        .respond_with(ResponseTemplate::new(201).set_body_json(serde_json::json!({
            "token": "ghs_mintedtoken",
            "expires_at": expires_at.to_rfc3339(),
        })))
        .expect(1)
        .mount(&server)
        .await;

    let jwt = github::sign_app_jwt(TEST_KEY, 123456).expect("sign");
    let client = GithubClient::new(&server.uri()).expect("client");
    let token = client
        .create_installation_token(&jwt, 42)
        .await
        .expect("mint");

    assert_eq!(token.token, "ghs_mintedtoken");
    // The expiry drives the cache, so it must be read rather than guessed.
    assert_eq!(token.expires_at.timestamp(), expires_at.timestamp());
}

#[tokio::test]
async fn the_minted_token_carries_a_bearer_jwt_signed_by_the_apps_key() {
    // The Authorization header is what GitHub authenticates, so it is asserted
    // rather than assumed: a bearer token, in three JWT segments, RS256.
    let server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/app/installations/42/access_tokens"))
        .respond_with(ResponseTemplate::new(201).set_body_json(serde_json::json!({
            "token": "ghs_x",
            "expires_at": chrono::Utc::now().to_rfc3339(),
        })))
        .mount(&server)
        .await;

    let jwt = github::sign_app_jwt(TEST_KEY, 123456).expect("sign");
    let client = GithubClient::new(&server.uri()).expect("client");
    client
        .create_installation_token(&jwt, 42)
        .await
        .expect("mint");

    let requests = server.received_requests().await.expect("requests");
    let authorization = requests[0]
        .headers
        .get("authorization")
        .and_then(|value| value.to_str().ok())
        .expect("an Authorization header");

    let presented = authorization
        .strip_prefix("Bearer ")
        .expect("a bearer token");
    assert_eq!(presented.split('.').count(), 3, "not a JWT");

    let header = jsonwebtoken::decode_header(presented).expect("decode the header");
    assert_eq!(header.alg, jsonwebtoken::Algorithm::RS256);
}

#[tokio::test]
async fn a_refused_token_request_is_reported() {
    let server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/app/installations/42/access_tokens"))
        .respond_with(ResponseTemplate::new(401).set_body_json(serde_json::json!({
            "message": "A JSON web token could not be decoded",
        })))
        .mount(&server)
        .await;

    let jwt = github::sign_app_jwt(TEST_KEY, 123456).expect("sign");
    let client = GithubClient::new(&server.uri()).expect("client");
    let error = client
        .create_installation_token(&jwt, 42)
        .await
        .expect_err("must fail");
    assert!(error.to_string().contains("could not be decoded"));
}

#[tokio::test]
async fn a_cached_token_is_reused_rather_than_minted_again() {
    // Coolify re-mints on every operation — a JWT signature plus two HTTP
    // round-trips per clone, per branch listing, per page of branches. This
    // asserts the cache actually prevents that.
    let server = MockServer::start().await;
    let store = Store::open_in_memory().await.expect("store");
    let key = SecretKey::generate();
    let (source_id, _) = configured_source(&store, &key, &server.uri()).await;

    Mock::given(method("POST"))
        .and(path("/app/installations/42/access_tokens"))
        .respond_with(ResponseTemplate::new(201).set_body_json(serde_json::json!({
            "token": "ghs_cached",
            "expires_at": (chrono::Utc::now() + chrono::Duration::hours(1)).to_rfc3339(),
        })))
        // Exactly once, however many times a token is asked for.
        .expect(1)
        .mount(&server)
        .await;

    for _ in 0..5 {
        let token = github::installation_token(&store, &key, &source_id)
            .await
            .expect("token");
        assert_eq!(token, "ghs_cached");
    }
}

#[tokio::test]
async fn a_token_nearing_expiry_is_refreshed_rather_than_reused() {
    // A token that expires mid-clone is worse than no token, so one inside the
    // refresh margin must be replaced.
    let server = MockServer::start().await;
    let store = Store::open_in_memory().await.expect("store");
    let key = SecretKey::generate();
    let (source_id, _) = configured_source(&store, &key, &server.uri()).await;

    // Cache one that is technically still valid but nearly gone.
    store
        .cache_installation_token(
            &key,
            &source_id,
            "ghs_nearly_expired",
            chrono::Utc::now() + chrono::Duration::seconds(30),
        )
        .await
        .expect("cache");

    Mock::given(method("POST"))
        .and(path("/app/installations/42/access_tokens"))
        .respond_with(ResponseTemplate::new(201).set_body_json(serde_json::json!({
            "token": "ghs_fresh",
            "expires_at": (chrono::Utc::now() + chrono::Duration::hours(1)).to_rfc3339(),
        })))
        .expect(1)
        .mount(&server)
        .await;

    let token = github::installation_token(&store, &key, &source_id)
        .await
        .expect("token");
    assert_eq!(
        token, "ghs_fresh",
        "a nearly-expired token must be replaced"
    );

    // And the replacement is now cached, so the next call does not mint again.
    let again = github::installation_token(&store, &key, &source_id)
        .await
        .expect("token");
    assert_eq!(again, "ghs_fresh");
}

#[tokio::test]
async fn an_expired_cached_token_is_refreshed() {
    let server = MockServer::start().await;
    let store = Store::open_in_memory().await.expect("store");
    let key = SecretKey::generate();
    let (source_id, _) = configured_source(&store, &key, &server.uri()).await;

    store
        .cache_installation_token(
            &key,
            &source_id,
            "ghs_expired",
            chrono::Utc::now() - chrono::Duration::minutes(5),
        )
        .await
        .expect("cache");

    Mock::given(method("POST"))
        .and(path("/app/installations/42/access_tokens"))
        .respond_with(ResponseTemplate::new(201).set_body_json(serde_json::json!({
            "token": "ghs_replacement",
            "expires_at": (chrono::Utc::now() + chrono::Duration::hours(1)).to_rfc3339(),
        })))
        .expect(1)
        .mount(&server)
        .await;

    assert_eq!(
        github::installation_token(&store, &key, &source_id)
            .await
            .expect("token"),
        "ghs_replacement"
    );
}

#[tokio::test]
async fn a_source_with_no_installation_cannot_mint_a_token() {
    let store = Store::open_in_memory().await.expect("store");
    let key = SecretKey::generate();

    let source = store
        .create_pending_github_source(
            "pending",
            "",
            "https://api.github.com",
            "https://github.com",
        )
        .await
        .expect("create");

    let error = github::installation_token(&store, &key, &source.id)
        .await
        .expect_err("must fail");
    assert!(error.to_string().contains("not installed"), "got: {error}");
}

#[tokio::test]
async fn an_installation_is_verified_to_belong_to_this_app() {
    // Without this check, anyone who can reach the callback could bind an
    // arbitrary installation id to a source.
    let server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/app/installations/42"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": 42,
            "app_id": 123456,
            "account": { "login": "acme-corp" },
        })))
        .mount(&server)
        .await;

    let jwt = github::sign_app_jwt(TEST_KEY, 123456).expect("sign");
    let client = GithubClient::new(&server.uri()).expect("client");

    let account = client
        .verify_installation(&jwt, 42, 123456)
        .await
        .expect("verify");
    assert_eq!(account, "acme-corp");
}

#[tokio::test]
async fn an_installation_belonging_to_another_app_is_refused() {
    let server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/app/installations/99"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": 99,
            // A different App's installation.
            "app_id": 999999,
            "account": { "login": "someone-else" },
        })))
        .mount(&server)
        .await;

    let jwt = github::sign_app_jwt(TEST_KEY, 123456).expect("sign");
    let client = GithubClient::new(&server.uri()).expect("client");

    let error = client
        .verify_installation(&jwt, 99, 123456)
        .await
        .expect_err("must be refused");
    assert!(error.to_string().contains("belongs to app"), "got: {error}");
}

#[tokio::test]
async fn repositories_are_listed_across_pages_and_sorted() {
    let server = MockServer::start().await;

    // Two pages, as GitHub delivers them.
    Mock::given(method("GET"))
        .and(path("/installation/repositories"))
        .and(query_param("page", "1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "total_count": 101,
            "repositories": (0..100)
                .map(|i| serde_json::json!({
                    "full_name": format!("acme/repo-{i:03}"),
                    "default_branch": "main",
                    "private": true,
                    "clone_url": format!("https://github.com/acme/repo-{i:03}.git"),
                }))
                .collect::<Vec<_>>(),
        })))
        .mount(&server)
        .await;

    Mock::given(method("GET"))
        .and(path("/installation/repositories"))
        .and(query_param("page", "2"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "total_count": 101,
            "repositories": [{
                "full_name": "acme/AAA-last-page",
                "default_branch": "trunk",
                "private": false,
                "clone_url": "https://github.com/acme/AAA-last-page.git",
            }],
        })))
        .mount(&server)
        .await;

    let client = GithubClient::new(&server.uri()).expect("client");
    let repositories = client.list_repositories("ghs_token").await.expect("list");

    assert_eq!(repositories.len(), 101, "both pages should be followed");
    // Sorted case-insensitively, so the second page's entry comes first.
    assert_eq!(repositories[0].full_name, "acme/AAA-last-page");
    assert_eq!(repositories[0].default_branch, "trunk");
    assert!(!repositories[0].private);
}

#[tokio::test]
async fn a_single_short_page_of_repositories_ends_the_pagination() {
    let server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/installation/repositories"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "total_count": 2,
            "repositories": [
                { "full_name": "acme/one", "default_branch": "main", "private": true, "clone_url": "" },
                { "full_name": "acme/two", "default_branch": "main", "private": true, "clone_url": "" },
            ],
        })))
        // One request only: a short page means there is no second one.
        .expect(1)
        .mount(&server)
        .await;

    let client = GithubClient::new(&server.uri()).expect("client");
    assert_eq!(
        client
            .list_repositories("ghs_token")
            .await
            .expect("list")
            .len(),
        2
    );
}

#[tokio::test]
async fn branches_are_listed_with_the_default_ones_first() {
    let server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/repos/acme/bot/branches"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([
            { "name": "zebra" },
            { "name": "master" },
            { "name": "feature/x" },
            { "name": "main" },
        ])))
        .mount(&server)
        .await;

    let client = GithubClient::new(&server.uri()).expect("client");
    let branches = client
        .list_branches("ghs_token", "acme/bot")
        .await
        .expect("list");

    // The ones people actually want are first.
    assert_eq!(branches, vec!["main", "master", "feature/x", "zebra"]);
}

#[tokio::test]
async fn a_repository_name_that_could_traverse_a_path_never_reaches_github() {
    let server = MockServer::start().await;
    // Nothing is mounted: any request at all is a failure.

    let client = GithubClient::new(&server.uri()).expect("client");
    for hostile in ["../../etc/passwd", "acme/../../secrets", "noslash", ""] {
        assert!(
            client.list_branches("ghs_token", hostile).await.is_err(),
            "{hostile:?} should be refused"
        );
    }

    assert!(
        server
            .received_requests()
            .await
            .expect("requests")
            .is_empty(),
        "a malformed repository must be rejected before any request is sent"
    );
}

#[tokio::test]
async fn a_deploys_outcome_is_written_back_to_the_commit() {
    let server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/repos/acme/bot/statuses/abc123def456"))
        .and(header("accept", "application/vnd.github+json"))
        .and(body_json(serde_json::json!({
            "state": "success",
            "context": "nudo/deploy",
            "description": "bot deployed",
            "target_url": "https://nudo.example.com/deployments/dep_1",
        })))
        .respond_with(ResponseTemplate::new(201).set_body_json(serde_json::json!({ "id": 1 })))
        .expect(1)
        .mount(&server)
        .await;

    let client = GithubClient::new(&server.uri()).expect("client");
    client
        .set_commit_status(
            "ghs_token",
            "acme/bot",
            "abc123def456",
            CommitStatus::Success,
            "https://nudo.example.com/deployments/dep_1",
            "bot deployed",
        )
        .await
        .expect("write the status");
}

#[tokio::test]
async fn a_status_description_longer_than_github_allows_is_truncated() {
    // GitHub caps this at 140 characters and would reject or silently cut a
    // longer one, so it is truncated here where the behaviour is visible.
    let server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/repos/acme/bot/statuses/sha"))
        .respond_with(ResponseTemplate::new(201).set_body_json(serde_json::json!({ "id": 1 })))
        .mount(&server)
        .await;

    let client = GithubClient::new(&server.uri()).expect("client");
    client
        .set_commit_status(
            "ghs_token",
            "acme/bot",
            "sha",
            CommitStatus::Failure,
            "",
            &"x".repeat(500),
        )
        .await
        .expect("write the status");

    let requests = server.received_requests().await.expect("requests");
    let body: serde_json::Value = serde_json::from_slice(&requests[0].body).expect("a JSON body");
    let description = body["description"].as_str().expect("a description");
    assert!(description.chars().count() <= 140, "not truncated");
    // With no target url, the field is omitted rather than sent empty.
    assert!(body.get("target_url").is_none());
}

#[tokio::test]
async fn the_full_setup_flow_works_end_to_end_against_a_mocked_github() {
    // Manifest exchange, credentials sealed into the store, an installation
    // recorded, and a token minted — the whole path an operator walks.
    let server = MockServer::start().await;
    let store = Store::open_in_memory().await.expect("store");
    let key = SecretKey::generate();

    Mock::given(method("POST"))
        .and(path("/app-manifests/the-code/conversions"))
        .respond_with(ResponseTemplate::new(201).set_body_json(serde_json::json!({
            "id": 777,
            "slug": "nudo-prod",
            "client_id": "Iv1.x",
            "client_secret": "cs",
            "pem": TEST_KEY,
            "webhook_secret": "ws",
            "html_url": "https://github.com/apps/nudo-prod",
        })))
        .mount(&server)
        .await;

    Mock::given(method("GET"))
        .and(path("/app/installations/500"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": 500,
            "app_id": 777,
            "account": { "login": "acme-corp" },
        })))
        .mount(&server)
        .await;

    Mock::given(method("POST"))
        .and(path("/app/installations/500/access_tokens"))
        .respond_with(ResponseTemplate::new(201).set_body_json(serde_json::json!({
            "token": "ghs_final",
            "expires_at": (chrono::Utc::now() + chrono::Duration::hours(1)).to_rfc3339(),
        })))
        .mount(&server)
        .await;

    // ---- the flow ----
    let pending = store
        .create_pending_github_source("nudo-prod", "", &server.uri(), "https://github.com")
        .await
        .expect("pending source");

    let state = store
        .create_setup_state(&pending.id, "manifest")
        .await
        .expect("state");

    // GitHub redirects back; the state is consumed exactly once.
    let consumed = store
        .consume_setup_state(&state, "manifest")
        .await
        .expect("consume")
        .expect("the state should be present");
    assert_eq!(consumed.source_id, pending.id);

    let client = GithubClient::new(&server.uri()).expect("client");
    let credentials = client
        .exchange_manifest_code("the-code")
        .await
        .expect("exchange");

    let configured = store
        .attach_github_credentials(&key, &pending.id, &credentials)
        .await
        .expect("attach");
    assert_eq!(configured.app_id, 777);

    // Nothing secret is on the message the API returns.
    let rendered = format!("{configured:?}");
    assert!(!rendered.contains("PRIVATE KEY"));
    assert!(!rendered.contains("ws"));

    // The installation is verified before being recorded.
    let jwt = github::sign_app_jwt(&credentials.private_key, credentials.app_id).expect("sign");
    let account = client
        .verify_installation(&jwt, 500, 777)
        .await
        .expect("verify");
    store
        .set_installation(&pending.id, 500, &account)
        .await
        .expect("record");

    // And a token can now be minted and cached.
    let token = github::installation_token(&store, &key, &pending.id)
        .await
        .expect("token");
    assert_eq!(token, "ghs_final");

    // A replayed callback finds no state.
    assert!(
        store
            .consume_setup_state(&state, "manifest")
            .await
            .expect("consume")
            .is_none()
    );

    // And the credentials cannot be rebound.
    assert!(
        store
            .attach_github_credentials(&key, &pending.id, &credentials)
            .await
            .is_err()
    );
}
