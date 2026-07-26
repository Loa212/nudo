//! Deployment and release persistence.

use anyhow::bail;
use nudo_proto::{Actor, Deployment, Release, actor, deployment};
use sqlx::Row;
use sqlx::sqlite::SqliteRow;

use super::{Store, from_db_time, from_db_time_opt, new_id, now_string};
// The SQL strings below are composed only from `const` fragments in this file
// plus bound parameters; no caller-supplied value is ever interpolated, which is
// what `AssertSqlSafe` asserts.
use sqlx::AssertSqlSafe;

/// What kicked a deployment off, for the history view.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeployTrigger {
    Manual,
    Webhook,
    Rollback,
    Api,
    Agent,
}

impl DeployTrigger {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Manual => "manual",
            Self::Webhook => "webhook",
            Self::Rollback => "rollback",
            Self::Api => "api",
            Self::Agent => "agent",
        }
    }

    /// Infers the trigger from the actor when a caller did not say.
    pub fn from_actor(actor: &Actor) -> Self {
        match actor::Kind::try_from(actor.kind) {
            Ok(actor::Kind::Webhook) => Self::Webhook,
            Ok(actor::Kind::Agent) => Self::Agent,
            _ => Self::Manual,
        }
    }
}

/// A new deployment record.
#[derive(Debug, Clone)]
pub struct NewDeployment {
    pub service_id: String,
    pub actor: Actor,
    pub previous_release_id: String,
    pub git_ref: String,
    pub trigger: DeployTrigger,
}

impl Store {
    /// Inserts a queued deployment. The engine picks it up and advances it.
    pub async fn create_deployment(&self, new: &NewDeployment) -> anyhow::Result<Deployment> {
        if self.get_service(&new.service_id).await?.is_none() {
            bail!("no such service: {}", new.service_id);
        }

        let id = new_id("dep");
        sqlx::query(
            "INSERT INTO deployments
               (id, service_id, release_id, status, actor_kind, actor_id, actor_label,
                previous_release_id, error, cancel_requested, git_sha, git_ref, trigger,
                started_at, finished_at)
             VALUES (?1, ?2, '', ?3, ?4, ?5, ?6, ?7, '', 0, '', ?8, ?9, ?10, NULL)",
        )
        .bind(&id)
        .bind(&new.service_id)
        .bind(deployment::Status::Queued.as_str())
        .bind(new.actor.kind_str())
        .bind(&new.actor.id)
        .bind(&new.actor.label)
        .bind(&new.previous_release_id)
        .bind(new.git_ref.trim())
        .bind(new.trigger.as_str())
        .bind(now_string())
        .execute(self.pool())
        .await?;

        self.get_deployment(&id)
            .await?
            .ok_or_else(|| anyhow::anyhow!("deployment vanished immediately after creation"))
    }

    pub async fn get_deployment(&self, id: &str) -> anyhow::Result<Option<Deployment>> {
        let row = sqlx::query(AssertSqlSafe(format!("{DEPLOYMENT_SELECT} WHERE id = ?1")))
            .bind(id)
            .fetch_optional(self.pool())
            .await?;
        Ok(row.as_ref().map(row_to_deployment))
    }

    /// Lists deployments, newest first, optionally for one service.
    pub async fn list_deployments(
        &self,
        service_id: &str,
        limit: i64,
        offset: i64,
    ) -> anyhow::Result<Vec<Deployment>> {
        let rows = if service_id.trim().is_empty() {
            sqlx::query(AssertSqlSafe(format!(
                "{DEPLOYMENT_SELECT} ORDER BY started_at DESC, id DESC LIMIT ?1 OFFSET ?2"
            )))
            .bind(limit)
            .bind(offset)
            .fetch_all(self.pool())
            .await?
        } else {
            sqlx::query(AssertSqlSafe(format!(
                "{DEPLOYMENT_SELECT} WHERE service_id = ?1 \
                 ORDER BY started_at DESC, id DESC LIMIT ?2 OFFSET ?3"
            )))
            .bind(service_id.trim())
            .bind(limit)
            .bind(offset)
            .fetch_all(self.pool())
            .await?
        };
        Ok(rows.iter().map(row_to_deployment).collect())
    }

    /// Deployments that are still running, used to resume the dashboard's live
    /// view and to surface in-flight work after a restart.
    pub async fn active_deployments(&self) -> anyhow::Result<Vec<Deployment>> {
        let rows = sqlx::query(AssertSqlSafe(format!(
            "{DEPLOYMENT_SELECT} WHERE status IN \
             ('queued','building','uploading','activating','health_checking') \
             ORDER BY started_at DESC"
        )))
        .fetch_all(self.pool())
        .await?;
        Ok(rows.iter().map(row_to_deployment).collect())
    }

