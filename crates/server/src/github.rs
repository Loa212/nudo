//! GitHub App integration: manifest flow, installation tokens, webhooks, and
//! commit-status writeback.
//!
//! Ported from Coolify's PHP implementation, with two deliberate departures.
//!
//! Coolify mints a fresh installation token on every operation and never caches
//! one, which costs a JWT signature and two HTTP round-trips per clone, per
//! branch listing, per page of branches. Here tokens are cached against the
//! `expires_at` GitHub returns and refreshed with a margin.
//!
//! Coolify also skips webhook signature verification entirely when the app
//! environment is `local`. There is no such bypass here: an unsigned or
//! wrongly-signed delivery is always rejected.

use anyhow::{Context, anyhow, bail};
use serde::{Deserialize, Serialize};

use crate::crypto::SecretKey;
use crate::store::{GithubAppCredentials, Store};

/// GitHub's API version header value, which pins response shapes.
const API_VERSION: &str = "2022-11-28";

/// How long an App JWT is valid. GitHub's ceiling is ten minutes; eight leaves
/// room for clock skew in either direction.
const JWT_TTL_SECONDS: i64 = 8 * 60;

/// How far to back-date `iat`, so a control plane whose clock is slightly ahead
/// of GitHub's does not have its tokens rejected as future-dated.
const JWT_BACKDATE_SECONDS: i64 = 60;

