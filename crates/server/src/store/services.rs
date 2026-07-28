//! Service persistence — one deployable systemd unit on one target.
//!
//! The proto nests `ArtifactSource`, `SystemdUnit` and `HealthCheck` inside
//! `Service`, and each of the first and last is a `oneof`. Those are flattened
//! into columns with a discriminator (`artifact_kind`, `health_kind`) so the
//! whole service is one row and one query.

use anyhow::bail;
use nudo_proto::{
    ArtifactSource, GitSource, HealthCheck, Service, SystemdUnit, artifact_source, health_check,
};
use sqlx::Row;
use sqlx::sqlite::SqliteRow;
// The SQL strings below are composed only from `const` fragments in this file
// plus bound parameters; no caller-supplied value is ever interpolated, which is
// what `AssertSqlSafe` asserts.
use sqlx::AssertSqlSafe;

use super::{
    Store, decode_list, decode_map, encode_list, encode_map, from_db_time, new_id, now_string,
};

impl Store {
    /// Creates a service. The target must exist, and defaults are filled in for
    /// anything the client left unset.
    pub async fn create_service(&self, service: &Service) -> anyhow::Result<Service> {
        let name = service.name.trim();
        if name.is_empty() {
            bail!("a service needs a name");
        }
        if self.get_target(&service.target_id).await?.is_none() {
            bail!("no such target: {}", service.target_id);
        }

        let id = new_id("svc");
        // A release root under /opt keyed by service name is the convention;
        // an explicit value always wins.
        let release_root = if service.release_root.trim().is_empty() {
            format!("/opt/{name}")
        } else {
            service.release_root.trim().to_string()
        };

        let unit = service.unit.clone().unwrap_or_default();
        let artifact = ArtifactColumns::from_proto(service.artifact.as_ref());
        let health = HealthColumns::from_proto(service.health_check.as_ref());

        sqlx::query(
            "INSERT INTO services (
                id, target_id, name,
                artifact_kind, artifact_url, git_source_id, git_repo, git_branch,
                git_build_command, git_artifact_path, git_auto_deploy,
                unit_name, unit_description, exec_args, working_directory,
                unit_user, unit_group, restart, restart_sec, after_units,
                cpu_affinity, nice, io_scheduling_class, extra_directives,
                health_kind, health_http_url, health_command,
                health_timeout_seconds, health_retries, health_initial_delay_seconds,
                release_root, keep_releases, secret_ids, env,
                current_release_id, created_at, git_build_host_id
             ) VALUES (
                ?1, ?2, ?3,
                ?4, ?5, ?6, ?7, ?8,
                ?9, ?10, ?11,
                ?12, ?13, ?14, ?15,
                ?16, ?17, ?18, ?19, ?20,
                ?21, ?22, ?23, ?24,
                ?25, ?26, ?27,
                ?28, ?29, ?30,
                ?31, ?32, ?33, ?34,
                '', ?35, ?36
             )",
        )
        .bind(&id)
        .bind(&service.target_id)
        .bind(name)
        .bind(&artifact.kind)
        .bind(&artifact.url)
        .bind(artifact.source_id.clone())
        .bind(&artifact.repo)
        .bind(&artifact.branch)
        .bind(&artifact.build_command)
        .bind(&artifact.artifact_path)
        .bind(artifact.auto_deploy as i64)
        .bind(unit.unit_name.trim())
        .bind(unit.description.trim())
        .bind(unit.exec_args.trim())
        .bind(unit.working_directory.trim())
        .bind(unit.user.trim())
        .bind(unit.group.trim())
        .bind(if unit.restart.trim().is_empty() {
            "always"
        } else {
            unit.restart.trim()
        })
        .bind(if unit.restart_sec == 0 {
            5
        } else {
            unit.restart_sec
        })
        .bind(encode_list(&unit.after))
        .bind(unit.cpu_affinity.trim())
        .bind(unit.nice.trim())
        .bind(unit.io_scheduling_class.trim())
        .bind(encode_map(&unit.extra_directives))
        .bind(&health.kind)
        .bind(&health.http_url)
        .bind(&health.command)
        .bind(health.timeout_seconds)
        .bind(health.retries)
        .bind(health.initial_delay_seconds)
        .bind(&release_root)
        .bind(if service.keep_releases == 0 {
            crate::systemd::DEFAULT_KEEP_RELEASES
        } else {
            service.keep_releases
        })
        .bind(encode_list(&service.secret_ids))
        .bind(encode_map(&service.env))
        .bind(now_string())
        .bind(&artifact.build_host_id)
        .execute(self.pool())
        .await
        .map_err(|e| {
            if super::targets::is_unique_violation(&e) {
                anyhow::anyhow!("a service named {name:?} already exists on that target")
            } else {
                e.into()
            }
        })?;