    pub async fn set_deployment_status(
        &self,
        id: &str,
        status: deployment::Status,
    ) -> anyhow::Result<()> {
        if status.is_terminal() {
            sqlx::query("UPDATE deployments SET status = ?1, finished_at = ?2 WHERE id = ?3")
                .bind(status.as_str())
                .bind(now_string())
                .bind(id)
                .execute(self.pool())
                .await?;
        } else {
            sqlx::query("UPDATE deployments SET status = ?1 WHERE id = ?2")
                .bind(status.as_str())
                .bind(id)
                .execute(self.pool())
                .await?;
        }
        Ok(())
    }

    /// Records why a deployment failed, alongside its terminal status.
    pub async fn set_deployment_error(&self, id: &str, error: &str) -> anyhow::Result<()> {
        sqlx::query("UPDATE deployments SET error = ?1 WHERE id = ?2")
            .bind(error)
            .bind(id)
            .execute(self.pool())
            .await?;
        Ok(())
    }

    /// Binds a deployment to the release it produced.
    pub async fn set_deployment_release(
        &self,
        id: &str,
        release_id: &str,
        git_sha: &str,
    ) -> anyhow::Result<()> {
        sqlx::query("UPDATE deployments SET release_id = ?1, git_sha = ?2 WHERE id = ?3")
            .bind(release_id)
            .bind(git_sha)
            .bind(id)
            .execute(self.pool())
            .await?;
        Ok(())
    }

    /// Flags a deployment for cancellation.
    ///
    /// The engine checks this between steps rather than being killed, so a
    /// cancel never interrupts a symlink swap or a half-written unit file.
    pub async fn request_cancel(&self, id: &str) -> anyhow::Result<()> {
        let result = sqlx::query(
            "UPDATE deployments SET cancel_requested = 1 WHERE id = ?1 AND finished_at IS NULL",
        )
        .bind(id)
        .execute(self.pool())
        .await?;

        if result.rows_affected() == 0 {
            bail!("deployment {id} is not running");
        }
        Ok(())
    }

    pub async fn cancel_requested(&self, id: &str) -> anyhow::Result<bool> {
        let row: Option<(i64,)> =
            sqlx::query_as("SELECT cancel_requested FROM deployments WHERE id = ?1")
                .bind(id)
                .fetch_optional(self.pool())
                .await?;
        Ok(row.map(|(flag,)| flag != 0).unwrap_or(false))
    }

    /// Appends a line of build or deploy output.
    pub async fn append_deployment_log(
        &self,
        deployment_id: &str,
        line: &str,
        stderr: bool,
    ) -> anyhow::Result<()> {
        sqlx::query(
            "INSERT INTO deployment_logs (deployment_id, at, stderr, line)
             VALUES (?1, ?2, ?3, ?4)",
        )
        .bind(deployment_id)
        .bind(now_string())
        .bind(stderr as i64)
        .bind(line)
        .execute(self.pool())
        .await?;
        Ok(())
    }

    /// Reads back a deployment's output, so a view opened after the fact is not
    /// empty.
    pub async fn deployment_logs(
        &self,
        deployment_id: &str,
    ) -> anyhow::Result<Vec<DeploymentLogLine>> {
        let rows = sqlx::query(
            "SELECT at, stderr, line FROM deployment_logs
             WHERE deployment_id = ?1 ORDER BY id ASC",
        )
        .bind(deployment_id)
        .fetch_all(self.pool())
        .await?;

        Ok(rows
            .iter()
            .map(|row| DeploymentLogLine {
                at: from_db_time(&row.get::<String, _>("at")).unwrap_or_else(chrono::Utc::now),
                stderr: row.get::<i64, _>("stderr") != 0,
                line: row.get("line"),
            })
            .collect())
    }

    // ---- releases ----

    /// Records a release. Called once the artifact is on the target.
    pub async fn create_release(&self, release: &Release) -> anyhow::Result<Release> {
        let id = if release.id.trim().is_empty() {
            new_id("rel")
        } else {
            release.id.trim().to_string()
        };

        sqlx::query(
            "INSERT INTO releases
               (id, service_id, git_sha, git_ref, artifact_digest, artifact_bytes,
                path, pruned, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 0, ?8)",
        )
        .bind(&id)
        .bind(&release.service_id)
        .bind(&release.git_sha)
        .bind(&release.git_ref)
        .bind(&release.artifact_digest)
        .bind(release.artifact_bytes as i64)
        .bind(&release.path)
        .bind(now_string())
        .execute(self.pool())
        .await?;

        self.get_release(&id)
            .await?
            .ok_or_else(|| anyhow::anyhow!("release vanished immediately after creation"))
    }

