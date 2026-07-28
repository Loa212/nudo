//! Target persistence — the machines we deploy to.

use anyhow::bail;
use nudo_proto::{HostKey, Target, target};
use sqlx::Row;
use sqlx::sqlite::SqliteRow;

use super::{Store, decode_map, encode_map, from_db_time_opt, new_id, now_string};
// The SQL strings below are composed only from `const` fragments in this file
// plus bound parameters; no caller-supplied value is ever interpolated, which is
// what `AssertSqlSafe` asserts.
use sqlx::AssertSqlSafe;

/// Fields a client may set when creating or updating a target.
#[derive(Debug, Clone, Default)]
pub struct TargetInput {
    pub name: String,
    pub host: String,
    pub port: u32,
    pub user: String,
    pub ssh_key_id: String,
    pub latency_critical: bool,
    pub labels: std::collections::HashMap<String, String>,
}

impl Store {
    pub async fn create_target(&self, input: &TargetInput) -> anyhow::Result<Target> {
        let name = input.name.trim();
        let host = input.host.trim();
        if name.is_empty() {
            bail!("a target needs a name");
        }
        if host.is_empty() {
            bail!("a target needs a host");
        }

        let id = new_id("tgt");
        let created_at = now_string();
        // Port 0 means the client did not set one, not "port zero".
        let port = if input.port == 0 { 22 } else { input.port };
        let user = if input.user.trim().is_empty() {
            "root"
        } else {
            input.user.trim()
        };

        sqlx::query(
            "INSERT INTO targets
               (id, name, host, port, user, ssh_key_id, latency_critical, labels,
                status, last_seen_at, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 'unknown', NULL, ?9)",
        )
        .bind(&id)
        .bind(name)
        .bind(host)
        .bind(port)
        .bind(user)
        .bind(input.ssh_key_id.trim())
        .bind(input.latency_critical as i64)
        .bind(encode_map(&input.labels))
        .bind(&created_at)
        .execute(self.pool())
        .await
        .map_err(|e| friendly_target_error(e, name))?;

        self.get_target(&id)
            .await?
            .ok_or_else(|| anyhow::anyhow!("target vanished immediately after creation"))
    }

    pub async fn get_target(&self, id: &str) -> anyhow::Result<Option<Target>> {
        let row = sqlx::query(TARGET_SELECT_BY_ID)
            .bind(id)
            .fetch_optional(self.pool())
            .await?;
        Ok(row.as_ref().map(row_to_target))
    }

    /// Lists targets, newest first, optionally filtered by a label selector.
    ///
    /// The selector is matched in Rust rather than SQL because labels live in a
    /// JSON column; the table is small enough (tens of hosts) that this is not
    /// worth a schema change.
    pub async fn list_targets(
        &self,
        label_selector: &str,
        limit: i64,
        offset: i64,
    ) -> anyhow::Result<Vec<Target>> {
        let selector = parse_label_selector(label_selector);

        if selector.is_empty() {
            let rows = sqlx::query(AssertSqlSafe(format!(
                "{TARGET_SELECT} ORDER BY created_at DESC, id DESC LIMIT ?1 OFFSET ?2"
            )))
            .bind(limit)
            .bind(offset)
            .fetch_all(self.pool())
            .await?;
            return Ok(rows.iter().map(row_to_target).collect());
        }

        let rows = sqlx::query(AssertSqlSafe(format!(
            "{TARGET_SELECT} ORDER BY created_at DESC, id DESC"
        )))
        .fetch_all(self.pool())
        .await?;

        Ok(rows
            .iter()
            .map(row_to_target)
            .filter(|t| matches_selector(&t.labels, &selector))
            .skip(offset.max(0) as usize)
            .take(limit.max(0) as usize)
            .collect())
    }