        self.get_service(&id)
            .await?
            .ok_or_else(|| anyhow::anyhow!("service vanished immediately after creation"))
    }

    pub async fn get_service(&self, id: &str) -> anyhow::Result<Option<Service>> {
        let row = sqlx::query(AssertSqlSafe(format!("{SERVICE_SELECT} WHERE id = ?1")))
            .bind(id)
            .fetch_optional(self.pool())
            .await?;
        Ok(row.as_ref().map(row_to_service))
    }

    /// Lists services, optionally restricted to one target.
    pub async fn list_services(
        &self,
        target_id: &str,
        limit: i64,
        offset: i64,
    ) -> anyhow::Result<Vec<Service>> {
        let rows = if target_id.trim().is_empty() {
            sqlx::query(AssertSqlSafe(format!(
                "{SERVICE_SELECT} ORDER BY created_at DESC, id DESC LIMIT ?1 OFFSET ?2"
            )))
            .bind(limit)
            .bind(offset)
            .fetch_all(self.pool())
            .await?
        } else {
            sqlx::query(AssertSqlSafe(format!(
                "{SERVICE_SELECT} WHERE target_id = ?1 \
                 ORDER BY created_at DESC, id DESC LIMIT ?2 OFFSET ?3"
            )))
            .bind(target_id.trim())
            .bind(limit)
            .bind(offset)
            .fetch_all(self.pool())
            .await?
        };

        Ok(rows.iter().map(row_to_service).collect())
    }

    pub async fn count_services(&self) -> anyhow::Result<i64> {
        let (count,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM services")
            .fetch_one(self.pool())
            .await?;
        Ok(count)
    }

    /// Finds the services a GitHub push should deploy: those bound to this
    /// source, repo and branch with auto-deploy enabled.
    pub async fn services_for_push(
        &self,
        source_id: &str,
        repo: &str,
        branch: &str,
    ) -> anyhow::Result<Vec<Service>> {
        let rows = sqlx::query(AssertSqlSafe(format!(
            // COALESCE because an unset source is stored as NULL to satisfy the
            // foreign key, and `NULL = ''` is never true in SQL — without it a
            // deploy-key service (which has no App source) would match nothing.
            "{SERVICE_SELECT} WHERE COALESCE(git_source_id, '') = ?1 \
               AND lower(git_repo) = lower(?2) \
               AND git_branch = ?3 \
               AND git_auto_deploy = 1"
        )))
        .bind(source_id.trim())
        .bind(repo)
        .bind(branch)
        .fetch_all(self.pool())
        .await?;
        Ok(rows.iter().map(row_to_service).collect())
    }

    /// Applies a field mask to a service.
    pub async fn update_service(
        &self,
        id: &str,
        service: &Service,
        update_mask: &[String],
    ) -> anyhow::Result<Service> {
        let existing = self
            .get_service(id)
            .await?
            .ok_or_else(|| anyhow::anyhow!("no such service: {id}"))?;

        let touch = |field: &str| update_mask.is_empty() || update_mask.iter().any(|m| m == field);

        // Moving a service between targets is deliberately not supported: the
        // releases and unit live on the old host, so a move would silently
        // orphan them. Validate this before writing any other masked field.
        if touch("target_id")
            && !service.target_id.trim().is_empty()
            && service.target_id != existing.target_id
        {
            bail!(
                "a service cannot be moved between targets; \
                 delete it and create it on the new target instead"
            );
        }

        // A mask may touch several column groups. They are one logical update,
        // so either every statement commits or none of them does.
        let mut transaction = self.pool().begin().await?;

        // The unit, artifact and health check are replaced wholesale when named,
        // because a partial update of a oneof has no meaning — "set the git
        // branch but keep the url kind" is not a state the proto can express.
        if touch("unit")
            && let Some(unit) = &service.unit
        {
            sqlx::query(
                "UPDATE services SET unit_name = ?1, unit_description = ?2, exec_args = ?3,
                       working_directory = ?4, unit_user = ?5, unit_group = ?6, restart = ?7,
                       restart_sec = ?8, after_units = ?9, cpu_affinity = ?10, nice = ?11,
                       io_scheduling_class = ?12, extra_directives = ?13
                     WHERE id = ?14",
            )
            .bind(unit.unit_name.trim())
            .bind(unit.description.trim())
            .bind(unit.exec_args.trim())
            .bind(unit.working_directory.trim())
            .bind(unit.user.trim())
            .bind(unit.group.trim())
            .bind(if unit.restart.trim().is_empty() {
                "always"
            } else {
                unit.restart.trim()
            })
            .bind(if unit.restart_sec == 0 {
                5
            } else {
                unit.restart_sec
            })
            .bind(encode_list(&unit.after))
            .bind(unit.cpu_affinity.trim())
            .bind(unit.nice.trim())
            .bind(unit.io_scheduling_class.trim())
            .bind(encode_map(&unit.extra_directives))
            .bind(id)
            .execute(&mut *transaction)
            .await?;
        }

        if touch("artifact")
            && let Some(artifact) = &service.artifact
        {
            let cols = ArtifactColumns::from_proto(Some(artifact));
            sqlx::query(
                "UPDATE services SET artifact_kind = ?1, artifact_url = ?2,
                       git_source_id = ?3, git_repo = ?4, git_branch = ?5,
                       git_build_command = ?6, git_artifact_path = ?7, git_auto_deploy = ?8,
                       git_build_host_id = ?9
                     WHERE id = ?10",
            )
            .bind(&cols.kind)
            .bind(&cols.url)
            .bind(cols.source_id.clone())
            .bind(&cols.repo)
            .bind(&cols.branch)
            .bind(&cols.build_command)
            .bind(&cols.artifact_path)
            .bind(cols.auto_deploy as i64)
            .bind(&cols.build_host_id)
            .bind(id)
            .execute(&mut *transaction)
            .await?;
        }

        if touch("health_check")
            && let Some(health) = &service.health_check
        {
            let cols = HealthColumns::from_proto(Some(health));
            sqlx::query(
                "UPDATE services SET health_kind = ?1, health_http_url = ?2,
                       health_command = ?3, health_timeout_seconds = ?4, health_retries = ?5,
                       health_initial_delay_seconds = ?6
                     WHERE id = ?7",
            )
            .bind(&cols.kind)
            .bind(&cols.http_url)
            .bind(&cols.command)
            .bind(cols.timeout_seconds)
            .bind(cols.retries)
            .bind(cols.initial_delay_seconds)
            .bind(id)
            .execute(&mut *transaction)
            .await?;
        }

        if touch("name") && !service.name.trim().is_empty() {
            sqlx::query("UPDATE services SET name = ?1 WHERE id = ?2")
                .bind(service.name.trim())
                .bind(id)
                .execute(&mut *transaction)
                .await
                .map_err(|error| {
                    if super::targets::is_unique_violation(&error) {
                        anyhow::anyhow!(
                            "a service named {:?} already exists on that target",
                            service.name.trim()
                        )
                    } else {
                        error.into()
                    }
                })?;
        }
        if touch("release_root") && !service.release_root.trim().is_empty() {
            sqlx::query("UPDATE services SET release_root = ?1 WHERE id = ?2")
                .bind(service.release_root.trim())
                .bind(id)
                .execute(&mut *transaction)
                .await?;
        }
        if touch("keep_releases") && service.keep_releases != 0 {
            sqlx::query("UPDATE services SET keep_releases = ?1 WHERE id = ?2")
                .bind(service.keep_releases)
                .bind(id)
                .execute(&mut *transaction)
                .await?;
        }
        if touch("secret_ids") {
            sqlx::query("UPDATE services SET secret_ids = ?1 WHERE id = ?2")
                .bind(encode_list(&service.secret_ids))
                .bind(id)
                .execute(&mut *transaction)
                .await?;
        }
        if touch("env") {
            sqlx::query("UPDATE services SET env = ?1 WHERE id = ?2")
                .bind(encode_map(&service.env))
                .bind(id)
                .execute(&mut *transaction)
                .await?;
        }

        transaction.commit().await?;
        self.get_service(id)
            .await?
            .ok_or_else(|| anyhow::anyhow!("service {id} vanished during update"))
    }

    pub async fn delete_service(&self, id: &str) -> anyhow::Result<()> {
        let result = sqlx::query("DELETE FROM services WHERE id = ?1")
            .bind(id)
            .execute(self.pool())
            .await?;
        if result.rows_affected() == 0 {
            bail!("no such service: {id}");
        }
        Ok(())
    }

    /// Points a service at the release that is now live.
    pub async fn set_current_release(
        &self,
        service_id: &str,
        release_id: &str,
    ) -> anyhow::Result<()> {
        sqlx::query("UPDATE services SET current_release_id = ?1 WHERE id = ?2")
            .bind(release_id)
            .bind(service_id)
            .execute(self.pool())
            .await?;
        Ok(())
    }
}

