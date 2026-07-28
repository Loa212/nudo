//! Secret persistence. Values are sealed on the way in and never returned.

use anyhow::bail;
use nudo_proto::Secret;
use sqlx::Row;
use sqlx::sqlite::SqliteRow;
// The SQL below is composed only from `const` fragments plus bound parameters;
// no caller-supplied value is interpolated, which is what `AssertSqlSafe` asserts.
use sqlx::AssertSqlSafe;

use super::{Store, from_db_time, new_id, now_string};
use crate::crypto::{SecretKey, sha256_hex};

impl Store {
    /// Stores a secret within its scope.
    ///
    /// Refuses a name that is already taken unless `replace` is set. A secret's
    /// value cannot be read back, so an accidental overwrite destroys something
    /// unrecoverable and leaves no way to tell what was lost — the name is the
    /// same, and only the digest changed. Rotation is a real operation, so it
    /// stays possible; it just has to be asked for.
    ///
    /// Returns metadata only — a digest and timestamps. The plaintext leaves the
    /// process exactly once more, when the deploy engine writes the target's
    /// `EnvironmentFile`.
    pub async fn put_secret(
        &self,
        key: &SecretKey,
        name: &str,
        value: &str,
        scope_target_id: &str,
        scope_service_id: &str,
        replace: bool,
    ) -> anyhow::Result<Secret> {
        let name = name.trim();
        if name.is_empty() {
            bail!("a secret needs a name");
        }
        // The name becomes an environment variable key on the target, so it has
        // to be one systemd and the shell will accept.
        if !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
            || name.chars().next().is_some_and(|c| c.is_ascii_digit())
        {
            bail!(
                "secret name {name:?} is not a valid environment variable name \
                 (letters, digits and underscore; not starting with a digit)"
            );
        }

        let target_scope = normalize_scope(scope_target_id);
        let service_scope = normalize_scope(scope_service_id);

        if let Some(target_id) = &target_scope
            && self.get_target(target_id).await?.is_none()
        {
            bail!("no such target: {target_id}");
        }
        if let Some(service_id) = &service_scope
            && self.get_service(service_id).await?.is_none()
        {
            bail!("no such service: {service_id}");
        }

        // Checked before the write rather than relying on the unique index, so
        // the refusal can name the thing that already exists and say what to do
        // about it. The index still backstops a race.
        let existing = self
            .find_secret(name, target_scope.as_deref(), service_scope.as_deref())
            .await?;
        if let Some(existing) = &existing
            && !replace
        {
            bail!(
                "a secret named {name:?} already exists in this scope. Its value \
                 cannot be read back, so it is not overwritten by accident — \
                 rotate it deliberately, or delete it first ({})",
                existing.id
            );
        }

        let sealed = key.seal(value)?;
        let digest = sha256_hex(value);
        let now = now_string();

        // The upsert stays, but only a caller that asked for it can reach here
        // with a name already present. `created_at` is left alone on the update
        // path so a rotated secret keeps the date it was first stored.
        sqlx::query(
            "INSERT INTO secrets
               (id, name, value_enc, digest, scope_target_id, scope_service_id,
                updated_at, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?7)
             ON CONFLICT (name, COALESCE(scope_target_id, ''), COALESCE(scope_service_id, ''))
             DO UPDATE SET value_enc = ?3, digest = ?4, updated_at = ?7",
        )
        .bind(new_id("sec"))
        .bind(name)
        .bind(&sealed)
        .bind(&digest)
        .bind(&target_scope)
        .bind(&service_scope)
        .bind(&now)
        .execute(self.pool())
        .await?;

        self.find_secret(name, target_scope.as_deref(), service_scope.as_deref())
            .await?
            .ok_or_else(|| anyhow::anyhow!("secret vanished immediately after write"))
    }