/// The manifest we POST to GitHub to create an App.
///
/// Field names and shape follow GitHub's manifest format exactly.
#[derive(Debug, Clone, Serialize)]
pub struct AppManifest {
    pub name: String,
    pub url: String,
    pub hook_attributes: HookAttributes,
    pub redirect_url: String,
    pub callback_urls: Vec<String>,
    pub setup_url: String,
    pub public: bool,
    pub request_oauth_on_install: bool,
    pub setup_on_update: bool,
    pub default_permissions: std::collections::BTreeMap<String, String>,
    pub default_events: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct HookAttributes {
    pub url: String,
    pub active: bool,
}

/// Builds the App manifest for this control plane.
///
/// Permissions are the minimum needed to do the job: read repository contents
/// so we can clone, read metadata because GitHub requires it alongside
/// contents, and write statuses so a deploy's outcome shows up in GitHub's UI.
/// Notably it does **not** request `administration`, which Coolify offers as an
/// option — nothing here needs it.
pub fn build_manifest(name: &str, base_url: &str) -> AppManifest {
    let base = base_url.trim_end_matches('/');

    let mut permissions = std::collections::BTreeMap::new();
    permissions.insert("contents".to_string(), "read".to_string());
    permissions.insert("metadata".to_string(), "read".to_string());
    permissions.insert("statuses".to_string(), "write".to_string());
    permissions.insert("pull_requests".to_string(), "read".to_string());

    AppManifest {
        name: name.trim().to_string(),
        url: base.to_string(),
        hook_attributes: HookAttributes {
            url: format!("{base}/webhooks/github"),
            active: true,
        },
        redirect_url: format!("{base}/sources/github/callback"),
        callback_urls: vec![format!("{base}/sources/github/callback")],
        setup_url: format!("{base}/sources/github/installed"),
        public: false,
        // Nothing here acts on behalf of a GitHub user, so there is no reason to
        // ask anyone to authorize an OAuth flow during install.
        request_oauth_on_install: false,
        setup_on_update: true,
        default_permissions: permissions,
        default_events: vec!["push".to_string(), "pull_request".to_string()],
    }
}

/// The URL the browser POSTs the manifest to.
///
/// Personal accounts and organizations use different paths, and GitHub
/// Enterprise Server uses a different prefix again.
pub fn manifest_post_url(html_url: &str, organization: &str, state: &str) -> String {
    let base = html_url.trim().trim_end_matches('/');
    let organization = organization.trim().trim_matches('/');
    let state = urlencoding::encode(state);

    if organization.is_empty() {
        format!("{base}/settings/apps/new?state={state}")
    } else {
        format!(
            "{base}/organizations/{}/settings/apps/new?state={state}",
            urlencoding::encode(organization)
        )
    }
}

/// The URL that installs an already-created App onto repositories.
pub fn installation_url(html_url: &str, app_slug: &str) -> String {
    let base = html_url.trim().trim_end_matches('/');
    format!(
        "{base}/apps/{}/installations/new",
        urlencoding::encode(app_slug.trim())
    )
}

/// Derives the API base URL from an HTML base URL.
///
/// github.com, GitHub Enterprise Cloud (`*.ghe.com`) and Enterprise Server each
/// place their API somewhere different.
pub fn api_url_from_html_url(html_url: &str) -> String {
    let trimmed = html_url.trim().trim_end_matches('/');
    let host = trimmed
        .split("://")
        .nth(1)
        .unwrap_or(trimmed)
        .split('/')
        .next()
        .unwrap_or_default()
        .to_lowercase();

    if host == "github.com" || host.is_empty() {
        return "https://api.github.com".to_string();
    }
    if host.ends_with(".ghe.com") && !host.starts_with("api.") {
        let scheme = trimmed.split("://").next().unwrap_or("https");
        return format!("{scheme}://api.{host}");
    }
    // Enterprise Server exposes v3 under a path rather than a subdomain.
    format!("{trimmed}/api/v3")
}

/// A minted installation token and when it expires.
#[derive(Debug, Clone)]
pub struct InstallationToken {
    pub token: String,
    pub expires_at: chrono::DateTime<chrono::Utc>,
}

/// The subset of GitHub's manifest-conversion response we need.
#[derive(Debug, Deserialize)]
struct ConversionResponse {
    id: i64,
    slug: String,
    #[serde(default)]
    client_id: String,
    #[serde(default)]
    client_secret: String,
    pem: String,
    #[serde(default)]
    webhook_secret: Option<String>,
    #[serde(default)]
    html_url: String,
}

#[derive(Debug, Deserialize)]
struct TokenResponse {
    token: String,
    expires_at: String,
}

#[derive(Debug, Deserialize)]
struct InstallationResponse {
    #[serde(default)]
    app_id: i64,
    #[serde(default)]
    account: Option<Account>,
}

#[derive(Debug, Deserialize)]
struct Account {
    #[serde(default)]
    login: String,
}

#[derive(Debug, Deserialize)]
struct RepositoriesResponse {
    #[serde(default)]
    total_count: i64,
    #[serde(default)]
    repositories: Vec<RepositoryResponse>,
}

#[derive(Debug, Deserialize)]
struct RepositoryResponse {
    full_name: String,
    #[serde(default)]
    default_branch: String,
    #[serde(default)]
    private: bool,
    #[serde(default)]
    clone_url: String,
}

#[derive(Debug, Deserialize)]
struct BranchResponse {
    name: String,
}

/// A client for one GitHub App.
pub struct GithubClient {
    http: reqwest::Client,
    api_url: String,
}

impl GithubClient {
    pub fn new(api_url: &str) -> anyhow::Result<Self> {
        Ok(Self {
            http: reqwest::Client::builder()
                // GitHub asks for a User-Agent and rejects requests without one.
                .user_agent(concat!("nudo/", env!("CARGO_PKG_VERSION")))
                .timeout(std::time::Duration::from_secs(30))
                .build()?,
            api_url: api_url.trim().trim_end_matches('/').to_string(),
        })
    }

    /// Exchanges a temporary manifest code for the App's credentials.
    ///
    /// This is the one call in the flow that cannot be retried: the code is
    /// single-use, so a failure here means starting the manifest flow again.
    pub async fn exchange_manifest_code(
        &self,
        code: &str,
    ) -> anyhow::Result<GithubAppCredentials> {
        let url = format!("{}/app-manifests/{}/conversions", self.api_url, urlencoding::encode(code));

        let response = self
            .http
            .post(&url)
            .header("Accept", "application/vnd.github+json")
            .header("X-GitHub-Api-Version", API_VERSION)
            .send()
            .await
            .context("exchanging the GitHub App manifest code")?;

        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        if !status.is_success() {
            bail!("GitHub rejected the manifest code exchange (HTTP {status}): {body}");
        }

        let parsed: ConversionResponse = serde_json::from_str(&body)
            .context("parsing GitHub's manifest conversion response")?;

        // A missing private key or webhook secret would leave a source that
        // cannot clone or cannot verify deliveries, so refuse it now rather than
        // failing mysteriously on the first push.
        if parsed.pem.trim().is_empty() {
            bail!("GitHub's response contained no private key");
        }
        let webhook_secret = parsed.webhook_secret.unwrap_or_default();
        if webhook_secret.trim().is_empty() {
            bail!(
                "GitHub's response contained no webhook secret, so deliveries \
                 could not be verified"
            );
        }

        Ok(GithubAppCredentials {
            app_id: parsed.id,
            slug: parsed.slug,
            client_id: parsed.client_id,
            client_secret: parsed.client_secret,
            private_key: parsed.pem,
            webhook_secret,
            html_url: parsed.html_url,
        })
    }

