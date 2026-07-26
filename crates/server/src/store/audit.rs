//! Audit log. Every mutating operation lands here, including dry runs.

use nudo_proto::{Actor, AuditEntry, actor};
use sqlx::Row;

use super::{Store, from_db_time, new_id, now_string};
use super::deployments::parse_actor_kind;
// The SQL strings below are composed only from `const` fragments in this file
// plus bound parameters; no caller-supplied value is ever interpolated, which is
// what `AssertSqlSafe` asserts.
use sqlx::AssertSqlSafe;

/// One entry to record.
#[derive(Debug, Clone)]
pub struct NewAuditEntry {
    pub actor: Actor,
    /// The RPC name, e.g. `Deployments.Deploy`.
    pub action: String,
    /// The target, service or deployment the action concerned.
    pub subject_id: String,
    pub dry_run: bool,
    /// A one-line human summary, which is what the dashboard shows.
    pub summary: String,
}

impl Store {
    /// Records an audit entry.
    ///
    /// Failures are logged rather than propagated: losing an audit line is bad,
    /// but failing the operation the user asked for because the audit write
    /// failed is worse, and would make the log a single point of failure for
    /// every mutation.
    pub async fn audit(&self, entry: NewAuditEntry) {
        if let Err(error) = self.try_audit(&entry).await {
            tracing::error!(
                %error,
                action = %entry.action,
                subject = %entry.subject_id,
                "failed to write an audit entry"
            );
        }
    }

    async fn try_audit(&self, entry: &NewAuditEntry) -> anyhow::Result<()> {
        sqlx::query(
            "INSERT INTO audit_entries
               (id, at, actor_kind, actor_id, actor_label, action, subject_id, dry_run, summary)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        )
        .bind(new_id("aud"))
        .bind(now_string())
        .bind(entry.actor.kind_str())
        .bind(&entry.actor.id)
        .bind(&entry.actor.label)
        .bind(&entry.action)
        .bind(&entry.subject_id)
        .bind(entry.dry_run as i64)
        .bind(&entry.summary)
        .execute(self.pool())
        .await?;
        Ok(())
    }

    /// Lists audit entries, newest first, optionally filtered by subject and
    /// actor kind.
    pub async fn list_audit(
        &self,
        subject_id: &str,
        actor_kind: actor::Kind,
        limit: i64,
        offset: i64,
    ) -> anyhow::Result<Vec<AuditEntry>> {
        let mut sql = String::from(
            "SELECT id, at, actor_kind, actor_id, actor_label, action, subject_id,
                    dry_run, summary
             FROM audit_entries WHERE 1 = 1",
        );

        let filter_subject = !subject_id.trim().is_empty();
        let filter_kind = actor_kind != actor::Kind::Unspecified;

        let mut next = 1;
        let subject_placeholder = if filter_subject {
            let p = next;
            next += 1;
            sql.push_str(&format!(" AND subject_id = ?{p}"));
            Some(p)
        } else {
            None
        };
        if filter_kind {
            sql.push_str(&format!(" AND actor_kind = ?{next}"));
            next += 1;
        }
        sql.push_str(&format!(" ORDER BY at DESC, id DESC LIMIT ?{next} OFFSET ?{}", next + 1));
        let _ = subject_placeholder;

        let mut query = sqlx::query(AssertSqlSafe(sql.clone()));
        if filter_subject {
            query = query.bind(subject_id.trim());
        }
        if filter_kind {
            query = query.bind(actor_kind_str(actor_kind));
        }
        let rows = query.bind(limit).bind(offset).fetch_all(self.pool()).await?;

        Ok(rows
            .into_iter()
            .map(|row| AuditEntry {
                id: row.get("id"),
                at: nudo_proto::to_timestamp_opt(from_db_time(&row.get::<String, _>("at"))),
                actor: Some(Actor {
                    kind: parse_actor_kind(&row.get::<String, _>("actor_kind")) as i32,
                    id: row.get("actor_id"),
                    label: row.get("actor_label"),
                }),
                action: row.get("action"),
                subject_id: row.get("subject_id"),
                dry_run: row.get::<i64, _>("dry_run") != 0,
                summary: row.get("summary"),
            })
            .collect())
    }