    /// Looks up one secret's metadata by name and scope.
    pub async fn find_secret(
        &self,
        name: &str,
        scope_target_id: Option<&str>,
        scope_service_id: Option<&str>,
    ) -> anyhow::Result<Option<Secret>> {
        let row = sqlx::query(
            "SELECT id, name, scope_target_id, scope_service_id, updated_at, digest
             FROM secrets
             WHERE name = ?1
               AND COALESCE(scope_target_id, '') = ?2
               AND COALESCE(scope_service_id, '') = ?3",
        )
        .bind(name)
        .bind(scope_target_id.unwrap_or_default())
        .bind(scope_service_id.unwrap_or_default())
        .fetch_optional(self.pool())
        .await?;
        Ok(row.as_ref().map(row_to_secret))
    }

    pub async fn get_secret(&self, id: &str) -> anyhow::Result<Option<Secret>> {
        let row = sqlx::query(
            "SELECT id, name, scope_target_id, scope_service_id, updated_at, digest
             FROM secrets WHERE id = ?1",
        )
        .bind(id)
        .fetch_optional(self.pool())
        .await?;
        Ok(row.as_ref().map(row_to_secret))
    }

    /// Lists secret metadata, optionally narrowed to a scope.
    pub async fn list_secrets(
        &self,
        target_id: &str,
        service_id: &str,
    ) -> anyhow::Result<Vec<Secret>> {
        let mut sql = String::from(
            "SELECT id, name, scope_target_id, scope_service_id, updated_at, digest
             FROM secrets WHERE 1 = 1",
        );
        if !target_id.trim().is_empty() {
            sql.push_str(" AND scope_target_id = ?1");
        }
        if !service_id.trim().is_empty() {
            sql.push_str(if target_id.trim().is_empty() {
                " AND scope_service_id = ?1"
            } else {
                " AND scope_service_id = ?2"
            });
        }
        sql.push_str(" ORDER BY name ASC");

        let mut query = sqlx::query(AssertSqlSafe(sql.clone()));
        if !target_id.trim().is_empty() {
            query = query.bind(target_id.trim());
        }
        if !service_id.trim().is_empty() {
            query = query.bind(service_id.trim());
        }

        let rows = query.fetch_all(self.pool()).await?;
        Ok(rows.iter().map(row_to_secret).collect())
    }

    /// Reads and decrypts a secret's value.
    ///
    /// Deliberately not reachable from the gRPC surface: only the deploy engine
    /// and the SSH layer call this, to build an `EnvironmentFile` or to
    /// authenticate to a target.
    pub async fn reveal_secret(&self, key: &SecretKey, id: &str) -> anyhow::Result<Option<String>> {
        let row: Option<(String,)> = sqlx::query_as("SELECT value_enc FROM secrets WHERE id = ?1")
            .bind(id)
            .fetch_optional(self.pool())
            .await?;

        match row {
            Some((sealed,)) => Ok(Some(key.open(&sealed)?)),
            None => Ok(None),
        }
    }

    /// Resolves a service's secrets into name/value pairs for its
    /// `EnvironmentFile`.
    ///
    /// Ordered so the rendered file is byte-stable across deploys, and missing
    /// ids are reported rather than silently skipped — a service starting
    /// without a secret it expects is worse than a failed deploy.
    pub async fn resolve_service_secrets(
        &self,
        key: &SecretKey,
        secret_ids: &[String],
    ) -> anyhow::Result<std::collections::BTreeMap<String, String>> {
        let mut resolved = std::collections::BTreeMap::new();

        for id in secret_ids {
            let row: Option<(String, String)> =
                sqlx::query_as("SELECT name, value_enc FROM secrets WHERE id = ?1")
                    .bind(id)
                    .fetch_optional(self.pool())
                    .await?;

            let (name, sealed) = row.ok_or_else(|| {
                anyhow::anyhow!("secret {id} is referenced by the service but does not exist")
            })?;
            resolved.insert(name, key.open(&sealed)?);
        }

        Ok(resolved)
    }

    pub async fn delete_secret(&self, id: &str) -> anyhow::Result<()> {
        let result = sqlx::query("DELETE FROM secrets WHERE id = ?1")
            .bind(id)
            .execute(self.pool())
            .await?;
        if result.rows_affected() == 0 {
            bail!("no such secret: {id}");
        }
        Ok(())
    }