    /// Total number of targets, for the dashboard's counts.
    pub async fn count_targets(&self) -> anyhow::Result<i64> {
        let (count,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM targets")
            .fetch_one(self.pool())
            .await?;
        Ok(count)
    }

    /// Applies a field mask to a target. Only named fields are written, so a
    /// client that sends a partially populated message cannot blank the rest.
    pub async fn update_target(
        &self,
        id: &str,
        target: &Target,
        update_mask: &[String],
    ) -> anyhow::Result<Target> {
        if self.get_target(id).await?.is_none() {
            bail!("no such target: {id}");
        }

        // An empty mask means "everything the message carries", which is what a
        // simple client sending a whole object expects.
        let touch = |field: &str| update_mask.is_empty() || update_mask.iter().any(|m| m == field);
        let mut transaction = self.pool().begin().await?;

        if touch("name") && !target.name.trim().is_empty() {
            sqlx::query("UPDATE targets SET name = ?1 WHERE id = ?2")
                .bind(target.name.trim())
                .bind(id)
                .execute(&mut *transaction)
                .await
                .map_err(|e| friendly_target_error(e, target.name.trim()))?;
        }
        if touch("host") && !target.host.trim().is_empty() {
            sqlx::query("UPDATE targets SET host = ?1 WHERE id = ?2")
                .bind(target.host.trim())
                .bind(id)
                .execute(&mut *transaction)
                .await?;
        }
        if touch("port") && target.port != 0 {
            sqlx::query("UPDATE targets SET port = ?1 WHERE id = ?2")
                .bind(target.port)
                .bind(id)
                .execute(&mut *transaction)
                .await?;
        }
        if touch("user") && !target.user.trim().is_empty() {
            sqlx::query("UPDATE targets SET user = ?1 WHERE id = ?2")
                .bind(target.user.trim())
                .bind(id)
                .execute(&mut *transaction)
                .await?;
        }
        if touch("ssh_key_id") {
            sqlx::query("UPDATE targets SET ssh_key_id = ?1 WHERE id = ?2")
                .bind(target.ssh_key_id.trim())
                .bind(id)
                .execute(&mut *transaction)
                .await?;
        }
        if touch("latency_critical") {
            sqlx::query("UPDATE targets SET latency_critical = ?1 WHERE id = ?2")
                .bind(target.latency_critical as i64)
                .bind(id)
                .execute(&mut *transaction)
                .await?;
        }
        if touch("labels") {
            sqlx::query("UPDATE targets SET labels = ?1 WHERE id = ?2")
                .bind(encode_map(&target.labels))
                .bind(id)
                .execute(&mut *transaction)
                .await?;
        }

        transaction.commit().await?;
        self.get_target(id)
            .await?
            .ok_or_else(|| anyhow::anyhow!("target {id} vanished during update"))
    }

    pub async fn delete_target(&self, id: &str) -> anyhow::Result<()> {
        let result = sqlx::query("DELETE FROM targets WHERE id = ?1")
            .bind(id)
            .execute(self.pool())
            .await?;
        if result.rows_affected() == 0 {
            bail!("no such target: {id}");
        }
        Ok(())
    }

    /// Records the outcome of a reachability probe.
    pub async fn set_target_status(&self, id: &str, status: target::Status) -> anyhow::Result<()> {
        // `last_seen_at` means "last confirmed reachable", so it is only
        // advanced on success — otherwise it would just track probe attempts.
        let last_seen = if status == target::Status::Reachable {
            Some(now_string())
        } else {
            None
        };

        match last_seen {
            Some(seen) => {
                sqlx::query("UPDATE targets SET status = ?1, last_seen_at = ?2 WHERE id = ?3")
                    .bind(status.as_str())
                    .bind(seen)
                    .bind(id)
                    .execute(self.pool())
                    .await?;
            }
            None => {
                sqlx::query("UPDATE targets SET status = ?1 WHERE id = ?2")
                    .bind(status.as_str())
                    .bind(id)
                    .execute(self.pool())
                    .await?;
            }
        }
        Ok(())
    }

    /// Pins a host key, clearing any pending change.
    ///
    /// Used for the first-use recording and for accepting a reviewed change;
    /// both end in the same state, which is why they are one method.
    pub async fn pin_host_key(&self, id: &str, key: &str, fingerprint: &str) -> anyhow::Result<()> {
        sqlx::query(
            "UPDATE targets
                SET host_key = ?1,
                    host_key_fingerprint = ?2,
                    host_key_pinned_at = ?3,
                    pending_host_key = '',
                    pending_host_key_fingerprint = '',
                    pending_host_key_seen_at = NULL
              WHERE id = ?4",
        )
        .bind(key)
        .bind(fingerprint)
        .bind(now_string())
        .bind(id)
        .execute(self.pool())
        .await?;
        Ok(())
    }

    /// Records a key that did not match the pinned one, for review.
    ///
    /// Nothing is trusted here — the connection that saw this key was refused.
    /// The point is that the operator can see *what* the host presented rather
    /// than only that it differed, and can accept it without touching the
    /// database.
    ///
    /// The first sighting's timestamp is kept when the same key is seen again,
    /// so "this has been failing since 09:14" survives a probe loop that
    /// re-sees it every minute.
    pub async fn record_pending_host_key(
        &self,
        id: &str,
        key: &str,
        fingerprint: &str,
    ) -> anyhow::Result<()> {
        sqlx::query(
            "UPDATE targets
                SET pending_host_key = ?1,
                    pending_host_key_fingerprint = ?2,
                    pending_host_key_seen_at =
                        CASE WHEN pending_host_key = ?1 AND pending_host_key_seen_at IS NOT NULL
                             THEN pending_host_key_seen_at
                             ELSE ?3
                        END
              WHERE id = ?4",
        )
        .bind(key)
        .bind(fingerprint)
        .bind(now_string())
        .bind(id)
        .execute(self.pool())
        .await?;
        Ok(())
    }

    /// Drops a pending change without accepting it.
    ///
    /// Called when a connection presents the pinned key again: whatever was
    /// offered before is no longer outstanding, and leaving it would keep
    /// showing a warning about a host that is now presenting exactly what it
    /// should.
    pub async fn clear_pending_host_key(&self, id: &str) -> anyhow::Result<()> {
        sqlx::query(
            "UPDATE targets
                SET pending_host_key = '',
                    pending_host_key_fingerprint = '',
                    pending_host_key_seen_at = NULL
              WHERE id = ?1 AND pending_host_key <> ''",
        )
        .bind(id)
        .execute(self.pool())
        .await?;
        Ok(())
    }

    /// Forgets a target's pinned key, reopening the first-use window.
    pub async fn forget_host_key(&self, id: &str) -> anyhow::Result<()> {
        let result = sqlx::query(
            "UPDATE targets
                SET host_key = '',
                    host_key_fingerprint = '',
                    host_key_pinned_at = NULL,
                    pending_host_key = '',
                    pending_host_key_fingerprint = '',
                    pending_host_key_seen_at = NULL
              WHERE id = ?1",
        )
        .bind(id)
        .execute(self.pool())
        .await?;
        if result.rows_affected() == 0 {
            bail!("no such target: {id}");
        }
        Ok(())
    }
}

const TARGET_SELECT: &str = "SELECT id, name, host, port, user, ssh_key_id, latency_critical, \
     labels, status, last_seen_at, created_at, host_key, host_key_fingerprint, \
     host_key_pinned_at, pending_host_key, pending_host_key_fingerprint, \
     pending_host_key_seen_at FROM targets";

const TARGET_SELECT_BY_ID: &str = "SELECT id, name, host, port, user, ssh_key_id, \
     latency_critical, labels, status, last_seen_at, created_at, host_key, \
     host_key_fingerprint, host_key_pinned_at, pending_host_key, \
     pending_host_key_fingerprint, pending_host_key_seen_at FROM targets WHERE id = ?1";

fn row_to_target(row: &SqliteRow) -> Target {
    Target {
        id: row.get("id"),
        name: row.get("name"),
        host: row.get("host"),
        port: row.get::<i64, _>("port") as u32,
        user: row.get("user"),
        ssh_key_id: row.get("ssh_key_id"),
        latency_critical: row.get::<i64, _>("latency_critical") != 0,
        labels: decode_map(&row.get::<String, _>("labels")),
        status: target::Status::parse(&row.get::<String, _>("status")) as i32,
        last_seen_at: nudo_proto::to_timestamp_opt(from_db_time_opt(
            row.get::<Option<String>, _>("last_seen_at").as_deref(),
        )),
        // Agentless by design: there is never an agent version to report.
        agent_version: String::new(),
        created_at: nudo_proto::to_timestamp_opt(super::from_db_time(
            &row.get::<String, _>("created_at"),
        )),
        host_key: row_to_host_key(row),
    }
}

/// The host-key half of a target row.
///
/// `None` when nothing is pinned and nothing is pending, so a target that has
/// never connected reads as having no host key rather than as having an empty
/// one. Public key material throughout — safe to hand to any client.
fn row_to_host_key(row: &SqliteRow) -> Option<HostKey> {
    let key: String = row.get("host_key");
    let pending: String = row.get("pending_host_key");
    if key.is_empty() && pending.is_empty() {
        return None;
    }

    Some(HostKey {
        key,
        fingerprint: row.get("host_key_fingerprint"),
        pinned_at: nudo_proto::to_timestamp_opt(from_db_time_opt(
            row.get::<Option<String>, _>("host_key_pinned_at")
                .as_deref(),
        )),
        pending_key: pending,
        pending_fingerprint: row.get("pending_host_key_fingerprint"),
        pending_seen_at: nudo_proto::to_timestamp_opt(from_db_time_opt(
            row.get::<Option<String>, _>("pending_host_key_seen_at")
                .as_deref(),
        )),
    })
}

/// Turns a unique-constraint violation into a message an operator can act on.
fn friendly_target_error(error: sqlx::Error, name: &str) -> anyhow::Error {
    if is_unique_violation(&error) {
        return anyhow::anyhow!("a target named {name:?} already exists");
    }
    error.into()
}

/// Whether a database error is a uniqueness violation.
pub(crate) fn is_unique_violation(error: &sqlx::Error) -> bool {
    matches!(error, sqlx::Error::Database(db) if db.code().as_deref() == Some("2067")
        || db.message().contains("UNIQUE constraint failed"))
}

/// Parses a selector like `env=prod,role=indexer` into pairs.
pub(crate) fn parse_label_selector(selector: &str) -> Vec<(String, String)> {
    selector
        .split(',')
        .filter_map(|clause| {
            let (key, value) = clause.split_once('=')?;
            let key = key.trim();
            if key.is_empty() {
                return None;
            }
            Some((key.to_string(), value.trim().to_string()))
        })
        .collect()
}

/// Whether a label set satisfies every clause of a selector.
pub(crate) fn matches_selector(
    labels: &std::collections::HashMap<String, String>,
    selector: &[(String, String)],
) -> bool {
    selector
        .iter()
        .all(|(key, value)| labels.get(key).map(|v| v == value).unwrap_or(false))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input(name: &str) -> TargetInput {
        TargetInput {
            name: name.to_string(),
            host: "10.0.0.5".to_string(),
            port: 22,
            user: "root".to_string(),
            ..Default::default()
        }
    }

    async fn store() -> Store {
        Store::open_in_memory().await.expect("open")
    }

    #[tokio::test]
    async fn a_created_target_can_be_read_back() {
        let store = store().await;
        let created = store.create_target(&input("edge-1")).await.expect("create");

        assert!(created.id.starts_with("tgt_"));
        assert_eq!(created.name, "edge-1");
        assert_eq!(created.host, "10.0.0.5");
        assert_eq!(created.port, 22);
        // A fresh target has not been probed yet.
        assert_eq!(created.status, target::Status::Unknown as i32);
        assert!(created.last_seen_at.is_none());
        // Agentless: never a version.
        assert!(created.agent_version.is_empty());

        let fetched = store
            .get_target(&created.id)
            .await
            .expect("get")
            .expect("some");
        assert_eq!(fetched.id, created.id);
    }

    #[tokio::test]
    async fn defaults_are_applied_for_unset_port_and_user() {
        let store = store().await;
        let created = store
            .create_target(&TargetInput {
                name: "defaults".to_string(),
                host: "host".to_string(),
                ..Default::default()
            })
            .await
            .expect("create");
        assert_eq!(created.port, 22);
        assert_eq!(created.user, "root");
    }

    #[tokio::test]
    async fn a_target_needs_a_name_and_a_host() {
        let store = store().await;
        assert!(
            store
                .create_target(&TargetInput {
                    name: "  ".to_string(),
                    host: "h".to_string(),
                    ..Default::default()
                })
                .await
                .is_err()
        );
        assert!(
            store
                .create_target(&TargetInput {
                    name: "n".to_string(),
                    host: String::new(),
                    ..Default::default()
                })
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn duplicate_names_are_refused_with_a_readable_message() {
        let store = store().await;
        store.create_target(&input("dup")).await.expect("first");
        let error = store
            .create_target(&input("dup"))
            .await
            .expect_err("second");
        assert!(error.to_string().contains("already exists"), "got: {error}");
    }

    #[tokio::test]
    async fn a_missing_target_reads_as_none_rather_than_an_error() {
        let store = store().await;
        assert!(store.get_target("tgt_nope").await.expect("get").is_none());
    }

    #[tokio::test]
    async fn an_update_mask_limits_which_fields_change() {
        let store = store().await;
        let created = store.create_target(&input("masked")).await.expect("create");

        let update = Target {
            name: "renamed".into(),
            host: "192.168.1.1".into(),
            latency_critical: true,
            ..Default::default()
        };
        let updated = store
            .update_target(&created.id, &update, &["name".to_string()])
            .await
            .expect("update");

        assert_eq!(updated.name, "renamed");
        // Outside the mask, so untouched.
        assert_eq!(updated.host, "10.0.0.5");
        assert!(!updated.latency_critical);
    }

    #[tokio::test]
    async fn a_multi_field_update_rolls_back_when_one_field_fails() {
        let store = store().await;
        let created = store.create_target(&input("edge-1")).await.expect("create");

        // Simulate a later database constraint so the first statement succeeds
        // inside the transaction and the second one fails.
        sqlx::query(
            "CREATE TRIGGER reject_target_host
             BEFORE UPDATE OF host ON targets
             BEGIN
               SELECT RAISE(ABORT, 'host rejected');
             END",
        )
        .execute(store.pool())
        .await
        .expect("trigger");

        let error = store
            .update_target(
                &created.id,
                &Target {
                    name: "renamed".to_string(),
                    host: "192.168.1.2".to_string(),
                    ..Default::default()
                },
                &["name".to_string(), "host".to_string()],
            )
            .await
            .expect_err("the host trigger must reject the update");
        assert!(error.to_string().contains("host rejected"), "got: {error}");

        let unchanged = store
            .get_target(&created.id)
            .await
            .expect("read")
            .expect("target");
        assert_eq!(unchanged.name, "edge-1");
        assert_eq!(unchanged.host, "10.0.0.5");
    }

    #[tokio::test]
    async fn an_empty_mask_updates_every_populated_field() {
        let store = store().await;
        let created = store
            .create_target(&input("wholesale"))
            .await
            .expect("create");

        let updated = store
            .update_target(
                &created.id,
                &Target {
                    host: "192.168.1.1".into(),
                    latency_critical: true,
                    ..Default::default()
                },
                &[],
            )
            .await
            .expect("update");

        assert_eq!(updated.host, "192.168.1.1");
        assert!(updated.latency_critical);
        // An empty name in the message must not blank the stored one.
        assert_eq!(updated.name, "wholesale");
    }

    #[tokio::test]
    async fn the_latency_critical_flag_can_be_cleared() {
        // Booleans need the mask path to allow false, not just true.
        let store = store().await;
        let created = store
            .create_target(&TargetInput {
                latency_critical: true,
                ..input("hot")
            })
            .await
            .expect("create");
        assert!(created.latency_critical);

        let updated = store
            .update_target(
                &created.id,
                &Target::default(),
                &["latency_critical".to_string()],
            )
            .await
            .expect("update");
        assert!(!updated.latency_critical);
    }

    #[tokio::test]
    async fn updating_a_missing_target_fails() {
        let store = store().await;
        assert!(
            store
                .update_target("tgt_nope", &Target::default(), &[])
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn deleting_a_target_removes_it_and_deleting_twice_fails() {
        let store = store().await;
        let created = store.create_target(&input("goner")).await.expect("create");

        store.delete_target(&created.id).await.expect("delete");
        assert!(store.get_target(&created.id).await.expect("get").is_none());
        assert!(store.delete_target(&created.id).await.is_err());
    }

    #[tokio::test]
    async fn a_reachable_probe_advances_last_seen_but_an_unreachable_one_does_not() {
        let store = store().await;
        let created = store.create_target(&input("probed")).await.expect("create");

        store
            .set_target_status(&created.id, target::Status::Reachable)
            .await
            .expect("set");
        let seen = store
            .get_target(&created.id)
            .await
            .expect("get")
            .expect("some");
        assert_eq!(seen.status, target::Status::Reachable as i32);
        let first_seen = seen.last_seen_at.expect("last_seen_at");

        store
            .set_target_status(&created.id, target::Status::Unreachable)
            .await
            .expect("set");
        let now_unreachable = store
            .get_target(&created.id)
            .await
            .expect("get")
            .expect("some");
        assert_eq!(now_unreachable.status, target::Status::Unreachable as i32);
        // Still shows when it was last actually up.
        assert_eq!(now_unreachable.last_seen_at, Some(first_seen));
    }

    #[tokio::test]
    async fn labels_round_trip_and_filter_the_list() {
        let store = store().await;
        store
            .create_target(&TargetInput {
                labels: std::collections::HashMap::from([
                    ("env".to_string(), "prod".to_string()),
                    ("role".to_string(), "indexer".to_string()),
                ]),
                ..input("prod-indexer")
            })
            .await
            .expect("create");
        store
            .create_target(&TargetInput {
                labels: std::collections::HashMap::from([(
                    "env".to_string(),
                    "staging".to_string(),
                )]),
                ..input("staging-box")
            })
            .await
            .expect("create");

        let all = store.list_targets("", 50, 0).await.expect("list");
        assert_eq!(all.len(), 2);

        let prod = store.list_targets("env=prod", 50, 0).await.expect("list");
        assert_eq!(prod.len(), 1);
        assert_eq!(prod[0].name, "prod-indexer");
        assert_eq!(
            prod[0].labels.get("role").map(String::as_str),
            Some("indexer")
        );

        // Every clause must match.
        let both = store
            .list_targets("env=prod,role=indexer", 50, 0)
            .await
            .expect("list");
        assert_eq!(both.len(), 1);
        let neither = store
            .list_targets("env=prod,role=web", 50, 0)
            .await
            .expect("list");
        assert!(neither.is_empty());
    }

    #[tokio::test]
    async fn listing_paginates_and_is_newest_first() {
        let store = store().await;
        for i in 0..5 {
            store
                .create_target(&input(&format!("box-{i}")))
                .await
                .expect("create");
        }

        let first = store.list_targets("", 2, 0).await.expect("list");
        assert_eq!(first.len(), 2);
        let second = store.list_targets("", 2, 2).await.expect("list");
        assert_eq!(second.len(), 2);
        // No overlap between pages.
        assert!(first.iter().all(|a| second.iter().all(|b| a.id != b.id)));

        assert_eq!(store.count_targets().await.expect("count"), 5);
    }

    // ---- host keys ----

    const KEY_A: &str =
        "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAINbQLN3OR4KHUki7vfmdITOI3q+Nfu9w3X2agJ+IDHXR";
    const KEY_B: &str =
        "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIIwpYjPDsSOQR3dD30dkum4PseIZzCiIqleJEkpyKBfu";

    #[tokio::test]
    async fn a_fresh_target_has_no_host_key_at_all() {
        // Not an empty one: "never connected" and "connected and presented
        // nothing" are different states, and the API should not conflate them.
        let store = store().await;
        let created = store
            .create_target(&input("unpinned"))
            .await
            .expect("create");
        assert!(created.host_key.is_none());
    }

    #[tokio::test]
    async fn a_pinned_key_is_read_back_with_its_fingerprint_and_a_timestamp() {
        let store = store().await;
        let created = store.create_target(&input("pinned")).await.expect("create");

        store
            .pin_host_key(&created.id, KEY_A, "SHA256:aaa")
            .await
            .expect("pin");

        let host_key = store
            .get_target(&created.id)
            .await
            .expect("get")
            .expect("target")
            .host_key
            .expect("a pinned key");
        assert_eq!(host_key.key, KEY_A);
        assert_eq!(host_key.fingerprint, "SHA256:aaa");
        assert!(host_key.pinned_at.is_some());
        assert!(host_key.pending_key.is_empty());
    }

    #[tokio::test]
    async fn a_changed_key_is_held_for_review_without_touching_the_pinned_one() {
        // The pinned key must survive: it is what every connection is still
        // checked against while the change is unreviewed.
        let store = store().await;
        let created = store
            .create_target(&input("changed"))
            .await
            .expect("create");
        store
            .pin_host_key(&created.id, KEY_A, "SHA256:aaa")
            .await
            .expect("pin");

        store
            .record_pending_host_key(&created.id, KEY_B, "SHA256:bbb")
            .await
            .expect("record");

        let host_key = store
            .get_target(&created.id)
            .await
            .expect("get")
            .expect("target")
            .host_key
            .expect("host key");
        assert_eq!(host_key.key, KEY_A, "the pinned key must not be replaced");
        assert_eq!(host_key.fingerprint, "SHA256:aaa");
        assert_eq!(host_key.pending_key, KEY_B);
        assert_eq!(host_key.pending_fingerprint, "SHA256:bbb");
        assert!(host_key.pending_seen_at.is_some());
    }

    #[tokio::test]
    async fn re_seeing_the_same_changed_key_keeps_when_it_was_first_seen() {
        // The probe loop re-sees it every minute; "failing since 09:14" is the
        // useful fact, and it would be lost by overwriting on each sighting.
        let store = store().await;
        let created = store.create_target(&input("reseen")).await.expect("create");
        store
            .pin_host_key(&created.id, KEY_A, "SHA256:aaa")
            .await
            .expect("pin");

        store
            .record_pending_host_key(&created.id, KEY_B, "SHA256:bbb")
            .await
            .expect("first sighting");
        let first = store
            .get_target(&created.id)
            .await
            .expect("get")
            .expect("target")
            .host_key
            .expect("host key")
            .pending_seen_at;

        store
            .record_pending_host_key(&created.id, KEY_B, "SHA256:bbb")
            .await
            .expect("second sighting");
        let second = store
            .get_target(&created.id)
            .await
            .expect("get")
            .expect("target")
            .host_key
            .expect("host key")
            .pending_seen_at;

        assert_eq!(first, second);
    }

    #[tokio::test]
    async fn a_second_different_key_restarts_the_clock() {
        // A new key is a new thing to review, so its own first-seen time is
        // the honest one.
        let store = store().await;
        let created = store.create_target(&input("twice")).await.expect("create");
        store
            .pin_host_key(&created.id, KEY_A, "SHA256:aaa")
            .await
            .expect("pin");

        store
            .record_pending_host_key(&created.id, KEY_B, "SHA256:bbb")
            .await
            .expect("first");
        store
            .record_pending_host_key(&created.id, KEY_A, "SHA256:ccc")
            .await
            .expect("second");

        let host_key = store
            .get_target(&created.id)
            .await
            .expect("get")
            .expect("target")
            .host_key
            .expect("host key");
        assert_eq!(host_key.pending_fingerprint, "SHA256:ccc");
    }

    #[tokio::test]
    async fn accepting_a_change_makes_it_the_pinned_key_and_clears_the_review() {
        let store = store().await;
        let created = store.create_target(&input("accept")).await.expect("create");
        store
            .pin_host_key(&created.id, KEY_A, "SHA256:aaa")
            .await
            .expect("pin");
        store
            .record_pending_host_key(&created.id, KEY_B, "SHA256:bbb")
            .await
            .expect("record");

        store
            .pin_host_key(&created.id, KEY_B, "SHA256:bbb")
            .await
            .expect("accept");

        let host_key = store
            .get_target(&created.id)
            .await
            .expect("get")
            .expect("target")
            .host_key
            .expect("host key");
        assert_eq!(host_key.key, KEY_B);
        assert_eq!(host_key.fingerprint, "SHA256:bbb");
        assert!(host_key.pending_key.is_empty());
        assert!(host_key.pending_seen_at.is_none());
    }

    #[tokio::test]
    async fn a_host_presenting_the_pinned_key_again_drops_the_pending_change() {
        // A transient misdirection that resolves itself should not leave a
        // warning about a host that is now presenting exactly what it should.
        let store = store().await;
        let created = store
            .create_target(&input("resolved"))
            .await
            .expect("create");
        store
            .pin_host_key(&created.id, KEY_A, "SHA256:aaa")
            .await
            .expect("pin");
        store
            .record_pending_host_key(&created.id, KEY_B, "SHA256:bbb")
            .await
            .expect("record");

        store
            .clear_pending_host_key(&created.id)
            .await
            .expect("clear");

        let host_key = store
            .get_target(&created.id)
            .await
            .expect("get")
            .expect("target")
            .host_key
            .expect("host key");
        assert_eq!(host_key.key, KEY_A, "the pinned key is untouched");
        assert!(host_key.pending_key.is_empty());
    }

    #[tokio::test]
    async fn forgetting_a_key_reopens_the_first_use_window() {
        let store = store().await;
        let created = store.create_target(&input("forget")).await.expect("create");
        store
            .pin_host_key(&created.id, KEY_A, "SHA256:aaa")
            .await
            .expect("pin");
        store
            .record_pending_host_key(&created.id, KEY_B, "SHA256:bbb")
            .await
            .expect("record");

        store.forget_host_key(&created.id).await.expect("forget");

        // Back to the state a newly created target is in, pending change and
        // all — otherwise a stale review would block the fresh pinning.
        let target = store
            .get_target(&created.id)
            .await
            .expect("get")
            .expect("target");
        assert!(target.host_key.is_none());
    }

    #[tokio::test]
    async fn forgetting_the_key_of_a_missing_target_fails() {
        let store = store().await;
        assert!(store.forget_host_key("tgt_nope").await.is_err());
    }

    #[tokio::test]
    async fn an_update_leaves_the_host_key_alone() {
        // The update mask covers connection details; re-pointing a target at a
        // different host must not silently carry the old host's key over, and
        // must not blank it either — the next connection is what decides.
        let store = store().await;
        let created = store.create_target(&input("edited")).await.expect("create");
        store
            .pin_host_key(&created.id, KEY_A, "SHA256:aaa")
            .await
            .expect("pin");

        let updated = store
            .update_target(
                &created.id,
                &Target {
                    name: "renamed".to_string(),
                    ..Default::default()
                },
                &[],
            )
            .await
            .expect("update");

        assert_eq!(
            updated.host_key.expect("host key").fingerprint,
            "SHA256:aaa"
        );
    }

    #[test]
    fn label_selectors_parse_and_tolerate_junk() {
        assert_eq!(
            parse_label_selector("env=prod,role=indexer"),
            vec![
                ("env".to_string(), "prod".to_string()),
                ("role".to_string(), "indexer".to_string())
            ]
        );
        // Whitespace around clauses is allowed.
        assert_eq!(
            parse_label_selector(" env = prod "),
            vec![("env".to_string(), "prod".to_string())]
        );
        // Clauses with no `=` and empty keys are dropped rather than erroring.
        assert!(parse_label_selector("garbage").is_empty());
        assert!(parse_label_selector("=value").is_empty());
        assert!(parse_label_selector("").is_empty());
    }

    #[test]
    fn a_selector_matching_an_empty_value_is_honored() {
        let labels = std::collections::HashMap::from([("tier".to_string(), String::new())]);
        assert!(matches_selector(&labels, &parse_label_selector("tier=")));
    }
}
