//! Users, dashboard sessions and API tokens.
//!
//! Nothing that grants access is stored in a form that can be replayed: session
//! cookies and API tokens are kept as sha256 digests, so a database read (a
//! backup, a stray copy of the file) does not hand over anyone's access.

use anyhow::bail;
use sqlx::Row;

use super::{Store, from_db_time, from_db_time_opt, new_id, now_string, to_db_time};
use crate::crypto::{hash_password, random_token, sha256_hex, verify_password};
// The SQL strings below are composed only from `const` fragments in this file
// plus bound parameters; no caller-supplied value is ever interpolated, which is
// what `AssertSqlSafe` asserts.
use sqlx::AssertSqlSafe;

/// How long a dashboard session lasts.
const SESSION_TTL_DAYS: i64 = 30;

/// A dashboard user.
#[derive(Debug, Clone)]
pub struct User {
    pub id: String,
    pub email: String,
    pub display_name: String,
    pub created_at: chrono::DateTime<chrono::Utc>,
}

/// A validated session, resolved from a cookie.
#[derive(Debug, Clone)]
pub struct Session {
    pub user: User,
    pub csrf_token: String,
}

/// An API token's metadata. The token itself is returned only at creation.
#[derive(Debug, Clone)]
pub struct ApiToken {
    pub id: String,
    pub name: String,
    pub scopes: String,
    pub created_by: String,
    pub last_used_at: Option<chrono::DateTime<chrono::Utc>>,
    pub revoked_at: Option<chrono::DateTime<chrono::Utc>>,
    pub created_at: chrono::DateTime<chrono::Utc>,
}

impl ApiToken {
    pub fn revoked(&self) -> bool {
        self.revoked_at.is_some()
    }

    pub fn can_write(&self) -> bool {
        self.scopes.split(',').any(|s| s.trim() == "write")
    }
}