    /// Mints an installation access token.
    pub async fn create_installation_token(
        &self,
        jwt: &str,
        installation_id: i64,
    ) -> anyhow::Result<InstallationToken> {
        let url = format!(
            "{}/app/installations/{installation_id}/access_tokens",
            self.api_url
        );

        let response = self
            .http
            .post(&url)
            .bearer_auth(jwt)
            .header("Accept", "application/vnd.github+json")
            .header("X-GitHub-Api-Version", API_VERSION)
            .send()
            .await
            .context("requesting an installation access token")?;

        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        if !status.is_success() {
            bail!("GitHub refused to issue an installation token (HTTP {status}): {body}");
        }

        let parsed: TokenResponse =
            serde_json::from_str(&body).context("parsing the installation token response")?;

        // The expiry drives the cache, so an unparseable one falls back to
        // GitHub's documented hour rather than caching indefinitely.
        let expires_at = chrono::DateTime::parse_from_rfc3339(&parsed.expires_at)
            .map(|dt| dt.with_timezone(&chrono::Utc))
            .unwrap_or_else(|_| chrono::Utc::now() + chrono::Duration::hours(1));

        Ok(InstallationToken {
            token: parsed.token,
            expires_at,
        })
    }

    /// Confirms an installation belongs to this App.
    ///
    /// Without this, anyone who can reach the callback could bind an arbitrary
    /// installation id to a source.
    pub async fn verify_installation(
        &self,
        jwt: &str,
        installation_id: i64,
        expected_app_id: i64,
    ) -> anyhow::Result<String> {
        let url = format!("{}/app/installations/{installation_id}", self.api_url);

        let response = self
            .http
            .get(&url)
            .bearer_auth(jwt)
            .header("Accept", "application/vnd.github+json")
            .header("X-GitHub-Api-Version", API_VERSION)
            .send()
            .await
            .context("looking up the installation")?;

        if !response.status().is_success() {
            bail!(
                "installation {installation_id} could not be verified (HTTP {})",
                response.status()
            );
        }

        let parsed: InstallationResponse = response
            .json()
            .await
            .context("parsing the installation response")?;

        if parsed.app_id != expected_app_id {
            bail!(
                "installation {installation_id} belongs to app {}, not {expected_app_id}",
                parsed.app_id
            );
        }

        Ok(parsed.account.map(|a| a.login).unwrap_or_default())
    }

    /// Lists every repository the installation can reach, following pagination.
    pub async fn list_repositories(
        &self,
        token: &str,
    ) -> anyhow::Result<Vec<nudo_proto::Repository>> {
        // Bounded so a misbehaving or hostile endpoint cannot loop forever.
        const MAX_PAGES: u32 = 100;
        const PER_PAGE: u32 = 100;

        let mut repositories = Vec::new();
        for page in 1..=MAX_PAGES {
            let url = format!(
                "{}/installation/repositories?per_page={PER_PAGE}&page={page}",
                self.api_url
            );

            let response = self
                .http
                .get(&url)
                .bearer_auth(token)
                .header("Accept", "application/vnd.github+json")
                .header("X-GitHub-Api-Version", API_VERSION)
                .send()
                .await
                .context("listing repositories")?;

            if !response.status().is_success() {
                bail!("listing repositories failed (HTTP {})", response.status());
            }

            let parsed: RepositoriesResponse =
                response.json().await.context("parsing the repository list")?;

            let returned = parsed.repositories.len();
            repositories.extend(parsed.repositories.into_iter().map(|repo| {
                nudo_proto::Repository {
                    full_name: repo.full_name,
                    default_branch: repo.default_branch,
                    private: repo.private,
                    clone_url: repo.clone_url,
                }
            }));

            // A short page is the last one; so is having everything GitHub says
            // exists.
            if returned < PER_PAGE as usize || repositories.len() as i64 >= parsed.total_count {
                break;
            }
        }

        repositories.sort_by(|a, b| a.full_name.to_lowercase().cmp(&b.full_name.to_lowercase()));
        Ok(repositories)
    }

