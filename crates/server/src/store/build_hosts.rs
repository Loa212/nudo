//! Build-host persistence — the machines that build, when the control plane
//! does not.
//!
//! Deliberately a separate table from `targets` rather than a flag on it. The
//! two share reachability, an SSH user and a key, and nothing else: a build
//! host has no release root, no unit and nothing deployed to it. Keeping them
//! apart is what makes "a build host is never deployed to" structural instead
//! of a rule someone has to remember.

use anyhow::bail;
use nudo_proto::{BuildHost, HostKey, build_host};
use sqlx::Row;
use sqlx::sqlite::SqliteRow;

use super::targets::{matches_selector, parse_label_selector};
use super::{Store, decode_map, encode_map, from_db_time_opt, new_id, now_string};
// As in `targets`, the SQL below is composed only from `const` fragments in
// this file plus bound parameters; no caller-supplied value is interpolated.
use sqlx::AssertSqlSafe;

/// Where builds go when a build host does not say otherwise.
pub const DEFAULT_WORKSPACE_ROOT: &str = "/var/lib/nudo/builds";

/// Fields a client may set when creating or updating a build host.
#[derive(Debug, Clone, Default)]
pub struct BuildHostInput {
    pub name: String,
    pub host: String,
    pub port: u32,
    pub user: String,
    pub ssh_key_id: String,
    pub workspace_root: String,
    pub latency_critical: bool,
    pub labels: std::collections::HashMap<String, String>,
}

impl Store {
    pub async fn create_build_host(&self, input: &BuildHostInput) -> anyhow::Result<BuildHost> {
        let name = input.name.trim();
        let host = input.host.trim();
        if name.is_empty() {
            bail!("a build host needs a name");
        }
        if host.is_empty() {
            bail!("a build host needs a host");
        }

        let id = new_id("bh");
        let created_at = now_string();
        // Port 0 means the client did not set one, not "port zero".
        let port = if input.port == 0 { 22 } else { input.port };
        let user = if input.user.trim().is_empty() {
            "root"
        } else {
            input.user.trim()
        };
        let workspace_root = normalize_workspace_root(&input.workspace_root);

        sqlx::query(
            "INSERT INTO build_hosts
               (id, name, host, port, user, ssh_key_id, workspace_root,
                latency_critical, labels, status, last_seen_at, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, 'unknown', NULL, ?10)",
        )
        .bind(&id)
        .bind(name)
        .bind(host)
        .bind(port)
        .bind(user)
        .bind(input.ssh_key_id.trim())
        .bind(&workspace_root)
        .bind(input.latency_critical as i64)
        .bind(encode_map(&input.labels))
        .bind(&created_at)
        .execute(self.pool())
        .await
        .map_err(|e| friendly_build_host_error(e, name))?;

        self.get_build_host(&id)
            .await?
            .ok_or_else(|| anyhow::anyhow!("build host vanished immediately after creation"))
    }

    pub async fn get_build_host(&self, id: &str) -> anyhow::Result<Option<BuildHost>> {
        let row = sqlx::query(BUILD_HOST_SELECT_BY_ID)
            .bind(id)
            .fetch_optional(self.pool())
            .await?;
        Ok(row.as_ref().map(row_to_build_host))
    }

    /// Lists build hosts, newest first, optionally filtered by a label selector.
    ///
    /// Matched in Rust rather than SQL for the same reason targets are: labels
    /// live in a JSON column and the table is small.
    pub async fn list_build_hosts(
        &self,
        label_selector: &str,
        limit: i64,
        offset: i64,
    ) -> anyhow::Result<Vec<BuildHost>> {
        let selector = parse_label_selector(label_selector);

        if selector.is_empty() {
            let rows = sqlx::query(AssertSqlSafe(format!(
                "{BUILD_HOST_SELECT} ORDER BY created_at DESC, id DESC LIMIT ?1 OFFSET ?2"
            )))
            .bind(limit)
            .bind(offset)
            .fetch_all(self.pool())
            .await?;
            return Ok(rows.iter().map(row_to_build_host).collect());
        }

        let rows = sqlx::query(AssertSqlSafe(format!(
            "{BUILD_HOST_SELECT} ORDER BY created_at DESC, id DESC"
        )))
        .fetch_all(self.pool())
        .await?;

        Ok(rows
            .iter()
            .map(row_to_build_host)
            .filter(|h| matches_selector(&h.labels, &selector))
            .skip(offset.max(0) as usize)
            .take(limit.max(0) as usize)
            .collect())
    }

