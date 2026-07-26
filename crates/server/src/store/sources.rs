//! Source persistence — GitHub App installations and deploy keys.
//!
//! App credentials (private key, webhook secret, client secret) are sealed with
//! the secret-store key and are never returned over the API.

use anyhow::bail;
use nudo_proto::{Source, source};
use sqlx::Row;
use sqlx::sqlite::SqliteRow;

use super::{Store, from_db_time, new_id, now_string, to_db_time};
use crate::crypto::{SecretKey, sha256_hex};
// The SQL strings below are composed only from `const` fragments in this file
// plus bound parameters; no caller-supplied value is ever interpolated, which is
// what `AssertSqlSafe` asserts.
use sqlx::AssertSqlSafe;

/// How long a pending manifest flow stays valid.
const SETUP_STATE_TTL_MINUTES: i64 = 60;

/// Credentials returned by GitHub's manifest conversion.
#[derive(Debug, Clone)]
pub struct GithubAppCredentials {
    pub app_id: i64,
    pub slug: String,
    pub client_id: String,
    pub client_secret: String,
    pub private_key: String,
    pub webhook_secret: String,
    pub html_url: String,
}

/// A pending manifest or install flow.
#[derive(Debug, Clone)]
pub struct SetupState {
    pub source_id: String,
    pub action: String,
}

impl Store {
    /// Creates a pending GitHub App source, before GitHub knows about it.
    ///
    /// The manifest flow needs a row to attach the returned credentials to, and
    /// the `state` we hand to GitHub has to reference something.
    pub async fn create_pending_github_source(
        &self,
        name: &str,
        organization: &str,
        api_url: &str,
        html_url: &str,
    ) -> anyhow::Result<Source> {
        let name = name.trim();
        if name.is_empty() {
            bail!("a source needs a name");
        }

        let id = new_id("src");
        sqlx::query(
            "INSERT INTO sources
               (id, name, kind, api_url, html_url, app_id, app_slug, client_id,
                installation_id, account_login, organization, private_key_enc,
                webhook_secret_enc, client_secret_enc, deploy_public_key, installed, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, NULL, '', '', NULL, '', ?6, NULL, NULL, NULL, '', 0, ?7)",
        )
        .bind(&id)
        .bind(name)
        .bind(source::Kind::GithubApp.as_str())
        .bind(api_url.trim())
        .bind(html_url.trim())
        .bind(organization.trim())
        .bind(now_string())
        .execute(self.pool())
        .await?;

        self.get_source(&id)
            .await?
            .ok_or_else(|| anyhow::anyhow!("source vanished immediately after creation"))
    }

    /// Creates a source that clones over SSH with a deploy key.
    pub async fn create_deploy_key_source(
        &self,
        key: &SecretKey,
        name: &str,
        private_key: &str,
        public_key: &str,
    ) -> anyhow::Result<Source> {
        let name = name.trim();
        if name.is_empty() {
            bail!("a source needs a name");
        }

        let id = new_id("src");
        sqlx::query(
            "INSERT INTO sources
               (id, name, kind, api_url, html_url, app_id, app_slug, client_id,
                installation_id, account_login, organization, private_key_enc,
                webhook_secret_enc, client_secret_enc, deploy_public_key, installed, created_at)
             VALUES (?1, ?2, ?3, 'https://api.github.com', 'https://github.com', NULL, '', '',
                     NULL, '', '', ?4, NULL, NULL, ?5, 1, ?6)",
        )
        .bind(&id)
        .bind(name)
        .bind(source::Kind::DeployKey.as_str())
        .bind(key.seal(private_key)?)
        .bind(public_key.trim())
        .bind(now_string())
        .execute(self.pool())
        .await?;

        self.get_source(&id)
            .await?
            .ok_or_else(|| anyhow::anyhow!("source vanished immediately after creation"))
    }

