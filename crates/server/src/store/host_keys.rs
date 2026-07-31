//! Pinned SSH host keys, for every table that has them.
//!
//! `targets` and `build_hosts` carry the same six host-key columns with the
//! same meaning — migration 0007 says so in as many words ("identical in shape
//! and meaning to the columns added to `targets` in 0006"). The tables stay
//! apart because a build host is never deployed to, but the trust-on-first-use
//! state machine written over those columns is one thing, so it is written
//! once here rather than once per table.
//!
//! Keeping it in one place is what makes "a host presenting the pinned key
//! clears a pending change" true of build hosts as well as targets, instead of
//! true of whichever copy was edited last.

use nudo_proto::HostKey;
use sqlx::AssertSqlSafe;
use sqlx::Row;
use sqlx::sqlite::SqliteRow;

use super::{Store, from_db_time_opt, now_string};

/// A kind of host reached over SSH, and so a table carrying the host-key
/// columns.
///
/// A closed enum rather than a table name: the SQL below is composed from
/// `const` fragments in this file plus bound parameters, and this is what keeps
/// a caller-supplied string from ever reaching it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SshHost {
    Target,
    BuildHost,
}

impl SshHost {
    /// The table carrying this host's rows.
    const fn table(self) -> &'static str {
        match self {
            Self::Target => "targets",
            Self::BuildHost => "build_hosts",
        }
    }

    /// What this kind of host is called when a message has to name it.
    pub const fn subject(self) -> &'static str {
        match self {
            Self::Target => "target",
            Self::BuildHost => "build host",
        }
    }
}