    pub async fn count_audit(&self) -> anyhow::Result<i64> {
        let (count,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM audit_entries")
            .fetch_one(self.pool())
            .await?;
        Ok(count)
    }
}

/// The stored form of an actor kind.
fn actor_kind_str(kind: actor::Kind) -> &'static str {
    match kind {
        actor::Kind::Human => "human",
        actor::Kind::Agent => "agent",
        actor::Kind::Webhook => "webhook",
        actor::Kind::System => "system",
        actor::Kind::Unspecified => "unspecified",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn store() -> Store {
        Store::open_in_memory().await.expect("open")
    }

    fn entry(action: &str, actor: Actor) -> NewAuditEntry {
        NewAuditEntry {
            actor,
            action: action.to_string(),
            subject_id: "svc_1".to_string(),
            dry_run: false,
            summary: format!("did {action}"),
        }
    }

    #[tokio::test]
    async fn an_entry_records_the_actor_action_and_summary() {
        let store = store().await;
        store
            .audit(entry("Deployments.Deploy", Actor::human("usr_1", "alice")))
            .await;

        let entries = store
            .list_audit("", actor::Kind::Unspecified, 50, 0)
            .await
            .expect("list");
        assert_eq!(entries.len(), 1);

        let recorded = &entries[0];
        assert_eq!(recorded.action, "Deployments.Deploy");
        assert_eq!(recorded.subject_id, "svc_1");
        assert!(!recorded.dry_run);
        assert_eq!(recorded.summary, "did Deployments.Deploy");

        let actor = recorded.actor.clone().expect("actor");
        assert_eq!(actor.kind, actor::Kind::Human as i32);
        assert_eq!(actor.label, "alice");
    }

    #[tokio::test]
    async fn dry_runs_are_recorded_and_distinguishable() {
        // An agent probing with dry_run should still leave a trail.
        let store = store().await;
        store
            .audit(NewAuditEntry {
                dry_run: true,
                ..entry("Deployments.Deploy", Actor::agent("sess_1", "claude"))
            })
            .await;

        let entries = store
            .list_audit("", actor::Kind::Unspecified, 50, 0)
            .await
            .expect("list");
        assert!(entries[0].dry_run);
    }

    #[tokio::test]
    async fn entries_are_newest_first() {
        let store = store().await;
        for i in 0..3 {
            store
                .audit(entry(&format!("Action{i}"), Actor::system("sweeper")))
                .await;
            tokio::time::sleep(std::time::Duration::from_millis(2)).await;
        }

        let entries = store
            .list_audit("", actor::Kind::Unspecified, 50, 0)
            .await
            .expect("list");
        assert_eq!(entries[0].action, "Action2");
        assert_eq!(entries[2].action, "Action0");
    }

    #[tokio::test]
    async fn entries_can_be_filtered_by_subject() {
        let store = store().await;
        store.audit(entry("A", Actor::system("s"))).await;
        store
            .audit(NewAuditEntry {
                subject_id: "tgt_9".to_string(),
                ..entry("B", Actor::system("s"))
            })
            .await;

        let filtered = store
            .list_audit("tgt_9", actor::Kind::Unspecified, 50, 0)
            .await
            .expect("list");
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].action, "B");
    }

    #[tokio::test]
    async fn entries_can_be_filtered_by_actor_kind() {
        let store = store().await;
        store.audit(entry("human-thing", Actor::human("u", "alice"))).await;
        store.audit(entry("agent-thing", Actor::agent("a", "claude"))).await;
        store.audit(entry("hook-thing", Actor::webhook("d", "push"))).await;

        let agents = store.list_audit("", actor::Kind::Agent, 50, 0).await.expect("list");
        assert_eq!(agents.len(), 1);
        assert_eq!(agents[0].action, "agent-thing");

        let hooks = store.list_audit("", actor::Kind::Webhook, 50, 0).await.expect("list");
        assert_eq!(hooks.len(), 1);
    }

    #[tokio::test]
    async fn both_filters_apply_together() {
        let store = store().await;
        store
            .audit(NewAuditEntry {
                subject_id: "svc_a".to_string(),
                ..entry("wanted", Actor::agent("a", "claude"))
            })
            .await;
        store
            .audit(NewAuditEntry {
                subject_id: "svc_a".to_string(),
                ..entry("wrong-kind", Actor::human("u", "alice"))
            })
            .await;
        store
            .audit(NewAuditEntry {
                subject_id: "svc_b".to_string(),
                ..entry("wrong-subject", Actor::agent("a", "claude"))
            })
            .await;

        let found = store
            .list_audit("svc_a", actor::Kind::Agent, 50, 0)
            .await
            .expect("list");
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].action, "wanted");
    }

    #[tokio::test]
    async fn listing_paginates() {
        let store = store().await;
        for i in 0..5 {
            store.audit(entry(&format!("A{i}"), Actor::system("s"))).await;
            tokio::time::sleep(std::time::Duration::from_millis(2)).await;
        }

        let first = store.list_audit("", actor::Kind::Unspecified, 2, 0).await.expect("list");
        let second = store.list_audit("", actor::Kind::Unspecified, 2, 2).await.expect("list");
        assert_eq!(first.len(), 2);
        assert_eq!(second.len(), 2);
        assert!(first.iter().all(|a| second.iter().all(|b| a.id != b.id)));
        assert_eq!(store.count_audit().await.expect("count"), 5);
    }

    #[tokio::test]
    async fn audit_entries_outlive_the_subjects_they_describe() {
        // The point of an audit log is to record what happened to something
        // that may since have been deleted, so there is no foreign key.
        let store = store().await;
        store.audit(entry("Targets.Delete", Actor::human("u", "alice"))).await;

        let entries = store
            .list_audit("svc_1", actor::Kind::Unspecified, 50, 0)
            .await
            .expect("list");
        assert_eq!(entries.len(), 1);
    }

    #[test]
    fn every_actor_kind_has_a_stored_form_that_parses_back() {
        for kind in [
            actor::Kind::Human,
            actor::Kind::Agent,
            actor::Kind::Webhook,
            actor::Kind::System,
        ] {
            assert_eq!(parse_actor_kind(actor_kind_str(kind)), kind);
        }
    }
}