    /// Lists a repository's branches, following pagination.
    pub async fn list_branches(&self, token: &str, repo: &str) -> anyhow::Result<Vec<String>> {
        const MAX_PAGES: u32 = 50;
        const PER_PAGE: u32 = 100;

        let (owner, name) = split_repo(repo)?;
        let mut branches = Vec::new();

        for page in 1..=MAX_PAGES {
            let url = format!(
                "{}/repos/{}/{}/branches?per_page={PER_PAGE}&page={page}",
                self.api_url,
                urlencoding::encode(owner),
                urlencoding::encode(name)
            );

            let response = self
                .http
                .get(&url)
                .bearer_auth(token)
                .header("Accept", "application/vnd.github+json")
                .header("X-GitHub-Api-Version", API_VERSION)
                .send()
                .await
                .context("listing branches")?;

            if !response.status().is_success() {
                bail!("listing branches failed (HTTP {})", response.status());
            }

            let page_branches: Vec<BranchResponse> =
                response.json().await.context("parsing the branch list")?;

            let returned = page_branches.len();
            branches.extend(page_branches.into_iter().map(|b| b.name));

            if returned < PER_PAGE as usize {
                break;
            }
        }

        Ok(sort_branches(branches))
    }

    /// Writes a deploy's outcome back to the commit, so it is visible in
    /// GitHub's UI next to the change that caused it.
    ///
    /// Uses the Commit Statuses API. Coolify instead posts a pull-request
    /// comment, which does not surface on the commit or in branch protection.
    pub async fn set_commit_status(
        &self,
        token: &str,
        repo: &str,
        sha: &str,
        status: CommitStatus,
        target_url: &str,
        description: &str,
    ) -> anyhow::Result<()> {
        let (owner, name) = split_repo(repo)?;
        let url = format!(
            "{}/repos/{}/{}/statuses/{}",
            self.api_url,
            urlencoding::encode(owner),
            urlencoding::encode(name),
            urlencoding::encode(sha.trim())
        );

        let mut body = serde_json::json!({
            "state": status.as_str(),
            "context": "nudo/deploy",
            // GitHub truncates at 140 characters, so do it here rather than
            // having the message silently cut.
            "description": description.chars().take(140).collect::<String>(),
        });
        if !target_url.trim().is_empty() {
            body["target_url"] = serde_json::Value::String(target_url.trim().to_string());
        }

        let response = self
            .http
            .post(&url)
            .bearer_auth(token)
            .header("Accept", "application/vnd.github+json")
            .header("X-GitHub-Api-Version", API_VERSION)
            .json(&body)
            .send()
            .await
            .context("writing the commit status")?;

        if !response.status().is_success() {
            bail!("writing the commit status failed (HTTP {})", response.status());
        }
        Ok(())
    }
}

/// The states GitHub's Commit Statuses API accepts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommitStatus {
    Pending,
    Success,
    Failure,
    Error,
}

impl CommitStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Success => "success",
            Self::Failure => "failure",
            Self::Error => "error",
        }
    }

    /// Maps a deployment status onto a commit status, or `None` for states that
    /// are not worth reporting.
    pub fn from_deployment(status: nudo_proto::deployment::Status) -> Option<Self> {
        use nudo_proto::deployment::Status;
        match status {
            Status::Queued | Status::Building | Status::Uploading | Status::Activating
            | Status::HealthChecking => Some(Self::Pending),
            Status::Succeeded => Some(Self::Success),
            // A rollback means the deploy did not hold, which is a failure of
            // the change, not an infrastructure error.
            Status::Failed | Status::RolledBack => Some(Self::Failure),
            Status::Cancelled => Some(Self::Error),
            Status::Unspecified => None,
        }
    }
}