    pub async fn get_release(&self, id: &str) -> anyhow::Result<Option<Release>> {
        let row = sqlx::query(
            "SELECT id, service_id, git_sha, git_ref, artifact_digest, artifact_bytes,
                    path, created_at
             FROM releases WHERE id = ?1",
        )
        .bind(id)
        .fetch_optional(self.pool())
        .await?;
        Ok(row.as_ref().map(row_to_release))
    }

    /// Retained releases for a service, newest first.
    ///
    /// Pruned releases are excluded: offering one for rollback would point the
    /// `current` symlink at a directory that is no longer on the target.
    pub async fn list_releases(&self, service_id: &str) -> anyhow::Result<Vec<Release>> {
        let rows = sqlx::query(
            "SELECT id, service_id, git_sha, git_ref, artifact_digest, artifact_bytes,
                    path, created_at
             FROM releases WHERE service_id = ?1 AND pruned = 0
             ORDER BY created_at DESC, id DESC",
        )
        .bind(service_id)
        .fetch_all(self.pool())
        .await?;
        Ok(rows.iter().map(row_to_release).collect())
    }

    /// Marks a release as removed from the target.
    pub async fn mark_release_pruned(&self, id: &str) -> anyhow::Result<()> {
        sqlx::query("UPDATE releases SET pruned = 1 WHERE id = ?1")
            .bind(id)
            .execute(self.pool())
            .await?;
        Ok(())
    }
}

/// One line of stored deployment output.
#[derive(Debug, Clone)]
pub struct DeploymentLogLine {
    pub at: chrono::DateTime<chrono::Utc>,
    pub stderr: bool,
    pub line: String,
}

const DEPLOYMENT_SELECT: &str = "SELECT
    id, service_id, release_id, status, actor_kind, actor_id, actor_label,
    previous_release_id, error, git_sha, git_ref, trigger, started_at, finished_at
  FROM deployments";

fn row_to_deployment(row: &SqliteRow) -> Deployment {
    let actor_kind: String = row.get("actor_kind");
    Deployment {
        id: row.get("id"),
        service_id: row.get("service_id"),
        release_id: row.get("release_id"),
        status: deployment::Status::parse(&row.get::<String, _>("status")) as i32,
        actor: Some(Actor {
            kind: parse_actor_kind(&actor_kind) as i32,
            id: row.get("actor_id"),
            label: row.get("actor_label"),
        }),
        previous_release_id: row.get("previous_release_id"),
        error: row.get("error"),
        started_at: nudo_proto::to_timestamp_opt(from_db_time(
            &row.get::<String, _>("started_at"),
        )),
        finished_at: nudo_proto::to_timestamp_opt(from_db_time_opt(
            row.get::<Option<String>, _>("finished_at").as_deref(),
        )),
    }
}

fn row_to_release(row: &SqliteRow) -> Release {
    Release {
        id: row.get("id"),
        service_id: row.get("service_id"),
        git_sha: row.get("git_sha"),
        git_ref: row.get("git_ref"),
        artifact_digest: row.get("artifact_digest"),
        artifact_bytes: row.get::<i64, _>("artifact_bytes") as u64,
        path: row.get("path"),
        created_at: nudo_proto::to_timestamp_opt(from_db_time(
            &row.get::<String, _>("created_at"),
        )),
    }
}

/// Parses the stored actor kind.
pub fn parse_actor_kind(raw: &str) -> actor::Kind {
    match raw {
        "human" => actor::Kind::Human,
        "agent" => actor::Kind::Agent,
        "webhook" => actor::Kind::Webhook,
        "system" => actor::Kind::System,
        _ => actor::Kind::Unspecified,
    }
}