const SERVICE_SELECT: &str = "SELECT
    id, target_id, name,
    artifact_kind, artifact_url, git_source_id, git_repo, git_branch,
    git_build_command, git_artifact_path, git_auto_deploy, git_build_host_id,
    unit_name, unit_description, exec_args, working_directory,
    unit_user, unit_group, restart, restart_sec, after_units,
    cpu_affinity, nice, io_scheduling_class, extra_directives,
    health_kind, health_http_url, health_command,
    health_timeout_seconds, health_retries, health_initial_delay_seconds,
    release_root, keep_releases, secret_ids, env, current_release_id, created_at
  FROM services";

/// The artifact `oneof` flattened into columns.
struct ArtifactColumns {
    kind: String,
    url: String,
    source_id: Option<String>,
    repo: String,
    branch: String,
    build_command: String,
    artifact_path: String,
    auto_deploy: bool,
    build_host_id: String,
}

impl ArtifactColumns {
    fn from_proto(artifact: Option<&ArtifactSource>) -> Self {
        let mut out = Self {
            kind: "direct_upload".to_string(),
            url: String::new(),
            source_id: None,
            repo: String::new(),
            branch: String::new(),
            build_command: String::new(),
            artifact_path: String::new(),
            auto_deploy: false,
            build_host_id: String::new(),
        };

        match artifact.and_then(|a| a.kind.as_ref()) {
            Some(artifact_source::Kind::Url(url)) => {
                out.kind = "url".to_string();
                out.url = url.trim().to_string();
            }
            Some(artifact_source::Kind::Git(git)) => {
                out.kind = "git".to_string();
                // Stored as NULL rather than "" so the foreign key to sources
                // is satisfied when no source is set yet.
                out.source_id = Some(git.source_id.trim().to_string()).filter(|s| !s.is_empty());
                out.repo = git.repo.trim().to_string();
                out.branch = git.branch.trim().to_string();
                out.build_command = git.build_command.trim().to_string();
                out.artifact_path = git.artifact_path.trim().to_string();
                out.auto_deploy = git.auto_deploy_on_push;
                // Empty means "use the instance default"; `local` pins the
                // control plane. Both are stored verbatim — see 0007.
                out.build_host_id = git.build_host_id.trim().to_string();
            }
            // `direct_upload: false` is indistinguishable from unset in
            // proto3, and both mean the same thing here.
            Some(artifact_source::Kind::DirectUpload(_)) | None => {}
        }

        out
    }
}