    /// Stores an existing App's credentials, for the paste-them-in path.
    pub async fn attach_github_credentials(
        &self,
        key: &SecretKey,
        source_id: &str,
        credentials: &GithubAppCredentials,
    ) -> anyhow::Result<Source> {
        // Refusing to overwrite existing credentials is what stops a replayed
        // or forged callback from rebinding a configured source to an attacker's
        // App. Ported from Coolify, which checks the same thing.
        let existing: Option<(Option<i64>, Option<String>)> = sqlx::query_as(
            "SELECT app_id, private_key_enc FROM sources WHERE id = ?1",
        )
        .bind(source_id)
        .fetch_optional(self.pool())
        .await?;

        let Some((app_id, private_key)) = existing else {
            bail!("no such source: {source_id}");
        };
        if app_id.is_some() || private_key.is_some() {
            bail!("this source already has GitHub App credentials configured");
        }

        sqlx::query(
            "UPDATE sources SET
               name = ?1, app_id = ?2, app_slug = ?3, client_id = ?4,
               private_key_enc = ?5, webhook_secret_enc = ?6, client_secret_enc = ?7,
               html_url = CASE WHEN ?8 = '' THEN html_url ELSE ?8 END
             WHERE id = ?9",
        )
        .bind(if credentials.slug.trim().is_empty() {
            None
        } else {
            Some(credentials.slug.trim())
        })
        .bind(credentials.app_id)
        .bind(credentials.slug.trim())
        .bind(credentials.client_id.trim())
        .bind(key.seal(&credentials.private_key)?)
        .bind(key.seal(&credentials.webhook_secret)?)
        .bind(key.seal(&credentials.client_secret)?)
        .bind(credentials.html_url.trim())
        .bind(source_id)
        .execute(self.pool())
        .await?;

        // The slug is only used as a name when the caller had no better one;
        // NULL binding above leaves the existing name in place.
        sqlx::query("UPDATE sources SET name = COALESCE(name, ?1) WHERE id = ?2")
            .bind(credentials.slug.trim())
            .bind(source_id)
            .execute(self.pool())
            .await?;

        self.get_source(source_id)
            .await?
            .ok_or_else(|| anyhow::anyhow!("source {source_id} vanished"))
    }

    /// Records the installation that GitHub redirected back with.
    pub async fn set_installation(
        &self,
        source_id: &str,
        installation_id: i64,
        account_login: &str,
    ) -> anyhow::Result<()> {
        sqlx::query(
            "UPDATE sources SET installation_id = ?1, account_login = ?2, installed = 1
             WHERE id = ?3",
        )
        .bind(installation_id)
        .bind(account_login.trim())
        .bind(source_id)
        .execute(self.pool())
        .await?;
        Ok(())
    }

    pub async fn get_source(&self, id: &str) -> anyhow::Result<Option<Source>> {
        let row = sqlx::query(AssertSqlSafe(format!("{SOURCE_SELECT} WHERE id = ?1")))
            .bind(id)
            .fetch_optional(self.pool())
            .await?;
        Ok(row.as_ref().map(row_to_source))
    }

    /// Finds a source by the App id GitHub sends on every webhook delivery in
    /// `X-GitHub-Hook-Installation-Target-Id`.
    pub async fn source_by_app_id(&self, app_id: i64) -> anyhow::Result<Option<Source>> {
        let row = sqlx::query(AssertSqlSafe(format!("{SOURCE_SELECT} WHERE app_id = ?1")))
            .bind(app_id)
            .fetch_optional(self.pool())
            .await?;
        Ok(row.as_ref().map(row_to_source))
    }

    pub async fn list_sources(&self) -> anyhow::Result<Vec<Source>> {
        let rows = sqlx::query(AssertSqlSafe(format!("{SOURCE_SELECT} ORDER BY created_at DESC")))
            .fetch_all(self.pool())
            .await?;
        Ok(rows.iter().map(row_to_source).collect())
    }

    pub async fn delete_source(&self, id: &str) -> anyhow::Result<()> {
        let result = sqlx::query("DELETE FROM sources WHERE id = ?1")
            .bind(id)
            .execute(self.pool())
            .await?;
        if result.rows_affected() == 0 {
            bail!("no such source: {id}");
        }
        Ok(())
    }

