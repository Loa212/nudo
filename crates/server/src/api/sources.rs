//! The `Sources` service: the GitHub App manifest flow and repo/branch listing.

use nudo_proto::sources_server::Sources;
use nudo_proto::*;
use tonic::{Request, Response, Status};

use super::{Context, internal};
use crate::github;

pub struct SourcesService {
    context: Context,
}

impl SourcesService {
    pub fn new(context: Context) -> Self {
        Self { context }
    }
}

#[tonic::async_trait]
impl Sources for SourcesService {
    async fn create_github_app_manifest(
        &self,
        request: Request<CreateGithubAppManifestRequest>,
    ) -> Result<Response<CreateGithubAppManifestResponse>, Status> {
        let request = request.into_inner();

        self.context
            .authorize(
                request.mutation.as_ref(),
                "Sources.CreateGithubAppManifest",
                "",
                None,
                format!("started a GitHub App manifest flow for {}", request.name),
            )
            .await?;

        let name = request.name.trim();
        if name.is_empty() {
            return Err(Status::invalid_argument("the App needs a name"));
        }

        // GitHub has to be able to reach the webhook and callback URLs, so a
        // localhost base URL would produce an App that silently never delivers.
        let base_url = if request.base_url.trim().is_empty() {
            self.context.config.base_url.clone()
        } else {
            request.base_url.trim().to_string()
        };
        if !base_url.starts_with("http://") && !base_url.starts_with("https://") {
            return Err(Status::invalid_argument(
                "base_url must be an absolute http(s) URL that GitHub can reach",
            ));
        }

        // GitHub's HTML host is where the manifest is POSTed; the API host is
        // derived from it so Enterprise installs work without extra config.
        let html_url = "https://github.com";
        let api_url = github::api_url_from_html_url(html_url);

        // The row exists before GitHub knows about the App, because the callback
        // needs something to attach the returned credentials to.
        let source = self
            .context
            .store
            .create_pending_github_source(name, &request.organization, &api_url, html_url)
            .await
            .map_err(super::invalid)?;

        // Only sha256(state) is stored, and it is consumed exactly once.
        let state = self
            .context
            .store
            .create_setup_state(&source.id, "manifest")
            .await
            .map_err(internal)?;

        let manifest = github::build_manifest(name, &base_url);
        let manifest_json = serde_json::to_string(&manifest).map_err(internal)?;

        Ok(Response::new(CreateGithubAppManifestResponse {
            source_id: source.id,
            manifest_json,
            post_url: github::manifest_post_url(html_url, &request.organization, &state),
            state,
        }))
    }

    async fn exchange_github_manifest_code(
        &self,
        request: Request<ExchangeGithubManifestCodeRequest>,
    ) -> Result<Response<Source>, Status> {
        let request = request.into_inner();

        if request.code.trim().is_empty() {
            return Err(Status::invalid_argument("GitHub returned no code"));
        }

        // Consuming the state both authenticates the callback and prevents a
        // replay: the row is deleted as part of the read.
        let setup = self
            .context
            .store
            .consume_setup_state(&request.state, "manifest")
            .await
            .map_err(internal)?
            .ok_or_else(|| {
                Status::permission_denied(
                    "that setup state is unknown, already used, or expired — \
                     start the GitHub App flow again",
                )
            })?;

        let urls = self
            .context
            .store
            .source_urls(&setup.source_id)
            .await
            .map_err(internal)?
            .ok_or_else(|| Status::not_found("the pending source no longer exists"))?;

        let client = github::GithubClient::new(&urls.api_url).map_err(internal)?;
        let credentials = client
            .exchange_manifest_code(request.code.trim())
            .await
            .map_err(|error| Status::failed_precondition(format!("{error:#}")))?;

        // Refuses to overwrite existing credentials, so a replayed or forged
        // callback cannot rebind a configured source.
        let source = self
            .context
            .store
            .attach_github_credentials(&self.context.secret_key, &setup.source_id, &credentials)
            .await
            .map_err(|error| Status::failed_precondition(format!("{error:#}")))?;

        self.context
            .store
            .audit(crate::store::NewAuditEntry {
                actor: Actor::system("github manifest callback"),
                action: "Sources.ExchangeGithubManifestCode".to_string(),
                subject_id: source.id.clone(),
                dry_run: false,
                summary: format!(
                    "configured GitHub App {} (app id {})",
                    source.app_slug, source.app_id
                ),
            })
            .await;

        Ok(Response::new(source))
    }