/// Signs an App JWT with the App's private key.
///
/// `iat` is back-dated and `exp` is inside GitHub's ten-minute ceiling, so
/// modest clock skew in either direction does not produce rejected tokens.
pub fn sign_app_jwt(private_key_pem: &str, app_id: i64) -> anyhow::Result<String> {
    #[derive(Serialize)]
    struct Claims {
        iat: i64,
        exp: i64,
        iss: String,
    }

    let now = chrono::Utc::now().timestamp();
    let claims = Claims {
        iat: now - JWT_BACKDATE_SECONDS,
        exp: now + JWT_TTL_SECONDS,
        iss: app_id.to_string(),
    };

    // GitHub App keys are RSA and GitHub requires RS256.
    let key = jsonwebtoken::EncodingKey::from_rsa_pem(private_key_pem.trim().as_bytes())
        .context(
            "reading the GitHub App private key — it must be the PEM GitHub issued, \
             not an OpenSSH-format key",
        )?;

    jsonwebtoken::encode(
        &jsonwebtoken::Header::new(jsonwebtoken::Algorithm::RS256),
        &claims,
        &key,
    )
    .context("signing the GitHub App JWT")
}

/// Returns a usable installation token for a source, from cache or freshly
/// minted.
pub async fn installation_token(
    store: &Store,
    key: &SecretKey,
    source_id: &str,
) -> anyhow::Result<String> {
    if let Some(cached) = store.cached_installation_token(key, source_id).await? {
        return Ok(cached);
    }

    let source = store
        .get_source(source_id)
        .await?
        .ok_or_else(|| anyhow!("no such source: {source_id}"))?;
    if source.installation_id == 0 {
        bail!(
            "source {} is not installed on any GitHub account yet",
            source.name
        );
    }

    let private_key = store
        .source_private_key(key, source_id)
        .await?
        .ok_or_else(|| anyhow!("source {} has no private key", source.name))?;

    let urls = store
        .source_urls(source_id)
        .await?
        .ok_or_else(|| anyhow!("no such source: {source_id}"))?;

    let jwt = sign_app_jwt(&private_key, source.app_id)?;
    let client = GithubClient::new(&urls.api_url)?;
    let minted = client
        .create_installation_token(&jwt, source.installation_id)
        .await?;

    store
        .cache_installation_token(key, source_id, &minted.token, minted.expires_at)
        .await?;

    Ok(minted.token)
}

/// Splits `owner/name`, rejecting anything else.
///
/// Both halves end up in a URL path and, for the git path, on a command line, so
/// the shape is checked rather than assumed.
pub fn split_repo(repo: &str) -> anyhow::Result<(&str, &str)> {
    let repo = repo.trim().trim_end_matches('/');
    let (owner, name) = repo
        .split_once('/')
        .ok_or_else(|| anyhow!("repository {repo:?} is not in owner/name form"))?;

    let name = name.strip_suffix(".git").unwrap_or(name);

    if owner.is_empty() || name.is_empty() || name.contains('/') {
        bail!("repository {repo:?} is not in owner/name form");
    }
    // GitHub's own character set for owners and repository names. Anything else
    // is either a typo or an attempt at path traversal.
    let valid = |s: &str| s.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'));
    if !valid(owner) || !valid(name) {
        bail!("repository {repo:?} contains characters GitHub does not allow");
    }
    // Would traverse out of the repository path.
    if name == "." || name == ".." || owner == "." || owner == ".." {
        bail!("repository {repo:?} is not a valid path");
    }

    Ok((owner, name))
}

/// Orders branches so the ones people want are first: `main`, then `master`,
/// then everything else alphabetically.
fn sort_branches(mut branches: Vec<String>) -> Vec<String> {
    branches.sort_by_key(|branch| {
        let rank = match branch.as_str() {
            "main" => 0,
            "master" => 1,
            _ => 2,
        };
        (rank, branch.to_lowercase())
    });
    branches
}