    /// The App's private key, for signing JWTs.
    pub async fn source_private_key(
        &self,
        key: &SecretKey,
        source_id: &str,
    ) -> anyhow::Result<Option<String>> {
        let row: Option<(Option<String>,)> =
            sqlx::query_as("SELECT private_key_enc FROM sources WHERE id = ?1")
                .bind(source_id)
                .fetch_optional(self.pool())
                .await?;

        match row.and_then(|(enc,)| enc) {
            Some(sealed) => Ok(Some(key.open(&sealed)?)),
            None => Ok(None),
        }
    }

    /// The App's webhook secret, for verifying delivery signatures.
    pub async fn source_webhook_secret(
        &self,
        key: &SecretKey,
        source_id: &str,
    ) -> anyhow::Result<Option<String>> {
        let row: Option<(Option<String>,)> =
            sqlx::query_as("SELECT webhook_secret_enc FROM sources WHERE id = ?1")
                .bind(source_id)
                .fetch_optional(self.pool())
                .await?;

        match row.and_then(|(enc,)| enc) {
            Some(sealed) => Ok(Some(key.open(&sealed)?)),
            None => Ok(None),
        }
    }

    // ---- setup state ----

    /// Records a pending manifest or install flow and returns the raw `state`.
    ///
    /// Only `sha256(state)` is stored, so reading the database does not let
    /// someone complete a flow they did not start.
    pub async fn create_setup_state(
        &self,
        source_id: &str,
        action: &str,
    ) -> anyhow::Result<String> {
        let state = crate::crypto::random_token(48);
        let expires_at = chrono::Utc::now() + chrono::Duration::minutes(SETUP_STATE_TTL_MINUTES);

        sqlx::query(
            "INSERT INTO github_setup_states (state_hash, source_id, action, expires_at, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5)",
        )
        .bind(sha256_hex(&state))
        .bind(source_id)
        .bind(action)
        .bind(to_db_time(expires_at))
        .bind(now_string())
        .execute(self.pool())
        .await?;

        Ok(state)
    }

    /// Consumes a setup state, atomically.
    ///
    /// The row is deleted as part of the read, so a replayed callback finds
    /// nothing — which is what prevents a captured redirect from being used
    /// twice. The action must match the endpoint that is consuming it, so a
    /// state minted for `install` cannot be spent on `manifest`.
    pub async fn consume_setup_state(
        &self,
        state: &str,
        expected_action: &str,
    ) -> anyhow::Result<Option<SetupState>> {
        if state.trim().is_empty() {
            return Ok(None);
        }

        let hash = sha256_hex(state.trim());
        let row = sqlx::query(
            "DELETE FROM github_setup_states WHERE state_hash = ?1
             RETURNING source_id, action, expires_at",
        )
        .bind(&hash)
        .fetch_optional(self.pool())
        .await?;

        let Some(row) = row else {
            return Ok(None);
        };

        let action: String = row.get("action");
        if action != expected_action {
            return Ok(None);
        }
        if from_db_time(&row.get::<String, _>("expires_at"))
            .is_none_or(|exp| exp < chrono::Utc::now())
        {
            return Ok(None);
        }

        Ok(Some(SetupState {
            source_id: row.get("source_id"),
            action,
        }))
    }

    // ---- installation token cache ----

    /// Reads a cached installation token if it is still comfortably valid.
    ///
    /// A margin is applied so a token that expires mid-clone is treated as
    /// already expired. Coolify mints a fresh token on every call; caching here
    /// avoids a JWT signature and two HTTP round-trips per operation.
    pub async fn cached_installation_token(
        &self,
        key: &SecretKey,
        source_id: &str,
    ) -> anyhow::Result<Option<String>> {
        const REFRESH_MARGIN_SECONDS: i64 = 300;

        let row: Option<(String, String)> = sqlx::query_as(
            "SELECT token_enc, expires_at FROM github_installation_tokens WHERE source_id = ?1",
        )
        .bind(source_id)
        .fetch_optional(self.pool())
        .await?;

        let Some((sealed, expires_at)) = row else {
            return Ok(None);
        };

        let Some(expires_at) = from_db_time(&expires_at) else {
            return Ok(None);
        };
        if expires_at - chrono::Duration::seconds(REFRESH_MARGIN_SECONDS) < chrono::Utc::now() {
            return Ok(None);
        }

        Ok(Some(key.open(&sealed)?))
    }