/// The trigger recorded against a deployment, for display.
pub async fn deployment_trigger(store: &Store, id: &str) -> anyhow::Result<String> {
    let row: Option<(String,)> = sqlx::query_as("SELECT trigger FROM deployments WHERE id = ?1")
        .bind(id)
        .fetch_optional(store.pool())
        .await?;
    Ok(row.map(|(t,)| t).unwrap_or_default())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::TargetInput;

    async fn fixture() -> (Store, String) {
        let store = Store::open_in_memory().await.expect("open");
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
                target_id: target.id,
                name: "bot".to_string(),
                ..Default::default()
            })
            .await
            .expect("service");
        (store, service.id)
    }

    fn new_deployment(service_id: &str) -> NewDeployment {
        NewDeployment {
            service_id: service_id.to_string(),
            actor: Actor::human("usr_1", "alice"),
            previous_release_id: String::new(),
            git_ref: "main".to_string(),
            trigger: DeployTrigger::Manual,
        }
    }

    #[tokio::test]
    async fn a_new_deployment_starts_queued_with_its_actor_recorded() {
        let (store, service_id) = fixture().await;
        let created = store
            .create_deployment(&new_deployment(&service_id))
            .await
            .expect("create");

        assert!(created.id.starts_with("dep_"));
        assert_eq!(created.status, deployment::Status::Queued as i32);
        assert!(created.finished_at.is_none());

        let actor = created.actor.expect("actor");
        assert_eq!(actor.kind, actor::Kind::Human as i32);
        assert_eq!(actor.label, "alice");
    }

    #[tokio::test]
    async fn a_deployment_needs_an_existing_service() {
        let store = Store::open_in_memory().await.expect("open");
        assert!(
            store
                .create_deployment(&new_deployment("svc_missing"))
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn reaching_a_terminal_status_stamps_the_finish_time() {
        let (store, service_id) = fixture().await;
        let created = store
            .create_deployment(&new_deployment(&service_id))
            .await
            .expect("create");

        // Intermediate statuses leave it open.
        store
            .set_deployment_status(&created.id, deployment::Status::Building)
            .await
            .expect("set");
        let building = store.get_deployment(&created.id).await.expect("get").expect("some");
        assert_eq!(building.status, deployment::Status::Building as i32);
        assert!(building.finished_at.is_none());

        store
            .set_deployment_status(&created.id, deployment::Status::Succeeded)
            .await
            .expect("set");
        let done = store.get_deployment(&created.id).await.expect("get").expect("some");
        assert!(done.finished_at.is_some());
    }

    #[tokio::test]
    async fn an_error_is_recorded_against_the_deployment() {
        let (store, service_id) = fixture().await;
        let created = store
            .create_deployment(&new_deployment(&service_id))
            .await
            .expect("create");

        store
            .set_deployment_error(&created.id, "health check failed after 3 retries")
            .await
            .expect("set");
        let failed = store.get_deployment(&created.id).await.expect("get").expect("some");
        assert!(failed.error.contains("health check failed"));
    }

    #[tokio::test]
    async fn a_cancel_request_is_visible_to_the_engine() {
        let (store, service_id) = fixture().await;
        let created = store
            .create_deployment(&new_deployment(&service_id))
            .await
            .expect("create");

        assert!(!store.cancel_requested(&created.id).await.expect("check"));
        store.request_cancel(&created.id).await.expect("cancel");
        assert!(store.cancel_requested(&created.id).await.expect("check"));
    }

    #[tokio::test]
    async fn a_finished_deployment_cannot_be_cancelled() {
        let (store, service_id) = fixture().await;
        let created = store
            .create_deployment(&new_deployment(&service_id))
            .await
            .expect("create");
        store
            .set_deployment_status(&created.id, deployment::Status::Succeeded)
            .await
            .expect("set");

        let error = store.request_cancel(&created.id).await.expect_err("must refuse");
        assert!(error.to_string().contains("not running"), "got: {error}");
    }

    #[tokio::test]
    async fn cancelling_an_unknown_deployment_fails_rather_than_silently_passing() {
        let store = Store::open_in_memory().await.expect("open");
        assert!(store.request_cancel("dep_nope").await.is_err());
        assert!(!store.cancel_requested("dep_nope").await.expect("check"));
    }

    #[tokio::test]
    async fn deployment_output_is_stored_in_order_with_streams_distinguished() {
        let (store, service_id) = fixture().await;
        let created = store
            .create_deployment(&new_deployment(&service_id))
            .await
            .expect("create");

        store.append_deployment_log(&created.id, "compiling", false).await.expect("log");
        store.append_deployment_log(&created.id, "warning: x", true).await.expect("log");
        store.append_deployment_log(&created.id, "done", false).await.expect("log");

        let logs = store.deployment_logs(&created.id).await.expect("logs");
        assert_eq!(logs.len(), 3);
        assert_eq!(logs[0].line, "compiling");
        assert!(!logs[0].stderr);
        assert!(logs[1].stderr, "stderr must be distinguishable");
        assert_eq!(logs[2].line, "done");
    }

    #[tokio::test]
    async fn only_running_deployments_are_reported_as_active() {
        let (store, service_id) = fixture().await;

        let running = store
            .create_deployment(&new_deployment(&service_id))
            .await
            .expect("create");
        store
            .set_deployment_status(&running.id, deployment::Status::HealthChecking)
            .await
            .expect("set");

        let finished = store
            .create_deployment(&new_deployment(&service_id))
            .await
            .expect("create");
        store
            .set_deployment_status(&finished.id, deployment::Status::Failed)
            .await
            .expect("set");

        let active = store.active_deployments().await.expect("active");
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].id, running.id);
    }

    #[tokio::test]
    async fn releases_list_newest_first_and_exclude_pruned_ones() {
        let (store, service_id) = fixture().await;

        let mut ids = Vec::new();
        for i in 0..3 {
            let release = store
                .create_release(&Release {
                    service_id: service_id.clone(),
                    git_sha: format!("sha{i}"),
                    path: format!("/opt/bot/releases/r{i}"),
                    artifact_bytes: 1024,
                    ..Default::default()
                })
                .await
                .expect("create");
            ids.push(release.id);
            // Distinct stored timestamps so ordering is deterministic.
            tokio::time::sleep(std::time::Duration::from_millis(2)).await;
        }

        let listed = store.list_releases(&service_id).await.expect("list");
        assert_eq!(listed.len(), 3);
        assert_eq!(listed[0].id, ids[2], "newest first");
        assert_eq!(listed[0].artifact_bytes, 1024);

        // A pruned release must not be offered for rollback.
        store.mark_release_pruned(&ids[0]).await.expect("prune");
        let remaining = store.list_releases(&service_id).await.expect("list");
        assert_eq!(remaining.len(), 2);
        assert!(!remaining.iter().any(|r| r.id == ids[0]));
    }

    #[tokio::test]
    async fn a_deployment_can_be_bound_to_the_release_it_produced() {
        let (store, service_id) = fixture().await;
        let deployment = store
            .create_deployment(&new_deployment(&service_id))
            .await
            .expect("create");
        let release = store
            .create_release(&Release {
                service_id: service_id.clone(),
                path: "/opt/bot/releases/r1".to_string(),
                ..Default::default()
            })
            .await
            .expect("release");

        store
            .set_deployment_release(&deployment.id, &release.id, "abc123")
            .await
            .expect("bind");
        let bound = store.get_deployment(&deployment.id).await.expect("get").expect("some");
        assert_eq!(bound.release_id, release.id);
    }

    #[tokio::test]
    async fn deleting_a_service_removes_its_deployments_and_releases() {
        let (store, service_id) = fixture().await;
        let deployment = store
            .create_deployment(&new_deployment(&service_id))
            .await
            .expect("create");
        store.append_deployment_log(&deployment.id, "line", false).await.expect("log");
        store
            .create_release(&Release {
                service_id: service_id.clone(),
                path: "/p".to_string(),
                ..Default::default()
            })
            .await
            .expect("release");

        store.delete_service(&service_id).await.expect("delete");

        assert!(store.get_deployment(&deployment.id).await.expect("get").is_none());
        assert!(store.list_releases(&service_id).await.expect("list").is_empty());
        // Logs cascade too, rather than being orphaned.
        assert!(store.deployment_logs(&deployment.id).await.expect("logs").is_empty());
    }

    #[tokio::test]
    async fn the_trigger_is_recorded_and_readable() {
        let (store, service_id) = fixture().await;
        let created = store
            .create_deployment(&NewDeployment {
                trigger: DeployTrigger::Webhook,
                ..new_deployment(&service_id)
            })
            .await
            .expect("create");

        assert_eq!(
            deployment_trigger(&store, &created.id).await.expect("trigger"),
            "webhook"
        );
    }

    #[test]
    fn a_trigger_is_inferred_from_the_actor_kind() {
        assert_eq!(
            DeployTrigger::from_actor(&Actor::webhook("d1", "push")),
            DeployTrigger::Webhook
        );
        assert_eq!(
            DeployTrigger::from_actor(&Actor::agent("a1", "claude")),
            DeployTrigger::Agent
        );
        assert_eq!(
            DeployTrigger::from_actor(&Actor::human("u1", "alice")),
            DeployTrigger::Manual
        );
    }

    #[test]
    fn actor_kinds_round_trip_through_storage() {
        for actor in [
            Actor::human("u", "u"),
            Actor::agent("a", "a"),
            Actor::webhook("w", "w"),
            Actor::system("s"),
        ] {
            let kind = actor::Kind::try_from(actor.kind).expect("kind");
            assert_eq!(parse_actor_kind(actor.kind_str()), kind);
        }
    }
}