    async fn list(
        &self,
        _request: Request<ListSourcesRequest>,
    ) -> Result<Response<ListSourcesResponse>, Status> {
        let sources = self.context.store.list_sources().await.map_err(internal)?;
        Ok(Response::new(ListSourcesResponse { sources }))
    }

    async fn list_repositories(
        &self,
        request: Request<ListRepositoriesRequest>,
    ) -> Result<Response<ListRepositoriesResponse>, Status> {
        let request = request.into_inner();
        let urls = self
            .context
            .store
            .source_urls(&request.source_id)
            .await
            .map_err(internal)?
            .ok_or_else(|| Status::not_found(format!("no such source: {}", request.source_id)))?;

        let token = github::installation_token(
            &self.context.store,
            &self.context.secret_key,
            &request.source_id,
        )
        .await
        .map_err(|error| Status::failed_precondition(format!("{error:#}")))?;

        let client = github::GithubClient::new(&urls.api_url).map_err(internal)?;
        let mut repositories = client
            .list_repositories(&token)
            .await
            .map_err(|error| Status::unavailable(format!("{error:#}")))?;

        // Filtering here rather than via GitHub's search API keeps one code path
        // and avoids a second set of rate limits; the list is already local.
        let query = request.query.trim().to_lowercase();
        if !query.is_empty() {
            repositories.retain(|repo| repo.full_name.to_lowercase().contains(&query));
        }

        Ok(Response::new(ListRepositoriesResponse { repositories }))
    }

    async fn list_branches(
        &self,
        request: Request<ListBranchesRequest>,
    ) -> Result<Response<ListBranchesResponse>, Status> {
        let request = request.into_inner();

        // Validated before it reaches a URL path.
        github::split_repo(&request.repo).map_err(super::invalid)?;

        let urls = self
            .context
            .store
            .source_urls(&request.source_id)
            .await
            .map_err(internal)?
            .ok_or_else(|| Status::not_found(format!("no such source: {}", request.source_id)))?;

        let token = github::installation_token(
            &self.context.store,
            &self.context.secret_key,
            &request.source_id,
        )
        .await
        .map_err(|error| Status::failed_precondition(format!("{error:#}")))?;

        let client = github::GithubClient::new(&urls.api_url).map_err(internal)?;
        let branches = client
            .list_branches(&token, &request.repo)
            .await
            .map_err(|error| Status::unavailable(format!("{error:#}")))?;

        Ok(Response::new(ListBranchesResponse { branches }))
    }