    /// Caches a freshly minted installation token.
    pub async fn cache_installation_token(
        &self,
        key: &SecretKey,
        source_id: &str,
        token: &str,
        expires_at: chrono::DateTime<chrono::Utc>,
    ) -> anyhow::Result<()> {
        sqlx::query(
            "INSERT INTO github_installation_tokens (source_id, token_enc, expires_at)
             VALUES (?1, ?2, ?3)
             ON CONFLICT (source_id) DO UPDATE SET token_enc = ?2, expires_at = ?3",
        )
        .bind(source_id)
        .bind(key.seal(token)?)
        .bind(to_db_time(expires_at))
        .execute(self.pool())
        .await?;
        Ok(())
    }
}

const SOURCE_SELECT: &str = "SELECT id, name, kind, api_url, html_url, app_id, app_slug, \
     installation_id, account_login, organization, deploy_public_key, installed, created_at \
     FROM sources";

fn row_to_source(row: &SqliteRow) -> Source {
    Source {
        id: row.get("id"),
        name: row.get::<Option<String>, _>("name").unwrap_or_default(),
        kind: source::Kind::parse(&row.get::<String, _>("kind")) as i32,
        app_id: row.get::<Option<i64>, _>("app_id").unwrap_or_default(),
        app_slug: row.get("app_slug"),
        html_url: row.get("html_url"),
        installation_id: row.get::<Option<i64>, _>("installation_id").unwrap_or_default(),
        account_login: row.get("account_login"),
        installed: row.get::<i64, _>("installed") != 0,
        created_at: nudo_proto::to_timestamp_opt(from_db_time(
            &row.get::<String, _>("created_at"),
        )),
    }
}

/// A source's API and HTML base URLs plus its organization, which the manifest
/// and install flows need but which are not on the proto message.
pub struct SourceUrls {
    pub api_url: String,
    pub html_url: String,
    pub organization: String,
}

impl Store {
    pub async fn source_urls(&self, source_id: &str) -> anyhow::Result<Option<SourceUrls>> {
        let row: Option<(String, String, String)> = sqlx::query_as(
            "SELECT api_url, html_url, organization FROM sources WHERE id = ?1",
        )
        .bind(source_id)
        .fetch_optional(self.pool())
        .await?;

        Ok(row.map(|(api_url, html_url, organization)| SourceUrls {
            api_url,
            html_url,
            organization,
        }))
    }