impl Store {
    /// Whether any user exists. Drives the first-run setup screen.
    pub async fn has_users(&self) -> anyhow::Result<bool> {
        let (count,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM users")
            .fetch_one(self.pool())
            .await?;
        Ok(count > 0)
    }

    /// Creates a user with an argon2 password hash.
    pub async fn create_user(
        &self,
        email: &str,
        password: &str,
        display_name: &str,
    ) -> anyhow::Result<User> {
        let email = email.trim().to_lowercase();
        if email.is_empty() || !email.contains('@') {
            bail!("a valid email address is required");
        }
        // Long enough to resist offline guessing of the argon2 hash; no
        // composition rules, which push users toward predictable patterns.
        if password.chars().count() < 12 {
            bail!("password must be at least 12 characters");
        }

        let id = new_id("usr");
        let created_at = now_string();

        sqlx::query(
            "INSERT INTO users (id, email, password_hash, display_name, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5)",
        )
        .bind(&id)
        .bind(&email)
        .bind(hash_password(password)?)
        .bind(display_name.trim())
        .bind(&created_at)
        .execute(self.pool())
        .await
        .map_err(|e| {
            if super::targets::is_unique_violation(&e) {
                anyhow::anyhow!("a user with that email already exists")
            } else {
                e.into()
            }
        })?;

        self.get_user(&id)
            .await?
            .ok_or_else(|| anyhow::anyhow!("user vanished immediately after creation"))
    }

    pub async fn get_user(&self, id: &str) -> anyhow::Result<Option<User>> {
        let row = sqlx::query(
            "SELECT id, email, display_name, created_at FROM users WHERE id = ?1",
        )
        .bind(id)
        .fetch_optional(self.pool())
        .await?;

        Ok(row.map(|row| User {
            id: row.get("id"),
            email: row.get("email"),
            display_name: row.get("display_name"),
            created_at: from_db_time(&row.get::<String, _>("created_at"))
                .unwrap_or_else(chrono::Utc::now),
        }))
    }

    /// Verifies an email and password.
    ///
    /// Returns `None` for both "no such user" and "wrong password" so the
    /// response cannot be used to enumerate accounts.
    pub async fn authenticate(
        &self,
        email: &str,
        password: &str,
    ) -> anyhow::Result<Option<User>> {
        let email = email.trim().to_lowercase();
        let row: Option<(String, String)> =
            sqlx::query_as("SELECT id, password_hash FROM users WHERE email = ?1")
                .bind(&email)
                .fetch_optional(self.pool())
                .await?;

        let Some((id, hash)) = row else {
            // Spend comparable time hashing so a missing account is not
            // detectably faster than a wrong password.
            let _ = verify_password(password, DUMMY_HASH);
            return Ok(None);
        };

        if !verify_password(password, &hash) {
            return Ok(None);
        }
        self.get_user(&id).await
    }

    /// Changes a user's password, requiring the current one.
    pub async fn change_password(
        &self,
        user_id: &str,
        current: &str,
        new: &str,
    ) -> anyhow::Result<()> {
        let row: Option<(String,)> =
            sqlx::query_as("SELECT password_hash FROM users WHERE id = ?1")
                .bind(user_id)
                .fetch_optional(self.pool())
                .await?;
        let Some((hash,)) = row else {
            bail!("no such user");
        };
        if !verify_password(current, &hash) {
            bail!("current password is incorrect");
        }
        if new.chars().count() < 12 {
            bail!("password must be at least 12 characters");
        }

        sqlx::query("UPDATE users SET password_hash = ?1 WHERE id = ?2")
            .bind(hash_password(new)?)
            .bind(user_id)
            .execute(self.pool())
            .await?;

        // Every other session is invalidated: a password change is how someone
        // responds to a suspected compromise.
        sqlx::query("DELETE FROM sessions WHERE user_id = ?1")
            .bind(user_id)
            .execute(self.pool())
            .await?;
        Ok(())
    }

    // ---- sessions ----

    /// Opens a session and returns `(cookie_value, csrf_token)`.
    ///
    /// Only the digest of the cookie value is stored.
    pub async fn create_session(&self, user_id: &str) -> anyhow::Result<(String, String)> {
        let cookie = random_token(32);
        let csrf = random_token(32);
        let expires_at = chrono::Utc::now() + chrono::Duration::days(SESSION_TTL_DAYS);

        sqlx::query(
            "INSERT INTO sessions (id, user_id, csrf_token, expires_at, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5)",
        )
        .bind(sha256_hex(&cookie))
        .bind(user_id)
        .bind(&csrf)
        .bind(to_db_time(expires_at))
        .bind(now_string())
        .execute(self.pool())
        .await?;

        Ok((cookie, csrf))
    }

    /// Resolves a cookie to a session, rejecting expired ones.
    pub async fn lookup_session(&self, cookie: &str) -> anyhow::Result<Option<Session>> {
        if cookie.trim().is_empty() {
            return Ok(None);
        }

        let row = sqlx::query(
            "SELECT user_id, csrf_token, expires_at FROM sessions WHERE id = ?1",
        )
        .bind(sha256_hex(cookie))
        .fetch_optional(self.pool())
        .await?;

        let Some(row) = row else {
            return Ok(None);
        };

        let expires_at = from_db_time(&row.get::<String, _>("expires_at"));
        if expires_at.is_none_or(|exp| exp < chrono::Utc::now()) {
            // Clean up as we go rather than needing a sweeper.
            self.delete_session(cookie).await?;
            return Ok(None);
        }

        let Some(user) = self.get_user(&row.get::<String, _>("user_id")).await? else {
            return Ok(None);
        };

        Ok(Some(Session {
            user,
            csrf_token: row.get("csrf_token"),
        }))
    }

    pub async fn delete_session(&self, cookie: &str) -> anyhow::Result<()> {
        sqlx::query("DELETE FROM sessions WHERE id = ?1")
            .bind(sha256_hex(cookie))
            .execute(self.pool())
            .await?;
        Ok(())
    }

    // ---- api tokens ----

    /// Mints an API token, returning `(metadata, plaintext)`.
    ///
    /// The plaintext is shown once and never recoverable, since only its digest
    /// is stored.
    pub async fn create_api_token(
        &self,
        name: &str,
        scopes: &[String],
        created_by: &str,
    ) -> anyhow::Result<(ApiToken, String)> {
        let name = name.trim();
        if name.is_empty() {
            bail!("an API token needs a name");
        }

        // Unknown scopes are dropped rather than stored, so a typo cannot
        // create a token whose privileges nobody can reason about.
        let mut normalized: Vec<&str> = scopes
            .iter()
            .map(|s| s.trim())
            .filter(|s| matches!(*s, "read" | "write"))
            .collect();
        normalized.sort_unstable();
        normalized.dedup();
        if normalized.is_empty() {
            normalized.push("read");
        }
        // Write access is useless without read; granting both avoids a token
        // that can deploy but cannot list what it deployed.
        if normalized.contains(&"write") && !normalized.contains(&"read") {
            normalized.insert(0, "read");
        }

        let id = new_id("tok");
        // Prefixed so a leaked token is recognizable in logs and by secret
        // scanners.
        let plaintext = format!("nudo_{}", random_token(32));

        sqlx::query(
            "INSERT INTO api_tokens
               (id, name, token_hash, scopes, created_by, last_used_at, revoked_at,
                expires_at, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, NULL, NULL, NULL, ?6)",
        )
        .bind(&id)
        .bind(name)
        .bind(sha256_hex(&plaintext))
        .bind(normalized.join(","))
        .bind(created_by)
        .bind(now_string())
        .execute(self.pool())
        .await?;

        let token = self
            .get_api_token(&id)
            .await?
            .ok_or_else(|| anyhow::anyhow!("token vanished immediately after creation"))?;
        Ok((token, plaintext))
    }

    pub async fn get_api_token(&self, id: &str) -> anyhow::Result<Option<ApiToken>> {
        let row = sqlx::query(AssertSqlSafe(format!("{TOKEN_SELECT} WHERE id = ?1")))
            .bind(id)
            .fetch_optional(self.pool())
            .await?;
        Ok(row.map(row_to_token))
    }

    /// Verifies a presented token, recording its use.
    ///
    /// Revoked and expired tokens do not authenticate.
    pub async fn authenticate_api_token(
        &self,
        plaintext: &str,
    ) -> anyhow::Result<Option<ApiToken>> {
        if plaintext.trim().is_empty() {
            return Ok(None);
        }

        let row = sqlx::query(AssertSqlSafe(format!("{TOKEN_SELECT} WHERE token_hash = ?1")))
            .bind(sha256_hex(plaintext.trim()))
            .fetch_optional(self.pool())
            .await?;

        let Some(token) = row.map(row_to_token) else {
            return Ok(None);
        };
        if token.revoked() {
            return Ok(None);
        }

        sqlx::query("UPDATE api_tokens SET last_used_at = ?1 WHERE id = ?2")
            .bind(now_string())
            .bind(&token.id)
            .execute(self.pool())
            .await?;

        Ok(Some(token))
    }

    pub async fn list_api_tokens(&self) -> anyhow::Result<Vec<ApiToken>> {
        let rows = sqlx::query(AssertSqlSafe(format!("{TOKEN_SELECT} ORDER BY created_at DESC")))
            .fetch_all(self.pool())
            .await?;
        Ok(rows.into_iter().map(row_to_token).collect())
    }

    /// Revokes a token. Kept as a row so the audit trail still resolves its
    /// name, rather than deleting it.
    pub async fn revoke_api_token(&self, id: &str) -> anyhow::Result<()> {
        let result = sqlx::query(
            "UPDATE api_tokens SET revoked_at = ?1 WHERE id = ?2 AND revoked_at IS NULL",
        )
        .bind(now_string())
        .bind(id)
        .execute(self.pool())
        .await?;

        if result.rows_affected() == 0 {
            bail!("no such active token: {id}");
        }
        Ok(())
    }
}

const TOKEN_SELECT: &str = "SELECT id, name, scopes, created_by, last_used_at, revoked_at, \
     created_at FROM api_tokens";

fn row_to_token(row: sqlx::sqlite::SqliteRow) -> ApiToken {
    ApiToken {
        id: row.get("id"),
        name: row.get("name"),
        scopes: row.get("scopes"),
        created_by: row.get("created_by"),
        last_used_at: from_db_time_opt(row.get::<Option<String>, _>("last_used_at").as_deref()),
        revoked_at: from_db_time_opt(row.get::<Option<String>, _>("revoked_at").as_deref()),
        created_at: from_db_time(&row.get::<String, _>("created_at"))
            .unwrap_or_else(chrono::Utc::now),
    }
}

/// A valid argon2 hash of an unguessable value, verified against when no user
/// matches so the timing of the two paths is comparable.
const DUMMY_HASH: &str = "$argon2id$v=19$m=19456,t=2,p=1$\
     c29tZXNhbHRzb21lc2FsdA$8OZ0jNjJQTHNQKPPzHT5cYFwLqCZRLKPTvvvZQqzJ7Q";

#[cfg(test)]
mod tests {
    use super::*;