    /// Total number of build hosts, for the dashboard's counts.
    pub async fn count_build_hosts(&self) -> anyhow::Result<i64> {
        let (count,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM build_hosts")
            .fetch_one(self.pool())
            .await?;
        Ok(count)
    }

    /// Applies a field mask to a build host. Only named fields are written.
    pub async fn update_build_host(
        &self,
        id: &str,
        build_host: &BuildHost,
        update_mask: &[String],
    ) -> anyhow::Result<BuildHost> {
        if self.get_build_host(id).await?.is_none() {
            bail!("no such build host: {id}");
        }

        // An empty mask means "everything the message carries".
        let touch = |field: &str| update_mask.is_empty() || update_mask.iter().any(|m| m == field);
        let mut transaction = self.pool().begin().await?;

        if touch("name") && !build_host.name.trim().is_empty() {
            sqlx::query("UPDATE build_hosts SET name = ?1 WHERE id = ?2")
                .bind(build_host.name.trim())
                .bind(id)
                .execute(&mut *transaction)
                .await
                .map_err(|e| friendly_build_host_error(e, build_host.name.trim()))?;
        }
        if touch("host") && !build_host.host.trim().is_empty() {
            sqlx::query("UPDATE build_hosts SET host = ?1 WHERE id = ?2")
                .bind(build_host.host.trim())
                .bind(id)
                .execute(&mut *transaction)
                .await?;
        }
        if touch("port") && build_host.port != 0 {
            sqlx::query("UPDATE build_hosts SET port = ?1 WHERE id = ?2")
                .bind(build_host.port)
                .bind(id)
                .execute(&mut *transaction)
                .await?;
        }
        if touch("user") && !build_host.user.trim().is_empty() {
            sqlx::query("UPDATE build_hosts SET user = ?1 WHERE id = ?2")
                .bind(build_host.user.trim())
                .bind(id)
                .execute(&mut *transaction)
                .await?;
        }
        if touch("ssh_key_id") {
            sqlx::query("UPDATE build_hosts SET ssh_key_id = ?1 WHERE id = ?2")
                .bind(build_host.ssh_key_id.trim())
                .bind(id)
                .execute(&mut *transaction)
                .await?;
        }
        if touch("workspace_root") {
            // Blanking the field resets to the default rather than leaving a
            // build host with nowhere to work.
            sqlx::query("UPDATE build_hosts SET workspace_root = ?1 WHERE id = ?2")
                .bind(normalize_workspace_root(&build_host.workspace_root))
                .bind(id)
                .execute(&mut *transaction)
                .await?;
        }
        if touch("latency_critical") {
            sqlx::query("UPDATE build_hosts SET latency_critical = ?1 WHERE id = ?2")
                .bind(build_host.latency_critical as i64)
                .bind(id)
                .execute(&mut *transaction)
                .await?;
        }
        if touch("labels") {
            sqlx::query("UPDATE build_hosts SET labels = ?1 WHERE id = ?2")
                .bind(encode_map(&build_host.labels))
                .bind(id)
                .execute(&mut *transaction)
                .await?;
        }

        transaction.commit().await?;
        self.get_build_host(id)
            .await?
            .ok_or_else(|| anyhow::anyhow!("build host {id} vanished during update"))
    }

    pub async fn delete_build_host(&self, id: &str) -> anyhow::Result<()> {
        let result = sqlx::query("DELETE FROM build_hosts WHERE id = ?1")
            .bind(id)
            .execute(self.pool())
            .await?;
        if result.rows_affected() == 0 {
            bail!("no such build host: {id}");
        }
        Ok(())
    }

    /// Services still pointing at this build host, by name.
    ///
    /// There is no foreign key from `services.git_build_host_id`: a deleted
    /// build host must leave those services failing loudly at deploy time
    /// rather than being silently reset to the default, which would move a
    /// build nobody asked to move. This is what lets a caller warn first.
    pub async fn services_using_build_host(&self, id: &str) -> anyhow::Result<Vec<String>> {
        let rows: Vec<(String,)> =
            sqlx::query_as("SELECT name FROM services WHERE git_build_host_id = ?1 ORDER BY name")
                .bind(id)
                .fetch_all(self.pool())
                .await?;
        Ok(rows.into_iter().map(|(name,)| name).collect())
    }

    /// Records the outcome of a reachability probe.
    pub async fn set_build_host_status(
        &self,
        id: &str,
        status: build_host::Status,
    ) -> anyhow::Result<()> {
        // As for targets, `last_seen_at` means "last confirmed reachable" and
        // is only advanced on success.
        let last_seen = if status == build_host::Status::Reachable {
            Some(now_string())
        } else {
            None
        };

        match last_seen {
            Some(seen) => {
                sqlx::query("UPDATE build_hosts SET status = ?1, last_seen_at = ?2 WHERE id = ?3")
                    .bind(status.as_str())
                    .bind(seen)
                    .bind(id)
                    .execute(self.pool())
                    .await?;
            }
            None => {
                sqlx::query("UPDATE build_hosts SET status = ?1 WHERE id = ?2")
                    .bind(status.as_str())
                    .bind(id)
                    .execute(self.pool())
                    .await?;
            }
        }
        Ok(())
    }

    /// Pins a build host's key, clearing any pending change.
    pub async fn pin_build_host_key(
        &self,
        id: &str,
        key: &str,
        fingerprint: &str,
    ) -> anyhow::Result<()> {
        sqlx::query(
            "UPDATE build_hosts
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
    /// The first sighting's timestamp survives a repeat, so "failing since
    /// 09:14" is not reset by a probe loop.
    pub async fn record_pending_build_host_key(
        &self,
        id: &str,
        key: &str,
        fingerprint: &str,
    ) -> anyhow::Result<()> {
        sqlx::query(
            "UPDATE build_hosts
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
    pub async fn clear_pending_build_host_key(&self, id: &str) -> anyhow::Result<()> {
        sqlx::query(
            "UPDATE build_hosts
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

    /// Forgets a build host's pinned key, reopening the first-use window.
    pub async fn forget_build_host_key(&self, id: &str) -> anyhow::Result<()> {
        let result = sqlx::query(
            "UPDATE build_hosts
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
            bail!("no such build host: {id}");
        }
        Ok(())
    }

    // ---- instance default ----

    /// The instance-wide default build host id.
    ///
    /// Empty — the initial state, and the state of every instance that upgrades
    /// and configures nothing — means the control plane.
    pub async fn default_build_host_id(&self) -> anyhow::Result<String> {
        let row: Option<(String,)> =
            sqlx::query_as("SELECT build_host_id FROM build_defaults WHERE id = 1")
                .fetch_optional(self.pool())
                .await?;
        Ok(row.map(|(id,)| id).unwrap_or_default())
    }

    /// Sets the instance-wide default build host.
    ///
    /// An empty id, or the `local` sentinel, returns the instance to building on
    /// the control plane.
    pub async fn set_default_build_host_id(&self, build_host_id: &str) -> anyhow::Result<()> {
        let id = build_host_id.trim();
        // A default pointing at a build host that does not exist would fail
        // every git-backed deploy on the instance at once, so it is refused
        // here rather than discovered at deploy time.
        if !id.is_empty()
            && id != nudo_proto::LOCAL_BUILD_HOST_ID
            && self.get_build_host(id).await?.is_none()
        {
            bail!("no such build host: {id}");
        }

        sqlx::query(
            "INSERT INTO build_defaults (id, build_host_id) VALUES (1, ?1)
             ON CONFLICT (id) DO UPDATE SET build_host_id = ?1",
        )
        .bind(id)
        .execute(self.pool())
        .await?;
        Ok(())
    }
}

/// Every column [`row_to_build_host`] reads, asserted against both selects.
#[cfg(test)]
const BUILD_HOST_COLUMNS: &str = "id, name, host, port, user, ssh_key_id, workspace_root, \
     latency_critical, labels, status, last_seen_at, created_at, host_key, \
     host_key_fingerprint, host_key_pinned_at, pending_host_key, \
     pending_host_key_fingerprint, pending_host_key_seen_at";

const BUILD_HOST_SELECT: &str = "SELECT id, name, host, port, user, ssh_key_id, workspace_root, \
     latency_critical, labels, status, last_seen_at, created_at, host_key, \
     host_key_fingerprint, host_key_pinned_at, pending_host_key, \
     pending_host_key_fingerprint, pending_host_key_seen_at FROM build_hosts";

const BUILD_HOST_SELECT_BY_ID: &str = "SELECT id, name, host, port, user, ssh_key_id, \
     workspace_root, latency_critical, labels, status, last_seen_at, created_at, \
     host_key, host_key_fingerprint, host_key_pinned_at, pending_host_key, \
     pending_host_key_fingerprint, pending_host_key_seen_at FROM build_hosts WHERE id = ?1";

fn row_to_build_host(row: &SqliteRow) -> BuildHost {
    BuildHost {
        id: row.get("id"),
        name: row.get("name"),
        host: row.get("host"),
        port: row.get::<i64, _>("port") as u32,
        user: row.get("user"),
        ssh_key_id: row.get("ssh_key_id"),
        workspace_root: row.get("workspace_root"),
        latency_critical: row.get::<i64, _>("latency_critical") != 0,
        labels: decode_map(&row.get::<String, _>("labels")),
        status: build_host::Status::parse(&row.get::<String, _>("status")) as i32,
        last_seen_at: nudo_proto::to_timestamp_opt(from_db_time_opt(
            row.get::<Option<String>, _>("last_seen_at").as_deref(),
        )),
        created_at: nudo_proto::to_timestamp_opt(super::from_db_time(
            &row.get::<String, _>("created_at"),
        )),
        host_key: row_to_host_key(row),
    }
}

/// The host-key half of a build-host row.
///
/// `None` when nothing is pinned and nothing is pending, so a host that has
/// never connected reads as having no host key rather than an empty one.
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

/// Cleans a workspace root, falling back to the default.
///
/// A relative root would put build trees wherever the SSH session happens to
/// land — usually the build user's home — which is not somewhere a later
/// `rm -rf` of a build directory should be aimed.
fn normalize_workspace_root(raw: &str) -> String {
    let trimmed = raw.trim().trim_end_matches('/');
    if trimmed.is_empty() || !trimmed.starts_with('/') {
        return DEFAULT_WORKSPACE_ROOT.to_string();
    }
    trimmed.to_string()
}

/// Turns a unique-constraint violation into a message an operator can act on.
fn friendly_build_host_error(error: sqlx::Error, name: &str) -> anyhow::Error {
    if super::targets::is_unique_violation(&error) {
        return anyhow::anyhow!("a build host named {name:?} already exists");
    }
    error.into()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input(name: &str) -> BuildHostInput {
        BuildHostInput {
            name: name.to_string(),
            host: "10.0.0.9".to_string(),
            port: 22,
            user: "build".to_string(),
            ..Default::default()
        }
    }

    async fn store() -> Store {
        Store::open_in_memory().await.expect("open")
    }

    #[tokio::test]
    async fn a_created_build_host_can_be_read_back() {
        let store = store().await;
        let created = store
            .create_build_host(&input("builder-1"))
            .await
            .expect("create");

        assert!(created.id.starts_with("bh_"), "got {}", created.id);
        assert_eq!(created.name, "builder-1");
        assert_eq!(created.host, "10.0.0.9");
        assert_eq!(created.user, "build");
        // A fresh build host has not been probed yet.
        assert_eq!(created.status, build_host::Status::Unknown as i32);
        assert!(created.last_seen_at.is_none());
        // And nothing is pinned until it first connects.
        assert!(created.host_key.is_none());
    }

    #[tokio::test]
    async fn a_build_host_gets_a_default_workspace_when_none_is_given() {
        let store = store().await;
        let created = store
            .create_build_host(&input("builder"))
            .await
            .expect("create");
        assert_eq!(created.workspace_root, DEFAULT_WORKSPACE_ROOT);
    }

    #[tokio::test]
    async fn a_relative_workspace_root_is_refused_in_favour_of_the_default() {
        // A relative root resolves against the build user's home, which is not
        // where an `rm -rf` of a build directory should be aimed.
        let store = store().await;
        let created = store
            .create_build_host(&BuildHostInput {
                workspace_root: "builds".to_string(),
                ..input("builder")
            })
            .await
            .expect("create");
        assert_eq!(created.workspace_root, DEFAULT_WORKSPACE_ROOT);
    }

    #[tokio::test]
    async fn a_workspace_root_keeps_its_absolute_path_without_a_trailing_slash() {
        let store = store().await;
        let created = store
            .create_build_host(&BuildHostInput {
                workspace_root: "  /mnt/fast/builds/  ".to_string(),
                ..input("builder")
            })
            .await
            .expect("create");
        assert_eq!(created.workspace_root, "/mnt/fast/builds");
    }

    #[tokio::test]
    async fn a_build_host_needs_a_name_and_a_host() {
        let store = store().await;
        assert!(
            store
                .create_build_host(&BuildHostInput {
                    name: "  ".to_string(),
                    ..input("x")
                })
                .await
                .is_err()
        );
        assert!(
            store
                .create_build_host(&BuildHostInput {
                    host: String::new(),
                    ..input("named")
                })
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn two_build_hosts_cannot_share_a_name() {
        let store = store().await;
        store
            .create_build_host(&input("builder"))
            .await
            .expect("first");

        let error = store
            .create_build_host(&input("builder"))
            .await
            .expect_err("must refuse a duplicate");
        assert!(error.to_string().contains("already exists"), "got: {error}");
    }

    #[tokio::test]
    async fn a_build_host_is_marked_latency_critical_when_asked() {
        // Permitted rather than refused — an operator may have exactly one
        // spare machine — but it has to be recorded for anything to warn.
        let store = store().await;
        let created = store
            .create_build_host(&BuildHostInput {
                latency_critical: true,
                ..input("the-only-spare-box")
            })
            .await
            .expect("create");
        assert!(created.latency_critical);
    }

    #[tokio::test]
    async fn an_update_writes_only_the_masked_fields() {
        let store = store().await;
        let created = store
            .create_build_host(&input("builder"))
            .await
            .expect("create");

        let updated = store
            .update_build_host(
                &created.id,
                &BuildHost {
                    host: "10.0.0.99".to_string(),
                    // Present in the message but absent from the mask, so it
                    // must not be written.
                    name: "renamed".to_string(),
                    ..Default::default()
                },
                &["host".to_string()],
            )
            .await
            .expect("update");

        assert_eq!(updated.host, "10.0.0.99");
        assert_eq!(updated.name, "builder", "an unmasked field was written");
    }

    #[tokio::test]
    async fn build_hosts_are_filtered_by_a_label_selector() {
        let store = store().await;
        store
            .create_build_host(&BuildHostInput {
                labels: std::collections::HashMap::from([(
                    "arch".to_string(),
                    "arm64".to_string(),
                )]),
                ..input("arm-builder")
            })
            .await
            .expect("create");
        store
            .create_build_host(&BuildHostInput {
                labels: std::collections::HashMap::from([(
                    "arch".to_string(),
                    "amd64".to_string(),
                )]),
                ..input("x86-builder")
            })
            .await
            .expect("create");

        let arm = store
            .list_build_hosts("arch=arm64", 50, 0)
            .await
            .expect("list");
        assert_eq!(arm.len(), 1);
        assert_eq!(arm[0].name, "arm-builder");

        assert_eq!(
            store.list_build_hosts("", 50, 0).await.expect("list").len(),
            2
        );
    }

    #[tokio::test]
    async fn a_deleted_build_host_is_gone_and_deleting_it_twice_reports_so() {
        let store = store().await;
        let created = store
            .create_build_host(&input("builder"))
            .await
            .expect("create");

        store.delete_build_host(&created.id).await.expect("delete");
        assert!(
            store
                .get_build_host(&created.id)
                .await
                .expect("get")
                .is_none()
        );
        assert!(store.delete_build_host(&created.id).await.is_err());
    }

    #[tokio::test]
    async fn an_instance_defaults_to_building_on_the_control_plane() {
        // The compatibility promise: an instance that configures nothing keeps
        // building exactly where it built before.
        let store = store().await;
        assert_eq!(store.default_build_host_id().await.expect("default"), "");
    }

    #[tokio::test]
    async fn the_instance_default_can_be_set_and_cleared() {
        let store = store().await;
        let created = store
            .create_build_host(&input("builder"))
            .await
            .expect("create");

        store
            .set_default_build_host_id(&created.id)
            .await
            .expect("set");
        assert_eq!(
            store.default_build_host_id().await.expect("default"),
            created.id
        );

        store.set_default_build_host_id("").await.expect("clear");
        assert_eq!(store.default_build_host_id().await.expect("default"), "");
    }

    #[tokio::test]
    async fn an_instance_default_naming_a_missing_build_host_is_refused() {
        // Otherwise every git-backed deploy on the instance fails at once, and
        // the cause is a setting nobody looked at since.
        let store = store().await;
        let error = store
            .set_default_build_host_id("bh_missing")
            .await
            .expect_err("must refuse");
        assert!(
            error.to_string().contains("no such build host"),
            "got: {error}"
        );
    }

    #[tokio::test]
    async fn the_local_sentinel_is_accepted_as_an_instance_default() {
        let store = store().await;
        store
            .set_default_build_host_id(nudo_proto::LOCAL_BUILD_HOST_ID)
            .await
            .expect("set local");
        assert_eq!(
            store.default_build_host_id().await.expect("default"),
            nudo_proto::LOCAL_BUILD_HOST_ID
        );
    }

    #[tokio::test]
    async fn a_probe_result_advances_last_seen_only_when_reachable() {
        let store = store().await;
        let created = store
            .create_build_host(&input("builder"))
            .await
            .expect("create");

        store
            .set_build_host_status(&created.id, build_host::Status::Unreachable)
            .await
            .expect("status");
        let unreachable = store
            .get_build_host(&created.id)
            .await
            .expect("get")
            .expect("exists");
        assert_eq!(unreachable.status, build_host::Status::Unreachable as i32);
        assert!(
            unreachable.last_seen_at.is_none(),
            "an unreachable probe must not count as having been seen"
        );

        store
            .set_build_host_status(&created.id, build_host::Status::Reachable)
            .await
            .expect("status");
        let reachable = store
            .get_build_host(&created.id)
            .await
            .expect("get")
            .expect("exists");
        assert!(reachable.last_seen_at.is_some());
    }

    #[tokio::test]
    async fn a_host_key_is_pinned_then_matched_then_forgotten() {
        let store = store().await;
        let created = store
            .create_build_host(&input("builder"))
            .await
            .expect("create");

        store
            .pin_build_host_key(&created.id, "ssh-ed25519 AAAA", "SHA256:abc")
            .await
            .expect("pin");
        let pinned = store
            .get_build_host(&created.id)
            .await
            .expect("get")
            .expect("exists")
            .host_key
            .expect("a key is pinned");
        assert_eq!(pinned.key, "ssh-ed25519 AAAA");
        assert_eq!(pinned.fingerprint, "SHA256:abc");
        assert!(pinned.pending_key.is_empty());

        store
            .forget_build_host_key(&created.id)
            .await
            .expect("forget");
        assert!(
            store
                .get_build_host(&created.id)
                .await
                .expect("get")
                .expect("exists")
                .host_key
                .is_none(),
            "forgetting reopens the first-use window"
        );
    }

    #[tokio::test]
    async fn a_changed_host_key_is_held_for_review_rather_than_applied() {
        let store = store().await;
        let created = store
            .create_build_host(&input("builder"))
            .await
            .expect("create");
        store
            .pin_build_host_key(&created.id, "ssh-ed25519 PINNED", "SHA256:pinned")
            .await
            .expect("pin");

        store
            .record_pending_build_host_key(&created.id, "ssh-ed25519 NEW", "SHA256:new")
            .await
            .expect("record");

        let key = store
            .get_build_host(&created.id)
            .await
            .expect("get")
            .expect("exists")
            .host_key
            .expect("host key");
        // The pinned key is untouched; the new one only waits for review.
        assert_eq!(key.key, "ssh-ed25519 PINNED");
        assert_eq!(key.pending_key, "ssh-ed25519 NEW");
        assert_eq!(key.pending_fingerprint, "SHA256:new");

        // A host presenting the pinned key again clears the pending change.
        store
            .clear_pending_build_host_key(&created.id)
            .await
            .expect("clear");
        let cleared = store
            .get_build_host(&created.id)
            .await
            .expect("get")
            .expect("exists")
            .host_key
            .expect("host key");
        assert!(cleared.pending_key.is_empty());
        assert_eq!(cleared.key, "ssh-ed25519 PINNED");
    }

    #[tokio::test]
    async fn re_seeing_the_same_pending_key_keeps_the_first_sighting() {
        // So "failing since 09:14" survives a probe loop that re-sees it every
        // minute.
        let store = store().await;
        let created = store
            .create_build_host(&input("builder"))
            .await
            .expect("create");

        store
            .record_pending_build_host_key(&created.id, "ssh-ed25519 NEW", "SHA256:new")
            .await
            .expect("first");
        let first = store
            .get_build_host(&created.id)
            .await
            .expect("get")
            .expect("exists")
            .host_key
            .expect("key")
            .pending_seen_at;

        store
            .record_pending_build_host_key(&created.id, "ssh-ed25519 NEW", "SHA256:new")
            .await
            .expect("again");
        let second = store
            .get_build_host(&created.id)
            .await
            .expect("get")
            .expect("exists")
            .host_key
            .expect("key")
            .pending_seen_at;

        assert_eq!(first, second);
    }

    #[tokio::test]
    async fn the_services_using_a_build_host_can_be_listed_before_deleting_it() {
        let store = store().await;
        let host = store
            .create_build_host(&input("builder"))
            .await
            .expect("create");
        let target = store
            .create_target(&crate::store::TargetInput {
                name: "edge".to_string(),
                host: "10.0.0.5".to_string(),
                ..Default::default()
            })
            .await
            .expect("target");

        store
            .create_service(&nudo_proto::Service {
                target_id: target.id.clone(),
                name: "bot".to_string(),
                artifact: Some(nudo_proto::ArtifactSource {
                    kind: Some(nudo_proto::artifact_source::Kind::Git(
                        nudo_proto::GitSource {
                            repo: "o/r".to_string(),
                            build_command: "make".to_string(),
                            artifact_path: "bot".to_string(),
                            build_host_id: host.id.clone(),
                            ..Default::default()
                        },
                    )),
                }),
                ..Default::default()
            })
            .await
            .expect("service");

        assert_eq!(
            store
                .services_using_build_host(&host.id)
                .await
                .expect("using"),
            vec!["bot".to_string()]
        );
        // A build host nothing points at has no dependants.
        assert!(
            store
                .services_using_build_host("bh_other")
                .await
                .expect("using")
                .is_empty()
        );
    }

    #[tokio::test]
    async fn a_services_build_host_survives_a_round_trip() {
        let store = store().await;
        let target = store
            .create_target(&crate::store::TargetInput {
                name: "edge".to_string(),
                host: "10.0.0.5".to_string(),
                ..Default::default()
            })
            .await
            .expect("target");

        let service = store
            .create_service(&nudo_proto::Service {
                target_id: target.id,
                name: "bot".to_string(),
                artifact: Some(nudo_proto::ArtifactSource {
                    kind: Some(nudo_proto::artifact_source::Kind::Git(
                        nudo_proto::GitSource {
                            repo: "o/r".to_string(),
                            build_command: "make".to_string(),
                            artifact_path: "bot".to_string(),
                            build_host_id: "bh_gpu".to_string(),
                            ..Default::default()
                        },
                    )),
                }),
                ..Default::default()
            })
            .await
            .expect("service");

        let git = match service.artifact.expect("artifact").kind {
            Some(nudo_proto::artifact_source::Kind::Git(git)) => git,
            other => panic!("expected a git source, got {other:?}"),
        };
        assert_eq!(git.build_host_id, "bh_gpu");
    }

    #[test]
    fn the_select_lists_every_column_the_row_mapper_reads() {
        // A column added to one and not the other is a runtime failure in a
        // query rather than a compile error, so it is asserted here.
        for column in BUILD_HOST_COLUMNS.split(',').map(str::trim) {
            assert!(
                BUILD_HOST_SELECT.contains(column),
                "{column} is missing from the list select"
            );
            assert!(
                BUILD_HOST_SELECT_BY_ID.contains(column),
                "{column} is missing from the by-id select"
            );
        }
    }
}