/// The health-check `oneof` flattened into columns.
struct HealthColumns {
    kind: String,
    http_url: String,
    command: String,
    timeout_seconds: u32,
    retries: u32,
    initial_delay_seconds: u32,
}

impl HealthColumns {
    fn from_proto(health: Option<&HealthCheck>) -> Self {
        let mut out = Self {
            kind: "systemd_active".to_string(),
            http_url: String::new(),
            command: String::new(),
            // A zero timeout would mean "give up instantly", so unset values
            // become working defaults.
            timeout_seconds: health
                .map(|h| h.timeout_seconds)
                .filter(|t| *t > 0)
                .unwrap_or(10),
            retries: health.map(|h| h.retries).filter(|r| *r > 0).unwrap_or(3),
            initial_delay_seconds: health.map(|h| h.initial_delay_seconds).unwrap_or(2),
        };

        match health.and_then(|h| h.kind.as_ref()) {
            Some(health_check::Kind::HttpUrl(url)) => {
                out.kind = "http".to_string();
                out.http_url = url.trim().to_string();
            }
            Some(health_check::Kind::Command(command)) => {
                out.kind = "command".to_string();
                out.command = command.trim().to_string();
            }
            Some(health_check::Kind::SystemdActive(_)) | None => {}
        }

        out
    }
}