/// Extracts the branch from a `refs/heads/...` ref.
///
/// Returns `None` for tag and other refs, which must not be treated as a branch
/// push — Coolify's looser check leaves the full ref in place and relies on a
/// later comparison failing.
pub fn branch_from_ref(git_ref: &str) -> Option<&str> {
    git_ref
        .strip_prefix("refs/heads/")
        .filter(|branch| !branch.is_empty())
}

/// Whether every commit message asks to skip deployment.
///
/// All of them, not any: a push whose last commit says `[skip ci]` but which
/// also carries real changes should still deploy.
pub fn should_skip_deploy(messages: &[String]) -> bool {
    let considered: Vec<&String> = messages.iter().filter(|m| !m.trim().is_empty()).collect();
    if considered.is_empty() {
        return false;
    }

    considered.iter().all(|message| {
        let lowered = message.to_lowercase();
        lowered.contains("[skip ci]")
            || lowered.contains("[skip cd]")
            || lowered.contains("[ci skip]")
    })
}

#[cfg(test)]
mod tests {
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
            manifest.default_permissions.get("contents").map(String::as_str),
            Some("read")
        );
        assert_eq!(
            manifest.default_permissions.get("metadata").map(String::as_str),
            Some("read")
        );
        // Needed to report a deploy's outcome on the commit.
        assert_eq!(
            manifest.default_permissions.get("statuses").map(String::as_str),
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
        let json = serde_json::to_value(build_manifest("nudo", "https://n.example.com"))
            .expect("serialize");
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
        assert_eq!(api_url_from_html_url("https://github.com"), "https://api.github.com");
        assert_eq!(api_url_from_html_url("https://github.com/"), "https://api.github.com");
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
        assert_eq!(api_url_from_html_url("https://GitHub.com"), "https://api.github.com");
    }

    // ---- jwt ----

    // A throwaway 2048-bit RSA key, generated for this test only.
    const TEST_KEY: &str = include_str!("../tests/data/test_app_key.pem");

    #[test]
    fn an_app_jwt_carries_the_claims_github_requires() {
        let token = sign_app_jwt(TEST_KEY, 123456).expect("sign");

        // Header must say RS256.
        let header = jsonwebtoken::decode_header(&token).expect("header");
        assert_eq!(header.alg, jsonwebtoken::Algorithm::RS256);

        // Decode the payload without verifying, to inspect the claims.
        let payload = token.split('.').nth(1).expect("payload");
        let decoded = base64::Engine::decode(
            &base64::engine::general_purpose::URL_SAFE_NO_PAD,
            payload,
        )
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
        assert!(exp - iat <= 600, "exp must be within GitHub's 10 minute limit");
        assert!(exp > now);
    }

    #[test]
    fn a_key_that_is_not_an_rsa_pem_is_rejected_with_a_useful_message() {
        let error = sign_app_jwt("-----BEGIN OPENSSH PRIVATE KEY-----\nnope\n", 1)
            .expect_err("must fail");
        assert!(error.to_string().contains("PEM"), "got: {error}");
    }

    // ---- repo parsing ----

    #[test]
    fn owner_and_name_are_split_from_a_repository_string() {
        assert_eq!(split_repo("owner/name").expect("split"), ("owner", "name"));
        assert_eq!(split_repo(" owner/name ").expect("split"), ("owner", "name"));
        // A .git suffix is what a clone URL carries.
        assert_eq!(split_repo("owner/name.git").expect("split"), ("owner", "name"));
        assert_eq!(split_repo("owner/name/").expect("split"), ("owner", "name"));
        assert_eq!(split_repo("my-org/my_repo.v2").expect("split"), ("my-org", "my_repo.v2"));
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
            assert!(split_repo(hostile).is_err(), "{hostile:?} should be rejected");
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

        assert_eq!(CommitStatus::from_deployment(Status::Queued), Some(CommitStatus::Pending));
        assert_eq!(
            CommitStatus::from_deployment(Status::HealthChecking),
            Some(CommitStatus::Pending)
        );
        assert_eq!(
            CommitStatus::from_deployment(Status::Succeeded),
            Some(CommitStatus::Success)
        );
        assert_eq!(CommitStatus::from_deployment(Status::Failed), Some(CommitStatus::Failure));
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
}