    /// The deploy key for an SSH clone.
    pub async fn source_deploy_key(
        &self,
        key: &SecretKey,
        source_id: &str,
    ) -> anyhow::Result<Option<String>> {
        self.source_private_key(key, source_id).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn fixture() -> (Store, SecretKey) {
        (
            Store::open_in_memory().await.expect("open"),
            SecretKey::generate(),
        )
    }

    fn credentials() -> GithubAppCredentials {
        GithubAppCredentials {
            app_id: 123456,
            slug: "my-nudo-app".to_string(),
            client_id: "Iv1.abc".to_string(),
            client_secret: "client-secret-value".to_string(),
            private_key: "-----BEGIN RSA PRIVATE KEY-----\nkey\n-----END RSA PRIVATE KEY-----"
                .to_string(),
            webhook_secret: "webhook-secret-value".to_string(),
            html_url: "https://github.com/apps/my-nudo-app".to_string(),
        }
    }

    #[tokio::test]
    async fn a_pending_source_starts_uninstalled_with_no_credentials() {
        let (store, _) = fixture().await;
        let source = store
            .create_pending_github_source("pending", "", "https://api.github.com", "https://github.com")
            .await
            .expect("create");

        assert!(source.id.starts_with("src_"));
        assert_eq!(source.kind, source::Kind::GithubApp as i32);
        assert_eq!(source.app_id, 0);
        assert!(!source.installed);
    }

    #[tokio::test]
    async fn attaching_credentials_seals_them_and_never_returns_them() {
        let (store, key) = fixture().await;
        let pending = store
            .create_pending_github_source("pending", "", "https://api.github.com", "https://github.com")
            .await
            .expect("create");

        let configured = store
            .attach_github_credentials(&key, &pending.id, &credentials())
            .await
            .expect("attach");

        assert_eq!(configured.app_id, 123456);
        assert_eq!(configured.app_slug, "my-nudo-app");

        // Nothing secret is on the returned message.
        let rendered = format!("{configured:?}");
        assert!(!rendered.contains("client-secret-value"));
        assert!(!rendered.contains("webhook-secret-value"));
        assert!(!rendered.contains("BEGIN RSA PRIVATE KEY"));

        // The stored columns are ciphertext.
        let (pk, wh): (String, String) = sqlx::query_as(
            "SELECT private_key_enc, webhook_secret_enc FROM sources WHERE id = ?1",
        )
        .bind(&pending.id)
        .fetch_one(store.pool())
        .await
        .expect("query");
        assert!(!pk.contains("BEGIN RSA"));
        assert!(!wh.contains("webhook-secret-value"));

        // But the server can still read them when it needs to.
        assert_eq!(
            store.source_private_key(&key, &pending.id).await.expect("read"),
            Some(credentials().private_key)
        );
        assert_eq!(
            store.source_webhook_secret(&key, &pending.id).await.expect("read"),
            Some("webhook-secret-value".to_string())
        );
    }

    #[tokio::test]
    async fn a_configured_source_cannot_have_its_credentials_replaced() {
        // This is what stops a forged or replayed callback from rebinding an
        // existing source to someone else's App.
        let (store, key) = fixture().await;
        let pending = store
            .create_pending_github_source("p", "", "https://api.github.com", "https://github.com")
            .await
            .expect("create");
        store
            .attach_github_credentials(&key, &pending.id, &credentials())
            .await
            .expect("first");

        let error = store
            .attach_github_credentials(
                &key,
                &pending.id,
                &GithubAppCredentials {
                    app_id: 999,
                    ..credentials()
                },
            )
            .await
            .expect_err("must refuse");
        assert!(error.to_string().contains("already has"), "got: {error}");

        // The original credentials survive.
        let unchanged = store.get_source(&pending.id).await.expect("get").expect("some");
        assert_eq!(unchanged.app_id, 123456);
    }

    #[tokio::test]
    async fn attaching_to_a_missing_source_fails() {
        let (store, key) = fixture().await;
        assert!(
            store
                .attach_github_credentials(&key, "src_nope", &credentials())
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn a_source_is_findable_by_the_app_id_webhooks_carry() {
        let (store, key) = fixture().await;
        let pending = store
            .create_pending_github_source("p", "", "https://api.github.com", "https://github.com")
            .await
            .expect("create");
        store
            .attach_github_credentials(&key, &pending.id, &credentials())
            .await
            .expect("attach");

        let found = store.source_by_app_id(123456).await.expect("lookup").expect("some");
        assert_eq!(found.id, pending.id);
        assert!(store.source_by_app_id(999).await.expect("lookup").is_none());
    }

    #[tokio::test]
    async fn recording_an_installation_marks_the_source_installed() {
        let (store, _) = fixture().await;
        let pending = store
            .create_pending_github_source("p", "", "https://api.github.com", "https://github.com")
            .await
            .expect("create");

        store
            .set_installation(&pending.id, 42, "acme-corp")
            .await
            .expect("install");

        let installed = store.get_source(&pending.id).await.expect("get").expect("some");
        assert_eq!(installed.installation_id, 42);
        assert_eq!(installed.account_login, "acme-corp");
        assert!(installed.installed);
    }

    #[tokio::test]
    async fn a_deploy_key_source_stores_its_key_sealed_and_exposes_the_public_half() {
        let (store, key) = fixture().await;
        let source = store
            .create_deploy_key_source(
                &key,
                "deploy-key",
                "-----BEGIN OPENSSH PRIVATE KEY-----\nprivate\n",
                "ssh-ed25519 AAAAC3Nz nudo",
            )
            .await
            .expect("create");

        assert_eq!(source.kind, source::Kind::DeployKey as i32);
        // Usable immediately; there is no install step.
        assert!(source.installed);

        assert_eq!(
            store.source_deploy_key(&key, &source.id).await.expect("read"),
            Some("-----BEGIN OPENSSH PRIVATE KEY-----\nprivate\n".to_string())
        );
    }

    #[tokio::test]
    async fn a_setup_state_is_single_use() {
        let (store, _) = fixture().await;
        let source = store
            .create_pending_github_source("p", "", "https://api.github.com", "https://github.com")
            .await
            .expect("create");

        let state = store.create_setup_state(&source.id, "manifest").await.expect("state");

        let consumed = store
            .consume_setup_state(&state, "manifest")
            .await
            .expect("consume")
            .expect("some");
        assert_eq!(consumed.source_id, source.id);

        // A replay finds nothing.
        assert!(
            store.consume_setup_state(&state, "manifest").await.expect("consume").is_none(),
            "a state must not be usable twice"
        );
    }

    #[tokio::test]
    async fn the_raw_state_is_not_stored() {
        let (store, _) = fixture().await;
        let source = store
            .create_pending_github_source("p", "", "https://api.github.com", "https://github.com")
            .await
            .expect("create");
        let state = store.create_setup_state(&source.id, "manifest").await.expect("state");

        let (stored,): (String,) = sqlx::query_as("SELECT state_hash FROM github_setup_states")
            .fetch_one(store.pool())
            .await
            .expect("query");
        assert_ne!(stored, state);
        assert_eq!(stored, sha256_hex(&state));
    }

    #[tokio::test]
    async fn a_state_minted_for_one_action_cannot_be_spent_on_another() {
        let (store, _) = fixture().await;
        let source = store
            .create_pending_github_source("p", "", "https://api.github.com", "https://github.com")
            .await
            .expect("create");
        let state = store.create_setup_state(&source.id, "install").await.expect("state");

        assert!(
            store.consume_setup_state(&state, "manifest").await.expect("consume").is_none(),
            "the action must match the endpoint consuming it"
        );
    }

    #[tokio::test]
    async fn an_unknown_or_empty_state_is_rejected() {
        let (store, _) = fixture().await;
        assert!(store.consume_setup_state("guessed", "manifest").await.expect("c").is_none());
        assert!(store.consume_setup_state("", "manifest").await.expect("c").is_none());
    }

    #[tokio::test]
    async fn an_expired_state_is_rejected() {
        let (store, _) = fixture().await;
        let source = store
            .create_pending_github_source("p", "", "https://api.github.com", "https://github.com")
            .await
            .expect("create");
        let state = store.create_setup_state(&source.id, "manifest").await.expect("state");

        sqlx::query("UPDATE github_setup_states SET expires_at = ?1")
            .bind(to_db_time(chrono::Utc::now() - chrono::Duration::minutes(1)))
            .execute(store.pool())
            .await
            .expect("expire");

        assert!(store.consume_setup_state(&state, "manifest").await.expect("c").is_none());
    }

    #[tokio::test]
    async fn a_cached_token_is_returned_until_it_nears_expiry() {
        let (store, key) = fixture().await;
        let source = store
            .create_pending_github_source("p", "", "https://api.github.com", "https://github.com")
            .await
            .expect("create");

        // GitHub issues these with an hour of life.
        store
            .cache_installation_token(
                &key,
                &source.id,
                "ghs_token",
                chrono::Utc::now() + chrono::Duration::minutes(60),
            )
            .await
            .expect("cache");
        assert_eq!(
            store.cached_installation_token(&key, &source.id).await.expect("read"),
            Some("ghs_token".to_string())
        );

        // Inside the refresh margin it is treated as already gone, so a clone
        // cannot start with a token that dies mid-transfer.
        store
            .cache_installation_token(
                &key,
                &source.id,
                "ghs_token",
                chrono::Utc::now() + chrono::Duration::seconds(60),
            )
            .await
            .expect("cache");
        assert!(
            store
                .cached_installation_token(&key, &source.id)
                .await
                .expect("read")
                .is_none()
        );
    }

    #[tokio::test]
    async fn caching_a_token_twice_replaces_it() {
        let (store, key) = fixture().await;
        let source = store
            .create_pending_github_source("p", "", "https://api.github.com", "https://github.com")
            .await
            .expect("create");

        let later = chrono::Utc::now() + chrono::Duration::minutes(60);
        store.cache_installation_token(&key, &source.id, "first", later).await.expect("a");
        store.cache_installation_token(&key, &source.id, "second", later).await.expect("b");

        assert_eq!(
            store.cached_installation_token(&key, &source.id).await.expect("read"),
            Some("second".to_string())
        );
    }

    #[tokio::test]
    async fn a_cached_token_is_stored_sealed() {
        let (store, key) = fixture().await;
        let source = store
            .create_pending_github_source("p", "", "https://api.github.com", "https://github.com")
            .await
            .expect("create");
        store
            .cache_installation_token(
                &key,
                &source.id,
                "ghs_secret_token",
                chrono::Utc::now() + chrono::Duration::minutes(60),
            )
            .await
            .expect("cache");

        let (stored,): (String,) =
            sqlx::query_as("SELECT token_enc FROM github_installation_tokens")
                .fetch_one(store.pool())
                .await
                .expect("query");
        assert!(!stored.contains("ghs_secret_token"));
    }

    #[tokio::test]
    async fn deleting_a_source_removes_its_states_and_cached_tokens() {
        let (store, key) = fixture().await;
        let source = store
            .create_pending_github_source("p", "", "https://api.github.com", "https://github.com")
            .await
            .expect("create");
        let state = store.create_setup_state(&source.id, "manifest").await.expect("state");
        store
            .cache_installation_token(
                &key,
                &source.id,
                "t",
                chrono::Utc::now() + chrono::Duration::minutes(60),
            )
            .await
            .expect("cache");

        store.delete_source(&source.id).await.expect("delete");

        assert!(store.consume_setup_state(&state, "manifest").await.expect("c").is_none());
        assert!(
            store
                .cached_installation_token(&key, &source.id)
                .await
                .expect("read")
                .is_none()
        );
        assert!(store.delete_source(&source.id).await.is_err());
    }

    #[tokio::test]
    async fn the_urls_and_organization_are_readable_for_the_manifest_flow() {
        let (store, _) = fixture().await;
        let source = store
            .create_pending_github_source(
                "p",
                "acme-corp",
                "https://api.octocorp.ghe.com",
                "https://octocorp.ghe.com",
            )
            .await
            .expect("create");

        let urls = store.source_urls(&source.id).await.expect("urls").expect("some");
        assert_eq!(urls.api_url, "https://api.octocorp.ghe.com");
        assert_eq!(urls.html_url, "https://octocorp.ghe.com");
        assert_eq!(urls.organization, "acme-corp");
    }

    #[tokio::test]
    async fn sources_list_newest_first() {
        let (store, _) = fixture().await;
        store
            .create_pending_github_source("first", "", "https://api.github.com", "https://github.com")
            .await
            .expect("a");
        tokio::time::sleep(std::time::Duration::from_millis(2)).await;
        store
            .create_pending_github_source("second", "", "https://api.github.com", "https://github.com")
            .await
            .expect("b");

        let listed = store.list_sources().await.expect("list");
        assert_eq!(listed.len(), 2);
        assert_eq!(listed[0].name, "second");
    }
}