fn row_to_service(row: &SqliteRow) -> Service {
    let artifact_kind: String = row.get("artifact_kind");
    let artifact = ArtifactSource {
        kind: Some(match artifact_kind.as_str() {
            "url" => artifact_source::Kind::Url(row.get("artifact_url")),
            "git" => artifact_source::Kind::Git(GitSource {
                source_id: row
                    .get::<Option<String>, _>("git_source_id")
                    .unwrap_or_default(),
                repo: row.get("git_repo"),
                branch: row.get("git_branch"),
                build_command: row.get("git_build_command"),
                artifact_path: row.get("git_artifact_path"),
                auto_deploy_on_push: row.get::<i64, _>("git_auto_deploy") != 0,
                build_host_id: row.get("git_build_host_id"),
            }),
            _ => artifact_source::Kind::DirectUpload(true),
        }),
    };

    let health_kind: String = row.get("health_kind");
    let health_check = HealthCheck {
        kind: Some(match health_kind.as_str() {
            "http" => health_check::Kind::HttpUrl(row.get("health_http_url")),
            "command" => health_check::Kind::Command(row.get("health_command")),
            _ => health_check::Kind::SystemdActive(true),
        }),
        timeout_seconds: row.get::<i64, _>("health_timeout_seconds") as u32,
        retries: row.get::<i64, _>("health_retries") as u32,
        initial_delay_seconds: row.get::<i64, _>("health_initial_delay_seconds") as u32,
    };

    Service {
        id: row.get("id"),
        target_id: row.get("target_id"),
        name: row.get("name"),
        artifact: Some(artifact),
        unit: Some(SystemdUnit {
            unit_name: row.get("unit_name"),
            description: row.get("unit_description"),
            exec_args: row.get("exec_args"),
            working_directory: row.get("working_directory"),
            user: row.get("unit_user"),
            group: row.get("unit_group"),
            restart: row.get("restart"),
            restart_sec: row.get::<i64, _>("restart_sec") as u32,
            after: decode_list(&row.get::<String, _>("after_units")),
            cpu_affinity: row.get("cpu_affinity"),
            nice: row.get("nice"),
            io_scheduling_class: row.get("io_scheduling_class"),
            extra_directives: decode_map(&row.get::<String, _>("extra_directives")),
        }),
        health_check: Some(health_check),
        release_root: row.get("release_root"),
        keep_releases: row.get::<i64, _>("keep_releases") as u32,
        secret_ids: decode_list(&row.get::<String, _>("secret_ids")),
        env: decode_map(&row.get::<String, _>("env")),
        current_release_id: row.get("current_release_id"),
        created_at: nudo_proto::to_timestamp_opt(from_db_time(&row.get::<String, _>("created_at"))),
    }
}

#[cfg(test)]
mod tests;