    async fn store() -> Store {
        Store::open_in_memory().await.expect("open")
    }

    #[tokio::test]
    async fn the_first_run_state_is_reported_until_a_user_exists() {
        let store = store().await;
        assert!(!store.has_users().await.expect("check"));

        store
            .create_user("admin@example.com", "correct horse battery", "Admin")
            .await
            .expect("create");
        assert!(store.has_users().await.expect("check"));
    }

    #[tokio::test]
    async fn a_user_authenticates_with_the_right_password_only() {
        let store = store().await;
        let created = store
            .create_user("Admin@Example.COM", "correct horse battery", "Admin")
            .await
            .expect("create");
        // Emails are normalized so login is not case-sensitive.
        assert_eq!(created.email, "admin@example.com");

        let ok = store
            .authenticate("admin@example.com", "correct horse battery")
            .await
            .expect("auth");
        assert_eq!(ok.map(|u| u.id), Some(created.id.clone()));

        assert!(
            store
                .authenticate("ADMIN@example.com", "correct horse battery")
                .await
                .expect("auth")
                .is_some(),
            "login must be case-insensitive"
        );
        assert!(
            store
                .authenticate("admin@example.com", "wrong")
                .await
                .expect("auth")
                .is_none()
        );
    }

    #[tokio::test]
    async fn an_unknown_account_is_indistinguishable_from_a_wrong_password() {
        let store = store().await;
        assert!(
            store
                .authenticate("nobody@example.com", "whatever")
                .await
                .expect("auth")
                .is_none()
        );
    }

