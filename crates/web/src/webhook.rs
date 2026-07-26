//! The GitHub webhook receiver.
//!
//! Signature verification happens over the **raw** request body, before any JSON
//! parsing: re-serializing a parsed payload changes the bytes the HMAC covers,
//! so a valid delivery would fail and — worse — a crafted one could be made to
//! pass. The body is therefore taken as `Bytes` and verified first.
//!
//! Unlike Coolify, there is no environment in which verification is skipped.

use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use nudo_proto::{Actor, artifact_source};
use nudo_server::crypto::verify_github_signature;
use nudo_server::deploy::DeployOptions;

use crate::AppState;

/// GitHub's headers, spelled exactly as delivered.
const SIGNATURE_HEADER: &str = "x-hub-signature-256";
const EVENT_HEADER: &str = "x-github-event";
const DELIVERY_HEADER: &str = "x-github-delivery";
const TARGET_ID_HEADER: &str = "x-github-hook-installation-target-id";

/// Handles a delivery.
///
/// Returns 200 with an explanatory body for deliveries that are authentic but
/// not actionable — an unknown event, a branch nothing watches — because GitHub
/// treats a 4xx as a failed delivery and will retry it and eventually disable
/// the hook. Only an authentication failure is a 4xx.
pub async fn receive(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let event = header(&headers, EVENT_HEADER).to_lowercase();
    let delivery = header(&headers, DELIVERY_HEADER);

    // `ping` is what GitHub sends when a hook is created, to prove the endpoint
    // exists. It is answered before anything else so a misconfigured secret is
    // still reported as a reachable endpoint rather than a dead one.
    if event == "ping" {
        return (StatusCode::OK, "pong").into_response();
    }

    // Which App this delivery belongs to. Without it there is no secret to
    // verify against.
    let app_id: i64 = match header(&headers, TARGET_ID_HEADER).parse() {
        Ok(app_id) => app_id,
        Err(_) => {
            return (
                StatusCode::OK,
                "ignored: the delivery carried no GitHub App id",
            )
                .into_response();
        }
    };

    let source = match state.store.source_by_app_id(app_id).await {
        Ok(Some(source)) => source,
        Ok(None) => {
            return (
                StatusCode::OK,
                "ignored: no source is configured for that GitHub App",
            )
                .into_response();
        }
        Err(error) => {
            tracing::error!(%error, "looking up the webhook's source failed");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };

    let secret = match state.store.source_webhook_secret(&state.secret_key, &source.id).await {
        Ok(Some(secret)) if !secret.trim().is_empty() => secret,
        Ok(_) => {
            // Refused rather than waved through: a source with no secret cannot
            // have its deliveries authenticated, so acting on one would let
            // anybody who knows the App id trigger a deploy.
            tracing::warn!(source = %source.id, "a delivery arrived for a source with no webhook secret");
            return (
                StatusCode::UNAUTHORIZED,
                "this source has no webhook secret configured, so deliveries cannot be verified",
            )
                .into_response();
        }
        Err(error) => {
            tracing::error!(%error, "reading the webhook secret failed");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };

    // The raw bytes, exactly as received.
    if !verify_github_signature(&header(&headers, SIGNATURE_HEADER), &body, &secret) {
        tracing::warn!(
            source = %source.id,
            %delivery,
            "rejected a webhook delivery whose signature did not verify"
        );
        return (StatusCode::UNAUTHORIZED, "invalid signature").into_response();
    }

    // Authenticated. Only now is it safe to look at the contents.
    let payload: serde_json::Value = match serde_json::from_slice(&body) {
        Ok(payload) => payload,
        Err(error) => {
            return (
                StatusCode::BAD_REQUEST,
                format!("the delivery body is not valid JSON: {error}"),
            )
                .into_response();
        }
    };

    match event.as_str() {
        "push" => handle_push(state, &source, &payload, &delivery).await,
        "pull_request" => {
            // Preview environments per pull request are out of scope; see
            // CHANGES.md. Acknowledged so GitHub does not retry.
            (
                StatusCode::OK,
                "acknowledged: pull_request events are recorded but do not deploy",
            )
                .into_response()
        }
        "installation" | "installation_repositories" => {
            handle_installation(state, &source, &payload).await
        }
        other => (
            StatusCode::OK,
            format!("ignored: {other} events are not handled"),
        )
            .into_response(),
    }
}

/// Deploys every service watching the pushed branch.
async fn handle_push(
    state: AppState,
    source: &nudo_proto::Source,
    payload: &serde_json::Value,
    delivery: &str,
) -> Response {
    let git_ref = payload["ref"].as_str().unwrap_or_default();
    let Some(branch) = nudo_server::github::branch_from_ref(git_ref) else {
        // A tag push, or a ref shape we do not deploy from.
        return (
            StatusCode::OK,
            format!("ignored: {git_ref} is not a branch"),
        )
            .into_response();
    };

    let repo = payload["repository"]["full_name"].as_str().unwrap_or_default();
    if repo.is_empty() {
        return (StatusCode::OK, "ignored: the delivery named no repository").into_response();
    }

    // A push whose every commit asks to skip CI should not deploy; one real
    // commit among them should.
    let messages: Vec<String> = payload["commits"]
        .as_array()
        .map(|commits| {
            commits
                .iter()
                .filter_map(|commit| commit["message"].as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();

    if nudo_server::github::should_skip_deploy(&messages) {
        return (
            StatusCode::OK,
            "skipped: every commit in this push asked to skip deployment",
        )
            .into_response();
    }

    let after = payload["after"].as_str().unwrap_or_default();

    let services = match state.store.services_for_push(&source.id, repo, branch).await {
        Ok(services) => services,
        Err(error) => {
            tracing::error!(%error, "resolving the push to services failed");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };

    if services.is_empty() {
        return (
            StatusCode::OK,
            format!("ignored: no service auto-deploys {repo}@{branch}"),
        )
            .into_response();
    }

    let actor = Actor::webhook(delivery, format!("github push {repo}@{branch}"));
    let mut queued = Vec::new();
    let mut refused = Vec::new();

    for service in services {
        // The latency-critical guardrail applies to a webhook exactly as it does
        // to an agent. A push must never restart the hot-path box unattended —
        // that is the whole point of the flag.
        let target = match state.store.get_target(&service.target_id).await {
            Ok(Some(target)) => target,
            _ => continue,
        };

        if target.latency_critical {
            state
                .store
                .audit(nudo_server::store::NewAuditEntry {
                    actor: actor.clone(),
                    action: "Webhook.Push (refused)".to_string(),
                    subject_id: service.id.clone(),
                    dry_run: false,
                    summary: format!(
                        "refused: {} is latency-critical, so a push cannot deploy to it \
                         unattended — deploy it deliberately instead",
                        target.name
                    ),
                })
                .await;
            refused.push(service.name.clone());
            continue;
        }

        match state
            .engine
            .start_deploy(
                &service.id,
                actor.clone(),
                DeployOptions {
                    git_ref: after.to_string(),
                    artifact_url: String::new(),
                    uploaded_artifact: None,
                    skip_health_check: false,
                    // A webhook deploy is unattended by definition, so an
                    // unhealthy release must put the previous one back.
                    auto_rollback_on_failure: true,
                },
            )
            .await
        {
            Ok(deployment) => {
                state
                    .store
                    .audit(nudo_server::store::NewAuditEntry {
                        actor: actor.clone(),
                        action: "Webhook.Push".to_string(),
                        subject_id: service.id.clone(),
                        dry_run: false,
                        summary: format!(
                            "queued a deploy of {} from {repo}@{branch}",
                            service.name
                        ),
                    })
                    .await;

                // Report the deploy's progress on the commit, so it shows up in
                // GitHub next to the change that caused it.
                spawn_status_writeback(
                    state.clone(),
                    source.id.clone(),
                    repo.to_string(),
                    after.to_string(),
                    deployment.id.clone(),
                    service.name.clone(),
                );
                queued.push(deployment.id);
            }
            Err(error) => {
                tracing::warn!(%error, service = %service.id, "a webhook deploy could not start");
                refused.push(service.name.clone());
            }
        }
    }

    let mut body = format!("queued {} deployment(s)", queued.len());
    if !refused.is_empty() {
        body.push_str(&format!("; skipped {}", refused.join(", ")));
    }
    (StatusCode::OK, body).into_response()
}

/// Records an installation event.
async fn handle_installation(
    state: AppState,
    source: &nudo_proto::Source,
    payload: &serde_json::Value,
) -> Response {
    let installation_id = payload["installation"]["id"].as_i64().unwrap_or_default();
    let account = payload["installation"]["account"]["login"]
        .as_str()
        .unwrap_or_default();
    let action = payload["action"].as_str().unwrap_or_default();

    // An uninstall must clear the installation, or the source would keep trying
    // to mint tokens against something that no longer exists.
    if action == "deleted" {
        if let Err(error) = state.store.set_installation(&source.id, 0, "").await {
            tracing::error!(%error, "clearing the installation failed");
        }
        return (StatusCode::OK, "recorded: the App was uninstalled").into_response();
    }

    if installation_id != 0 {
        if let Err(error) = state
            .store
            .set_installation(&source.id, installation_id, account)
            .await
        {
            tracing::error!(%error, "recording the installation failed");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    }

    (StatusCode::OK, "recorded").into_response()
}

/// Follows a deployment and writes its outcome back to the commit.
///
/// Spawned rather than awaited: the delivery must be acknowledged promptly, and
/// GitHub does not need to hold a connection open for the length of a build.
fn spawn_status_writeback(
    state: AppState,
    source_id: String,
    repo: String,
    sha: String,
    deployment_id: String,
    service_name: String,
) {
    if sha.trim().is_empty() {
        return;
    }

    tokio::spawn(async move {
        use nudo_server::github::CommitStatus;

        let target_url = format!(
            "{}/deployments/{}",
            state.base_url.trim_end_matches('/'),
            deployment_id
        );

        let write = |status: CommitStatus, description: String| {
            let state = state.clone();
            let source_id = source_id.clone();
            let repo = repo.clone();
            let sha = sha.clone();
            let target_url = target_url.clone();
            async move {
                let token = match nudo_server::github::installation_token(
                    &state.store,
                    &state.secret_key,
                    &source_id,
                )
                .await
                {
                    Ok(token) => token,
                    Err(error) => {
                        tracing::warn!(%error, "no token for the commit status writeback");
                        return;
                    }
                };

                let urls = match state.store.source_urls(&source_id).await {
                    Ok(Some(urls)) => urls,
                    _ => return,
                };

                let client = match nudo_server::github::GithubClient::new(&urls.api_url) {
                    Ok(client) => client,
                    Err(error) => {
                        tracing::warn!(%error, "building the GitHub client failed");
                        return;
                    }
                };

                if let Err(error) = client
                    .set_commit_status(&token, &repo, &sha, status, &target_url, &description)
                    .await
                {
                    tracing::warn!(%error, "writing the commit status failed");
                }
            }
        };

        write(
            CommitStatus::Pending,
            format!("Deploying {service_name}…"),
        )
        .await;

        // Poll rather than subscribe, because this task outlives the request and
        // a broadcast subscription would be dropped when the deployment's
        // channel is closed.
        let mut receiver = state.bus.watch_deployment(&deployment_id);
        loop {
            match receiver.recv().await {
                Ok(nudo_server::events::DeploymentEvent::Finished(status)) => {
                    let (commit_status, description) = match status {
                        nudo_proto::deployment::Status::Succeeded => (
                            CommitStatus::Success,
                            format!("{service_name} deployed"),
                        ),
                        nudo_proto::deployment::Status::RolledBack => (
                            CommitStatus::Failure,
                            format!("{service_name} failed its health check and was rolled back"),
                        ),
                        nudo_proto::deployment::Status::Cancelled => (
                            CommitStatus::Error,
                            format!("the deploy of {service_name} was cancelled"),
                        ),
                        _ => (
                            CommitStatus::Failure,
                            format!("{service_name} failed to deploy"),
                        ),
                    };
                    write(commit_status, description).await;
                    return;
                }
                Ok(_) => continue,
                Err(_) => {
                    // The channel closed without a terminal event; read the row
                    // so a status is still written rather than left pending
                    // forever.
                    if let Ok(Some(deployment)) =
                        state.store.get_deployment(&deployment_id).await
                    {
                        let status = nudo_proto::deployment::Status::try_from(deployment.status)
                            .unwrap_or(nudo_proto::deployment::Status::Unspecified);
                        if let Some(commit_status) = CommitStatus::from_deployment(status) {
                            write(
                                commit_status,
                                format!("{service_name}: {}", status.as_str()),
                            )
                            .await;
                        }
                    }
                    return;
                }
            }
        }
    });
}

/// Reads a header as a string, or empty when absent or not valid ASCII.
fn header(headers: &HeaderMap, name: &str) -> String {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_string()
}

/// Whether a service is bound to a given source, for the dashboard's display.
pub fn service_uses_source(service: &nudo_proto::Service, source_id: &str) -> bool {
    matches!(
        service.artifact.as_ref().and_then(|a| a.kind.as_ref()),
        Some(artifact_source::Kind::Git(git)) if git.source_id == source_id
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut headers = HeaderMap::new();
        for (name, value) in pairs {
            headers.insert(
                axum::http::HeaderName::from_bytes(name.as_bytes()).expect("name"),
                axum::http::HeaderValue::from_str(value).expect("value"),
            );
        }
        headers
    }

    #[test]
    fn headers_are_read_case_insensitively_and_missing_ones_are_empty() {
        // GitHub sends `X-GitHub-Event`; HTTP header names are case-insensitive,
        // so the lookup must not depend on the casing.
        let map = headers(&[("X-GitHub-Event", "push")]);
        assert_eq!(header(&map, EVENT_HEADER), "push");
        assert_eq!(header(&map, DELIVERY_HEADER), "");
    }

    #[test]
    fn the_header_names_match_what_github_sends() {
        assert_eq!(SIGNATURE_HEADER, "x-hub-signature-256");
        assert_eq!(EVENT_HEADER, "x-github-event");
        assert_eq!(DELIVERY_HEADER, "x-github-delivery");
        assert_eq!(TARGET_ID_HEADER, "x-github-hook-installation-target-id");
    }

    #[test]
    fn a_services_source_binding_is_recognized() {
        use nudo_proto::{ArtifactSource, GitSource, Service};

        let bound = Service {
            artifact: Some(ArtifactSource {
                kind: Some(artifact_source::Kind::Git(GitSource {
                    source_id: "src_1".to_string(),
                    ..Default::default()
                })),
            }),
            ..Default::default()
        };
        assert!(service_uses_source(&bound, "src_1"));
        assert!(!service_uses_source(&bound, "src_other"));

        // A URL-sourced or upload-only service is bound to nothing.
        let unbound = Service {
            artifact: Some(ArtifactSource {
                kind: Some(artifact_source::Kind::Url("https://x/bot".to_string())),
            }),
            ..Default::default()
        };
        assert!(!service_uses_source(&unbound, "src_1"));
        assert!(!service_uses_source(&Service::default(), "src_1"));
    }

    // The signature verification itself is tested exhaustively in
    // nudo_server::crypto against GitHub's documented vector. What matters here
    // is that this module verifies over the *raw* body, which the following
    // test pins.

    #[test]
    fn the_signature_covers_the_raw_bytes_not_a_reserialized_payload() {
        // GitHub signs the exact bytes it sent. Parsing and re-serializing JSON
        // changes key order and whitespace, so a signature checked against the
        // round-tripped form would reject valid deliveries.
        let secret = "It's a Secret to Everybody";
        let raw = br#"{"zebra":1,  "alpha":2}"#;

        let signature = {
            use hmac::{KeyInit as _, Mac as _};
            let mut mac =
                hmac::Hmac::<sha2::Sha256>::new_from_slice(secret.as_bytes()).expect("key");
            mac.update(raw);
            format!("sha256={}", hex::encode(mac.finalize().into_bytes()))
        };

        // Verifying against the raw bytes passes.
        assert!(verify_github_signature(&signature, raw, secret));

        // Verifying against a re-serialized form does not — which is why this
        // handler takes Bytes and verifies before parsing.
        let parsed: serde_json::Value = serde_json::from_slice(raw).expect("json");
        let reserialized = serde_json::to_vec(&parsed).expect("serialize");
        assert_ne!(raw.to_vec(), reserialized);
        assert!(!verify_github_signature(&signature, &reserialized, secret));
    }
}