    async fn delete(&self, request: Request<DeleteSourceRequest>) -> Result<Response<()>, Status> {
        let request = request.into_inner();
        let source = self
            .context
            .store
            .get_source(&request.id)
            .await
            .map_err(internal)?
            .ok_or_else(|| Status::not_found(format!("no such source: {}", request.id)))?;

        // A service bound to this source would lose its ability to build, so say
        // how many before doing it.
        let services = self
            .context
            .store
            .list_services("", 500, 0)
            .await
            .map_err(internal)?;
        let dependent = services
            .iter()
            .filter(
                |service| match service.artifact.as_ref().and_then(|a| a.kind.as_ref()) {
                    Some(artifact_source::Kind::Git(git)) => git.source_id == request.id,
                    _ => false,
                },
            )
            .count();

        let authorized = self
            .context
            .authorize(
                request.mutation.as_ref(),
                "Sources.Delete",
                &request.id,
                None,
                format!(
                    "deleted source {} ({dependent} service(s) referenced it)",
                    source.name
                ),
            )
            .await?;

        if authorized.dry_run {
            return Ok(Response::new(()));
        }

        if dependent > 0 {
            return Err(Status::failed_precondition(format!(
                "{dependent} service(s) still build from this source; \
                 point them elsewhere first"
            )));
        }

        self.context
            .store
            .delete_source(&request.id)
            .await
            .map_err(super::invalid)?;
        Ok(Response::new(()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::Bus;
    use crate::store::{Store, TargetInput};
    use std::sync::Arc;

    async fn service() -> SourcesService {
        SourcesService::new(Context::new(
            Store::open_in_memory().await.expect("store"),
            Bus::default(),
            crate::crypto::SecretKey::generate(),
            Arc::new(crate::Config {
                base_url: "https://nudo.example.com".to_string(),
                ..crate::Config::default()
            }),
        ))
    }

    fn manifest(name: &str, organization: &str) -> CreateGithubAppManifestRequest {
        CreateGithubAppManifestRequest {
            mutation: Some(Mutation::by(Actor::human("usr_1", "alice"))),
            name: name.to_string(),
            organization: organization.to_string(),
            base_url: "https://nudo.example.com".to_string(),
        }
    }

    #[tokio::test]
    async fn the_manifest_flow_returns_a_post_url_a_state_and_a_pending_source() {
        let service = service().await;
        let response = service
            .create_github_app_manifest(Request::new(manifest("nudo-prod", "")))
            .await
            .expect("manifest")
            .into_inner();

        assert!(response.source_id.starts_with("src_"));
        assert!(!response.state.is_empty());
        assert!(
            response
                .post_url
                .starts_with("https://github.com/settings/apps/new?state=")
        );

        // The manifest is valid JSON pointing back at us.
        let parsed: serde_json::Value =
            serde_json::from_str(&response.manifest_json).expect("json");
        assert_eq!(parsed["name"], "nudo-prod");
        assert_eq!(
            parsed["hook_attributes"]["url"],
            "https://nudo.example.com/webhooks/github"
        );

        // The pending source is listed but not yet installed.
        let listed = service
            .list(Request::new(ListSourcesRequest {}))
            .await
            .expect("list")
            .into_inner();
        assert_eq!(listed.sources.len(), 1);
        assert!(!listed.sources[0].installed);
    }

    #[tokio::test]
    async fn an_organization_manifest_posts_to_the_organization_path() {
        let service = service().await;
        let response = service
            .create_github_app_manifest(Request::new(manifest("nudo", "acme-corp")))
            .await
            .expect("manifest")
            .into_inner();
        assert!(
            response
                .post_url
                .starts_with("https://github.com/organizations/acme-corp/settings/apps/new"),
            "got: {}",
            response.post_url
        );
    }

    #[tokio::test]
    async fn a_manifest_needs_a_name() {
        let service = service().await;
        let status = service
            .create_github_app_manifest(Request::new(manifest("  ", "")))
            .await
            .expect_err("err");
        assert_eq!(status.code(), tonic::Code::InvalidArgument);
    }

    #[tokio::test]
    async fn a_base_url_github_could_not_reach_is_refused() {
        // An App whose webhook URL is not absolute silently never delivers.
        let service = service().await;
        let status = service
            .create_github_app_manifest(Request::new(CreateGithubAppManifestRequest {
                base_url: "nudo.example.com".to_string(),
                ..manifest("nudo", "")
            }))
            .await
            .expect_err("err");
        assert_eq!(status.code(), tonic::Code::InvalidArgument);
        assert!(status.message().contains("absolute"));
    }

    #[tokio::test]
    async fn an_omitted_base_url_falls_back_to_the_configured_one() {
        let service = service().await;
        let response = service
            .create_github_app_manifest(Request::new(CreateGithubAppManifestRequest {
                base_url: String::new(),
                ..manifest("nudo", "")
            }))
            .await
            .expect("manifest")
            .into_inner();

        let parsed: serde_json::Value =
            serde_json::from_str(&response.manifest_json).expect("json");
        assert_eq!(parsed["url"], "https://nudo.example.com");
    }

    #[tokio::test]
    async fn the_callback_refuses_an_unknown_state() {
        // Without this, anyone who can reach the callback could bind an App.
        let service = service().await;
        let status = service
            .exchange_github_manifest_code(Request::new(ExchangeGithubManifestCodeRequest {
                state: "guessed".to_string(),
                code: "abc123".to_string(),
            }))
            .await
            .expect_err("must be refused");
        assert_eq!(status.code(), tonic::Code::PermissionDenied);
    }

    #[tokio::test]
    async fn the_callback_refuses_a_replayed_state() {
        let service = service().await;
        let response = service
            .create_github_app_manifest(Request::new(manifest("nudo", "")))
            .await
            .expect("manifest")
            .into_inner();

        // Burn the state as the first (network-failing) attempt would.
        service
            .context
            .store
            .consume_setup_state(&response.state, "manifest")
            .await
            .expect("consume");

        let status = service
            .exchange_github_manifest_code(Request::new(ExchangeGithubManifestCodeRequest {
                state: response.state,
                code: "abc123".to_string(),
            }))
            .await
            .expect_err("must be refused");
        assert_eq!(status.code(), tonic::Code::PermissionDenied);
    }

    #[tokio::test]
    async fn the_callback_needs_a_code() {
        let service = service().await;
        let response = service
            .create_github_app_manifest(Request::new(manifest("nudo", "")))
            .await
            .expect("manifest")
            .into_inner();

        let status = service
            .exchange_github_manifest_code(Request::new(ExchangeGithubManifestCodeRequest {
                state: response.state,
                code: "   ".to_string(),
            }))
            .await
            .expect_err("err");
        assert_eq!(status.code(), tonic::Code::InvalidArgument);
    }

    #[tokio::test]
    async fn listing_branches_validates_the_repository_shape_before_calling_github() {
        let service = service().await;
        let status = service
            .list_branches(Request::new(ListBranchesRequest {
                source_id: "src_whatever".to_string(),
                repo: "../../etc/passwd".to_string(),
            }))
            .await
            .expect_err("must be refused");
        assert_eq!(status.code(), tonic::Code::InvalidArgument);
    }

    #[tokio::test]
    async fn listing_repositories_for_a_missing_source_is_not_found() {
        let service = service().await;
        let status = service
            .list_repositories(Request::new(ListRepositoriesRequest {
                source_id: "src_nope".to_string(),
                query: String::new(),
            }))
            .await
            .expect_err("err");
        assert_eq!(status.code(), tonic::Code::NotFound);
    }

    #[tokio::test]
    async fn listing_repositories_for_an_uninstalled_source_explains_the_precondition() {
        let service = service().await;
        let response = service
            .create_github_app_manifest(Request::new(manifest("nudo", "")))
            .await
            .expect("manifest")
            .into_inner();

        let status = service
            .list_repositories(Request::new(ListRepositoriesRequest {
                source_id: response.source_id,
                query: String::new(),
            }))
            .await
            .expect_err("err");
        assert_eq!(status.code(), tonic::Code::FailedPrecondition);
        assert!(status.message().contains("not installed"));
    }

    #[tokio::test]
    async fn a_source_a_service_still_builds_from_cannot_be_deleted() {
        let service = service().await;
        let response = service
            .create_github_app_manifest(Request::new(manifest("nudo", "")))
            .await
            .expect("manifest")
            .into_inner();

        let target = service
            .context
            .store
            .create_target(&TargetInput {
                name: "box".to_string(),
                host: "10.0.0.1".to_string(),
                ..Default::default()
            })
            .await
            .expect("target");
        service
            .context
            .store
            .create_service(&Service {
                target_id: target.id,
                name: "bot".to_string(),
                artifact: Some(ArtifactSource {
                    kind: Some(artifact_source::Kind::Git(GitSource {
                        source_id: response.source_id.clone(),
                        repo: "owner/bot".to_string(),
                        ..Default::default()
                    })),
                }),
                ..Default::default()
            })
            .await
            .expect("service");

        let status = service
            .delete(Request::new(DeleteSourceRequest {
                mutation: Some(Mutation::by(Actor::human("u", "alice"))),
                id: response.source_id.clone(),
            }))
            .await
            .expect_err("must be refused");
        assert_eq!(status.code(), tonic::Code::FailedPrecondition);
        assert!(status.message().contains("still build"));

        // Still there.
        let listed = service
            .list(Request::new(ListSourcesRequest {}))
            .await
            .expect("list")
            .into_inner();
        assert_eq!(listed.sources.len(), 1);
    }

    #[tokio::test]
    async fn an_unreferenced_source_can_be_deleted() {
        let service = service().await;
        let response = service
            .create_github_app_manifest(Request::new(manifest("nudo", "")))
            .await
            .expect("manifest")
            .into_inner();

        service
            .delete(Request::new(DeleteSourceRequest {
                mutation: Some(Mutation::by(Actor::human("u", "alice"))),
                id: response.source_id,
            }))
            .await
            .expect("delete");

        let listed = service
            .list(Request::new(ListSourcesRequest {}))
            .await
            .expect("list")
            .into_inner();
        assert!(listed.sources.is_empty());
    }

    #[tokio::test]
    async fn deleting_a_missing_source_is_not_found() {
        let service = service().await;
        let status = service
            .delete(Request::new(DeleteSourceRequest {
                mutation: Some(Mutation::by(Actor::human("u", "alice"))),
                id: "src_nope".to_string(),
            }))
            .await
            .expect_err("err");
        assert_eq!(status.code(), tonic::Code::NotFound);
    }
}