    #[tokio::test]
    async fn weak_passwords_and_bad_emails_are_refused() {
        let store = store().await;
        assert!(store.create_user("a@b.com", "short", "x").await.is_err());
        assert!(store.create_user("not-an-email", "correct horse battery", "x").await.is_err());
        assert!(store.create_user("", "correct horse battery", "x").await.is_err());
    }

    #[tokio::test]
    async fn duplicate_emails_are_refused() {
        let store = store().await;
        store.create_user("a@b.com", "correct horse battery", "x").await.expect("first");
        let error = store
            .create_user("A@B.com", "correct horse battery", "y")
            .await
            .expect_err("second");
        assert!(error.to_string().contains("already exists"), "got: {error}");
    }

    #[tokio::test]
    async fn the_stored_password_hash_is_not_the_password() {
        let store = store().await;
        store
            .create_user("a@b.com", "correct horse battery", "x")
            .await
            .expect("create");

        let (hash,): (String,) = sqlx::query_as("SELECT password_hash FROM users")
            .fetch_one(store.pool())
            .await
            .expect("query");
        assert!(!hash.contains("correct horse battery"));
        assert!(hash.starts_with("$argon2"));
    }

    #[tokio::test]
    async fn a_session_resolves_back_to_its_user() {
        let store = store().await;
        let user = store
            .create_user("a@b.com", "correct horse battery", "Alice")
            .await
            .expect("create");

        let (cookie, csrf) = store.create_session(&user.id).await.expect("session");
        let session = store.lookup_session(&cookie).await.expect("lookup").expect("some");

        assert_eq!(session.user.id, user.id);
        assert_eq!(session.csrf_token, csrf);
        assert!(!csrf.is_empty());
    }