/// The host-key half of a row.
///
/// `None` when nothing is pinned and nothing is pending, so a host that has
/// never connected reads as having no host key rather than as having an empty
/// one. Public key material throughout — safe to hand to any client.
pub(crate) fn row_to_host_key(row: &SqliteRow) -> Option<HostKey> {
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

impl Store {
    /// Pins a host's key, clearing any pending change.
    ///
    /// Used for the first-use recording and for accepting a reviewed change;
    /// both end in the same state, which is why they are one method.
    pub async fn pin_host_key(
        &self,
        host: SshHost,
        id: &str,
        key: &str,
        fingerprint: &str,
    ) -> anyhow::Result<()> {
        sqlx::query(AssertSqlSafe(format!(
            "UPDATE {}
                SET host_key = ?1,
                    host_key_fingerprint = ?2,
                    host_key_pinned_at = ?3,
                    pending_host_key = '',
                    pending_host_key_fingerprint = '',
                    pending_host_key_seen_at = NULL
              WHERE id = ?4",
            host.table()
        )))
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
        host: SshHost,
        id: &str,
        key: &str,
        fingerprint: &str,
    ) -> anyhow::Result<()> {
        sqlx::query(AssertSqlSafe(format!(
            "UPDATE {}
                SET pending_host_key = ?1,
                    pending_host_key_fingerprint = ?2,
                    pending_host_key_seen_at =
                        CASE WHEN pending_host_key = ?1 AND pending_host_key_seen_at IS NOT NULL
                             THEN pending_host_key_seen_at
                             ELSE ?3
                        END
              WHERE id = ?4",
            host.table()
        )))
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
    pub async fn clear_pending_host_key(&self, host: SshHost, id: &str) -> anyhow::Result<()> {
        sqlx::query(AssertSqlSafe(format!(
            "UPDATE {}
                SET pending_host_key = '',
                    pending_host_key_fingerprint = '',
                    pending_host_key_seen_at = NULL
              WHERE id = ?1 AND pending_host_key <> ''",
            host.table()
        )))
        .bind(id)
        .execute(self.pool())
        .await?;
        Ok(())
    }

    /// Forgets a host's pinned key, reopening the first-use window.
    pub async fn forget_host_key(&self, host: SshHost, id: &str) -> anyhow::Result<()> {
        let result = sqlx::query(AssertSqlSafe(format!(
            "UPDATE {}
                SET host_key = '',
                    host_key_fingerprint = '',
                    host_key_pinned_at = NULL,
                    pending_host_key = '',
                    pending_host_key_fingerprint = '',
                    pending_host_key_seen_at = NULL
              WHERE id = ?1",
            host.table()
        )))
        .bind(id)
        .execute(self.pool())
        .await?;
        if result.rows_affected() == 0 {
            anyhow::bail!("no such {}: {id}", host.subject());
        }
        Ok(())
    }

    /// Records the outcome of a reachability probe.
    ///
    /// `last_seen` is the caller's decision, not this method's: it means "last
    /// confirmed reachable", so only a successful probe passes a value and a
    /// failed one leaves the column as it was. `COALESCE` is what expresses
    /// that in one statement, the same way [`Store::set_ingress_status`]
    /// handles its own reload timestamp.
    pub(crate) async fn set_host_status(
        &self,
        host: SshHost,
        id: &str,
        status: &str,
        last_seen: Option<String>,
    ) -> anyhow::Result<()> {
        sqlx::query(AssertSqlSafe(format!(
            "UPDATE {} SET status = ?1, last_seen_at = COALESCE(?2, last_seen_at) WHERE id = ?3",
            host.table()
        )))
        .bind(status)
        .bind(last_seen)
        .bind(id)
        .execute(self.pool())
        .await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_host_kind_names_a_distinct_table_and_subject() {
        // The table name is interpolated into SQL, so it must come from this
        // closed set and nowhere else.
        assert_eq!(SshHost::Target.table(), "targets");
        assert_eq!(SshHost::BuildHost.table(), "build_hosts");
        assert_ne!(SshHost::Target.subject(), SshHost::BuildHost.subject());
    }

    #[tokio::test]
    async fn a_failed_probe_leaves_the_last_seen_time_alone() {
        let store = Store::open_in_memory().await.expect("open");
        let target = store
            .create_target(&crate::store::TargetInput {
                name: "web".to_string(),
                host: "10.0.0.1".to_string(),
                ..Default::default()
            })
            .await
            .expect("create");

        store
            .set_host_status(SshHost::Target, &target.id, "reachable", Some(now_string()))
            .await
            .expect("reachable");
        let seen = store
            .get_target(&target.id)
            .await
            .expect("get")
            .expect("target")
            .last_seen_at;
        assert!(seen.is_some(), "a successful probe records the time");

        // "Last confirmed reachable" must survive a later failure, otherwise it
        // would just track the most recent attempt.
        store
            .set_host_status(SshHost::Target, &target.id, "unreachable", None)
            .await
            .expect("unreachable");
        let after = store
            .get_target(&target.id)
            .await
            .expect("get")
            .expect("target");
        assert_eq!(after.last_seen_at, seen);
        assert_eq!(after.status, nudo_proto::target::Status::Unreachable as i32);
    }

    #[tokio::test]
    async fn the_same_state_machine_governs_both_kinds_of_host() {
        let store = Store::open_in_memory().await.expect("open");
        let target = store
            .create_target(&crate::store::TargetInput {
                name: "web".to_string(),
                host: "10.0.0.1".to_string(),
                ..Default::default()
            })
            .await
            .expect("create target");
        let build_host = store
            .create_build_host(&crate::store::BuildHostInput {
                name: "builder".to_string(),
                host: "10.0.0.2".to_string(),
                ..Default::default()
            })
            .await
            .expect("create build host");

        for (host, id) in [
            (SshHost::Target, target.id.as_str()),
            (SshHost::BuildHost, build_host.id.as_str()),
        ] {
            store
                .pin_host_key(host, id, "ssh-ed25519 AAAA", "SHA256:aaa")
                .await
                .expect("pin");
            store
                .record_pending_host_key(host, id, "ssh-ed25519 BBBB", "SHA256:bbb")
                .await
                .expect("record pending");
            // A host presenting the pinned key again drops the pending change.
            store.clear_pending_host_key(host, id).await.expect("clear");
            store.forget_host_key(host, id).await.expect("forget");
        }

        assert!(
            store
                .get_target(&target.id)
                .await
                .expect("get")
                .expect("target")
                .host_key
                .is_none(),
            "forgetting leaves no key at all, reopening first use"
        );
        assert!(
            store
                .get_build_host(&build_host.id)
                .await
                .expect("get")
                .expect("build host")
                .host_key
                .is_none()
        );
    }

    #[tokio::test]
    async fn forgetting_an_unknown_host_names_the_kind_it_looked_for() {
        let store = Store::open_in_memory().await.expect("open");

        let error = store
            .forget_host_key(SshHost::BuildHost, "missing")
            .await
            .expect_err("no such row");
        assert!(
            error.to_string().contains("no such build host"),
            "got: {error}"
        );

        let error = store
            .forget_host_key(SshHost::Target, "missing")
            .await
            .expect_err("no such row");
        assert!(error.to_string().contains("no such target"), "got: {error}");
    }
}