    /// Whether any service still references this secret, so a delete can warn
    /// rather than break the next deploy.
    pub async fn secret_referenced_by(&self, id: &str) -> anyhow::Result<Vec<String>> {
        let rows: Vec<(String, String)> = sqlx::query_as("SELECT id, secret_ids FROM services")
            .fetch_all(self.pool())
            .await?;

        Ok(rows
            .into_iter()
            .filter(|(_, ids)| super::decode_list(ids).iter().any(|s| s == id))
            .map(|(service_id, _)| service_id)
            .collect())
    }
}

/// An empty scope means "not scoped"; stored as NULL so the unique index and
/// the foreign keys behave.
fn normalize_scope(value: &str) -> Option<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

fn row_to_secret(row: &SqliteRow) -> Secret {
    Secret {
        id: row.get("id"),
        name: row.get("name"),
        scope_target_id: row
            .get::<Option<String>, _>("scope_target_id")
            .unwrap_or_default(),
        scope_service_id: row
            .get::<Option<String>, _>("scope_service_id")
            .unwrap_or_default(),
        updated_at: nudo_proto::to_timestamp_opt(from_db_time(&row.get::<String, _>("updated_at"))),
        digest: row.get("digest"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::TargetInput;

    async fn fixture() -> (Store, SecretKey, String, String) {
        let store = Store::open_in_memory().await.expect("open");
        let key = SecretKey::generate();
        let target = store
            .create_target(&TargetInput {
                name: "box".to_string(),
                host: "10.0.0.1".to_string(),
                ..Default::default()
            })
            .await
            .expect("target");
        let service = store
            .create_service(&nudo_proto::Service {
                target_id: target.id.clone(),
                name: "bot".to_string(),
                ..Default::default()
            })
            .await
            .expect("service");
        (store, key, target.id, service.id)
    }

    #[tokio::test]
    async fn a_stored_secret_returns_metadata_but_never_its_value() {
        let (store, key, _, _) = fixture().await;
        let secret = store
            .put_secret(&key, "API_KEY", "super-secret", "", "", false)
            .await
            .expect("put");

        assert!(secret.id.starts_with("sec_"));
        assert_eq!(secret.name, "API_KEY");
        // The digest lets a client detect drift without reading the value.
        assert_eq!(secret.digest, sha256_hex("super-secret"));

        // Nothing in the returned message carries the plaintext.
        let rendered = format!("{secret:?}");
        assert!(!rendered.contains("super-secret"));
    }

    #[tokio::test]
    async fn the_stored_column_is_ciphertext_not_plaintext() {
        let (store, key, _, _) = fixture().await;
        store
            .put_secret(&key, "TOKEN", "plaintext-value", "", "", false)
            .await
            .expect("put");

        let (stored,): (String,) = sqlx::query_as("SELECT value_enc FROM secrets")
            .fetch_one(store.pool())
            .await
            .expect("query");
        assert!(
            !stored.contains("plaintext-value"),
            "secret stored in the clear"
        );
        assert_eq!(key.open(&stored).expect("open"), "plaintext-value");
    }

    #[tokio::test]
    async fn writing_a_name_that_is_taken_is_refused_and_changes_nothing() {
        // The value cannot be read back, so an overwrite destroys something
        // unrecoverable. Refusing is what makes that impossible by accident.
        let (store, key, _, _) = fixture().await;
        let first = store
            .put_secret(&key, "TAKEN", "v1", "", "", false)
            .await
            .expect("put");

        let error = store
            .put_secret(&key, "TAKEN", "v2", "", "", false)
            .await
            .expect_err("the second write must be refused");
        let message = error.to_string();
        assert!(message.contains("already exists"), "got: {message}");
        // The message has to name the row, so it can be dealt with.
        assert!(message.contains(&first.id), "got: {message}");

        // And the original value survives untouched.
        assert_eq!(
            store.reveal_secret(&key, &first.id).await.expect("reveal"),
            Some("v1".to_string())
        );
        assert_eq!(store.list_secrets("", "").await.expect("list").len(), 1);
    }

    #[tokio::test]
    async fn rotating_replaces_the_value_in_the_same_row() {
        // Rotation stays available — a leaked key has to be replaceable.
        let (store, key, _, _) = fixture().await;
        let first = store
            .put_secret(&key, "ROTATE", "v1", "", "", false)
            .await
            .expect("put");
        let second = store
            .put_secret(&key, "ROTATE", "v2", "", "", true)
            .await
            .expect("rotate");

        assert_eq!(first.id, second.id, "the same row is updated");
        assert_ne!(first.digest, second.digest);
        assert_eq!(
            store.reveal_secret(&key, &second.id).await.expect("reveal"),
            Some("v2".to_string())
        );
        assert_eq!(store.list_secrets("", "").await.expect("list").len(), 1);
    }

    #[tokio::test]
    async fn a_name_taken_in_one_scope_is_still_free_in_another() {
        // The refusal is per scope, not per name: two targets may each hold a
        // DATABASE_URL, and neither should block the other.
        let (store, key, target_id, _) = fixture().await;
        store
            .put_secret(&key, "SCOPED", "global", "", "", false)
            .await
            .expect("global");
        store
            .put_secret(&key, "SCOPED", "per-target", &target_id, "", false)
            .await
            .expect("a different scope is a different secret");

        assert_eq!(store.list_secrets("", "").await.expect("list").len(), 2);
    }

    #[tokio::test]
    async fn the_same_name_can_exist_in_different_scopes() {
        let (store, key, target_id, service_id) = fixture().await;

        let global = store
            .put_secret(&key, "DB_URL", "global", "", "", false)
            .await
            .expect("put");
        let per_target = store
            .put_secret(&key, "DB_URL", "target", &target_id, "", false)
            .await
            .expect("put");
        let per_service = store
            .put_secret(&key, "DB_URL", "service", "", &service_id, false)
            .await
            .expect("put");

        assert_eq!(
            std::collections::HashSet::from([
                global.id.clone(),
                per_target.id.clone(),
                per_service.id.clone()
            ])
            .len(),
            3,
            "each scope is a distinct secret"
        );

        assert_eq!(
            store
                .reveal_secret(&key, &per_target.id)
                .await
                .expect("reveal"),
            Some("target".to_string())
        );
    }

    #[tokio::test]
    async fn listing_can_be_narrowed_to_a_scope() {
        let (store, key, target_id, service_id) = fixture().await;
        store
            .put_secret(&key, "GLOBAL_ONE", "a", "", "", false)
            .await
            .expect("put");
        store
            .put_secret(&key, "TARGET_ONE", "b", &target_id, "", false)
            .await
            .expect("put");
        store
            .put_secret(&key, "SERVICE_ONE", "c", "", &service_id, false)
            .await
            .expect("put");

        assert_eq!(store.list_secrets("", "").await.expect("list").len(), 3);

        let target_scoped = store.list_secrets(&target_id, "").await.expect("list");
        assert_eq!(target_scoped.len(), 1);
        assert_eq!(target_scoped[0].name, "TARGET_ONE");

        let service_scoped = store.list_secrets("", &service_id).await.expect("list");
        assert_eq!(service_scoped.len(), 1);
        assert_eq!(service_scoped[0].name, "SERVICE_ONE");
    }

    #[tokio::test]
    async fn secret_names_must_be_valid_environment_variable_names() {
        let (store, key, _, _) = fixture().await;

        for good in ["API_KEY", "_private", "KEY2", "lowercase"] {
            store
                .put_secret(&key, good, "v", "", "", false)
                .await
                .expect(good);
        }
        for bad in ["has space", "has-dash", "2LEADING", "dollar$", "", "  "] {
            assert!(
                store
                    .put_secret(&key, bad, "v", "", "", false)
                    .await
                    .is_err(),
                "{bad:?} should be rejected"
            );
        }
    }

    #[tokio::test]
    async fn a_secret_cannot_be_scoped_to_something_that_does_not_exist() {
        let (store, key, _, _) = fixture().await;
        assert!(
            store
                .put_secret(&key, "KEY", "v", "tgt_missing", "", false)
                .await
                .is_err()
        );
        assert!(
            store
                .put_secret(&key, "KEY", "v", "", "svc_missing", false)
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn a_services_secrets_resolve_to_an_ordered_name_value_map() {
        let (store, key, _, _) = fixture().await;
        let b = store
            .put_secret(&key, "B_KEY", "second", "", "", false)
            .await
            .expect("put");
        let a = store
            .put_secret(&key, "A_KEY", "first", "", "", false)
            .await
            .expect("put");

        let resolved = store
            .resolve_service_secrets(&key, &[b.id.clone(), a.id.clone()])
            .await
            .expect("resolve");

        // BTreeMap ordering makes the rendered EnvironmentFile byte-stable
        // regardless of the order the ids were listed in.
        let names: Vec<&String> = resolved.keys().collect();
        assert_eq!(names, vec!["A_KEY", "B_KEY"]);
        assert_eq!(resolved.get("A_KEY").map(String::as_str), Some("first"));
    }

    #[tokio::test]
    async fn resolving_a_missing_secret_fails_loudly() {
        // Starting a service without a secret it expects is worse than a
        // failed deploy.
        let (store, key, _, _) = fixture().await;
        let error = store
            .resolve_service_secrets(&key, &["sec_missing".to_string()])
            .await
            .expect_err("must fail");
        assert!(error.to_string().contains("does not exist"), "got: {error}");
    }

    #[tokio::test]
    async fn a_secret_sealed_under_a_different_key_cannot_be_read() {
        let (store, key, _, _) = fixture().await;
        let secret = store
            .put_secret(&key, "KEY", "v", "", "", false)
            .await
            .expect("put");
        // Simulates an operator losing or rotating the key file.
        assert!(
            store
                .reveal_secret(&SecretKey::generate(), &secret.id)
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn revealing_a_missing_secret_is_none_not_an_error() {
        let (store, key, _, _) = fixture().await;
        assert!(
            store
                .reveal_secret(&key, "sec_nope")
                .await
                .expect("reveal")
                .is_none()
        );
    }

    #[tokio::test]
    async fn deleting_a_secret_works_once() {
        let (store, key, _, _) = fixture().await;
        let secret = store
            .put_secret(&key, "GONE", "v", "", "", false)
            .await
            .expect("put");

        store.delete_secret(&secret.id).await.expect("delete");
        assert!(store.get_secret(&secret.id).await.expect("get").is_none());
        assert!(store.delete_secret(&secret.id).await.is_err());
    }

    #[tokio::test]
    async fn references_from_services_are_discoverable_before_a_delete() {
        let (store, key, target_id, _) = fixture().await;
        let secret = store
            .put_secret(&key, "USED", "v", "", "", false)
            .await
            .expect("put");

        let service = store
            .create_service(&nudo_proto::Service {
                target_id,
                name: "consumer".to_string(),
                secret_ids: vec![secret.id.clone()],
                ..Default::default()
            })
            .await
            .expect("service");

        let referencing = store.secret_referenced_by(&secret.id).await.expect("refs");
        assert_eq!(referencing, vec![service.id]);
        assert!(
            store
                .secret_referenced_by("sec_other")
                .await
                .expect("refs")
                .is_empty()
        );
    }

    #[tokio::test]
    async fn deleting_a_scope_deletes_the_secrets_scoped_to_it() {
        let (store, key, target_id, _) = fixture().await;
        store
            .put_secret(&key, "SCOPED", "v", &target_id, "", false)
            .await
            .expect("put");
        store
            .put_secret(&key, "GLOBAL", "v", "", "", false)
            .await
            .expect("put");

        store.delete_target(&target_id).await.expect("delete");

        let remaining = store.list_secrets("", "").await.expect("list");
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].name, "GLOBAL");
    }
}