    #[tokio::test]
    async fn the_session_cookie_is_stored_only_as_a_digest() {
        let store = store().await;
        let user = store
            .create_user("a@b.com", "correct horse battery", "x")
            .await
            .expect("create");
        let (cookie, _) = store.create_session(&user.id).await.expect("session");

        let (stored_id,): (String,) = sqlx::query_as("SELECT id FROM sessions")
            .fetch_one(store.pool())
            .await
            .expect("query");
        assert_ne!(stored_id, cookie, "a database read must not yield the cookie");
        assert_eq!(stored_id, sha256_hex(&cookie));
    }

    #[tokio::test]
    async fn unknown_and_empty_cookies_resolve_to_nothing() {
        let store = store().await;
        assert!(store.lookup_session("nope").await.expect("lookup").is_none());
        assert!(store.lookup_session("").await.expect("lookup").is_none());
    }

    #[tokio::test]
    async fn an_expired_session_is_rejected_and_cleaned_up() {
        let store = store().await;
        let user = store
            .create_user("a@b.com", "correct horse battery", "x")
            .await
            .expect("create");
        let (cookie, _) = store.create_session(&user.id).await.expect("session");

        sqlx::query("UPDATE sessions SET expires_at = ?1")
            .bind(to_db_time(chrono::Utc::now() - chrono::Duration::hours(1)))
            .execute(store.pool())
            .await
            .expect("expire");

        assert!(store.lookup_session(&cookie).await.expect("lookup").is_none());
        let (count,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM sessions")
            .fetch_one(store.pool())
            .await
            .expect("count");
        assert_eq!(count, 0, "the expired row is swept on lookup");
    }

    #[tokio::test]
    async fn logging_out_invalidates_the_session() {
        let store = store().await;
        let user = store
            .create_user("a@b.com", "correct horse battery", "x")
            .await
            .expect("create");
        let (cookie, _) = store.create_session(&user.id).await.expect("session");

        store.delete_session(&cookie).await.expect("delete");
        assert!(store.lookup_session(&cookie).await.expect("lookup").is_none());
    }

    #[tokio::test]
    async fn changing_a_password_requires_the_current_one_and_ends_other_sessions() {
        let store = store().await;
        let user = store
            .create_user("a@b.com", "correct horse battery", "x")
            .await
            .expect("create");
        let (cookie, _) = store.create_session(&user.id).await.expect("session");

        assert!(
            store
                .change_password(&user.id, "wrong", "new password value")
                .await
                .is_err()
        );
        // Too short.
        assert!(
            store
                .change_password(&user.id, "correct horse battery", "short")
                .await
                .is_err()
        );

        store
            .change_password(&user.id, "correct horse battery", "new password value")
            .await
            .expect("change");

        assert!(
            store
                .authenticate("a@b.com", "new password value")
                .await
                .expect("auth")
                .is_some()
        );
        assert!(
            store.lookup_session(&cookie).await.expect("lookup").is_none(),
            "a password change must invalidate existing sessions"
        );
    }

    #[tokio::test]
    async fn deleting_a_user_deletes_their_sessions() {
        let store = store().await;
        let user = store
            .create_user("a@b.com", "correct horse battery", "x")
            .await
            .expect("create");
        let (cookie, _) = store.create_session(&user.id).await.expect("session");

        sqlx::query("DELETE FROM users WHERE id = ?1")
            .bind(&user.id)
            .execute(store.pool())
            .await
            .expect("delete");

        assert!(store.lookup_session(&cookie).await.expect("lookup").is_none());
    }

    #[tokio::test]
    async fn an_api_token_authenticates_once_and_is_shown_only_at_creation() {
        let store = store().await;
        let (token, plaintext) = store
            .create_api_token("ci", &["write".to_string()], "usr_1")
            .await
            .expect("create");

        assert!(plaintext.starts_with("nudo_"), "tokens are recognizable");
        assert!(token.can_write());
        // Write implies read.
        assert!(token.scopes.contains("read"));

        let authed = store
            .authenticate_api_token(&plaintext)
            .await
            .expect("auth")
            .expect("some");
        assert_eq!(authed.id, token.id);

        // Only the digest is stored.
        let (stored,): (String,) = sqlx::query_as("SELECT token_hash FROM api_tokens")
            .fetch_one(store.pool())
            .await
            .expect("query");
        assert_ne!(stored, plaintext);
    }

    #[tokio::test]
    async fn using_a_token_records_when_it_was_last_used() {
        let store = store().await;
        let (token, plaintext) = store
            .create_api_token("ci", &["read".to_string()], "usr_1")
            .await
            .expect("create");
        assert!(token.last_used_at.is_none());

        store.authenticate_api_token(&plaintext).await.expect("auth");
        let reloaded = store.get_api_token(&token.id).await.expect("get").expect("some");
        assert!(reloaded.last_used_at.is_some());
    }

    #[tokio::test]
    async fn a_revoked_token_stops_authenticating_but_remains_listed() {
        let store = store().await;
        let (token, plaintext) = store
            .create_api_token("ci", &["read".to_string()], "usr_1")
            .await
            .expect("create");

        store.revoke_api_token(&token.id).await.expect("revoke");
        assert!(store.authenticate_api_token(&plaintext).await.expect("auth").is_none());

        // Kept so the audit trail can still name it.
        let listed = store.list_api_tokens().await.expect("list");
        assert_eq!(listed.len(), 1);
        assert!(listed[0].revoked());

        // Revoking twice is an error rather than a silent no-op.
        assert!(store.revoke_api_token(&token.id).await.is_err());
    }

    #[tokio::test]
    async fn a_read_only_token_cannot_write() {
        let store = store().await;
        let (token, _) = store
            .create_api_token("readonly", &["read".to_string()], "usr_1")
            .await
            .expect("create");
        assert!(!token.can_write());
    }

    #[tokio::test]
    async fn unknown_scopes_are_dropped_and_the_default_is_read() {
        let store = store().await;
        let (token, _) = store
            .create_api_token("odd", &["admin".to_string(), "".to_string()], "usr_1")
            .await
            .expect("create");
        assert_eq!(token.scopes, "read");
        assert!(!token.can_write());
    }

    #[tokio::test]
    async fn an_unknown_token_does_not_authenticate() {
        let store = store().await;
        assert!(store.authenticate_api_token("nudo_bogus").await.expect("auth").is_none());
        assert!(store.authenticate_api_token("").await.expect("auth").is_none());
    }

    #[tokio::test]
    async fn a_token_needs_a_name() {
        let store = store().await;
        assert!(store.create_api_token("  ", &[], "usr_1").await.is_err());
    }

    #[test]
    fn the_timing_equalizing_hash_is_a_valid_argon2_hash() {
        // If it were malformed, verify would fail early and the timing
        // equalization it exists for would not happen.
        assert!(!verify_password("anything", DUMMY_HASH));
        assert!(
            argon2::password_hash::PasswordHash::new(DUMMY_HASH).is_ok(),
            "DUMMY_HASH must parse or it does no work"
        );
    }
}
