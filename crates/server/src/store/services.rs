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
                current_release_id, created_at
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
                '', ?35
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
        .bind(if unit.restart_sec == 0 { 5 } else { unit.restart_sec })
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

        let touch =
            |field: &str| update_mask.is_empty() || update_mask.iter().any(|m| m == field);

        // The unit, artifact and health check are replaced wholesale when named,
        // because a partial update of a oneof has no meaning — "set the git
        // branch but keep the url kind" is not a state the proto can express.
        if touch("unit") {
            if let Some(unit) = &service.unit {
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
                .bind(if unit.restart_sec == 0 { 5 } else { unit.restart_sec })
                .bind(encode_list(&unit.after))
                .bind(unit.cpu_affinity.trim())
                .bind(unit.nice.trim())
                .bind(unit.io_scheduling_class.trim())
                .bind(encode_map(&unit.extra_directives))
                .bind(id)
                .execute(self.pool())
                .await?;
            }
        }

        if touch("artifact") {
            if let Some(artifact) = &service.artifact {
                let cols = ArtifactColumns::from_proto(Some(artifact));
                sqlx::query(
                    "UPDATE services SET artifact_kind = ?1, artifact_url = ?2,
                       git_source_id = ?3, git_repo = ?4, git_branch = ?5,
                       git_build_command = ?6, git_artifact_path = ?7, git_auto_deploy = ?8
                     WHERE id = ?9",
                )
                .bind(&cols.kind)
                .bind(&cols.url)
                .bind(cols.source_id.clone())
                .bind(&cols.repo)
                .bind(&cols.branch)
                .bind(&cols.build_command)
                .bind(&cols.artifact_path)
                .bind(cols.auto_deploy as i64)
                .bind(id)
                .execute(self.pool())
                .await?;
            }
        }

        if touch("health_check") {
            if let Some(health) = &service.health_check {
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
                .execute(self.pool())
                .await?;
            }
        }

        if touch("name") && !service.name.trim().is_empty() {
            sqlx::query("UPDATE services SET name = ?1 WHERE id = ?2")
                .bind(service.name.trim())
                .bind(id)
                .execute(self.pool())
                .await?;
        }
        if touch("release_root") && !service.release_root.trim().is_empty() {
            sqlx::query("UPDATE services SET release_root = ?1 WHERE id = ?2")
                .bind(service.release_root.trim())
                .bind(id)
                .execute(self.pool())
                .await?;
        }
        if touch("keep_releases") && service.keep_releases != 0 {
            sqlx::query("UPDATE services SET keep_releases = ?1 WHERE id = ?2")
                .bind(service.keep_releases)
                .bind(id)
                .execute(self.pool())
                .await?;
        }
        if touch("secret_ids") {
            sqlx::query("UPDATE services SET secret_ids = ?1 WHERE id = ?2")
                .bind(encode_list(&service.secret_ids))
                .bind(id)
                .execute(self.pool())
                .await?;
        }
        if touch("env") {
            sqlx::query("UPDATE services SET env = ?1 WHERE id = ?2")
                .bind(encode_map(&service.env))
                .bind(id)
                .execute(self.pool())
                .await?;
        }
        // Moving a service between targets is deliberately not supported: the
        // releases and unit live on the old host, so a move would silently
        // orphan them.
        if touch("target_id")
            && !service.target_id.trim().is_empty()
            && service.target_id != existing.target_id
        {
            bail!(
                "a service cannot be moved between targets; \
                 delete it and create it on the new target instead"
            );
        }

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
    git_build_command, git_artifact_path, git_auto_deploy,
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
                out.source_id = Some(git.source_id.trim().to_string())
                    .filter(|s| !s.is_empty());
                out.repo = git.repo.trim().to_string();
                out.branch = git.branch.trim().to_string();
                out.build_command = git.build_command.trim().to_string();
                out.artifact_path = git.artifact_path.trim().to_string();
                out.auto_deploy = git.auto_deploy_on_push;
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
            timeout_seconds: health.map(|h| h.timeout_seconds).filter(|t| *t > 0).unwrap_or(10),
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
mod tests {
    use super::*;
    use crate::store::TargetInput;

    async fn store_with_target() -> (Store, String) {
        let store = Store::open_in_memory().await.expect("open");
        let target = store
            .create_target(&TargetInput {
                name: "box".to_string(),
                host: "10.0.0.1".to_string(),
                ..Default::default()
            })
            .await
            .expect("target");
        (store, target.id)
    }

    fn service(target_id: &str) -> Service {
        Service {
            target_id: target_id.to_string(),
            name: "bot".to_string(),
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn a_created_service_gets_defaults_for_everything_unset() {
        let (store, target_id) = store_with_target().await;
        let created = store.create_service(&service(&target_id)).await.expect("create");

        assert!(created.id.starts_with("svc_"));
        // Release root is derived from the name.
        assert_eq!(created.release_root, "/opt/bot");
        assert_eq!(created.keep_releases, crate::systemd::DEFAULT_KEEP_RELEASES);
        assert!(created.current_release_id.is_empty());

        let unit = created.unit.expect("unit");
        assert_eq!(unit.restart, "always");
        assert_eq!(unit.restart_sec, 5);

        // An unspecified artifact means the CLI will push one.
        assert!(matches!(
            created.artifact.expect("artifact").kind,
            Some(artifact_source::Kind::DirectUpload(true))
        ));

        let health = created.health_check.expect("health");
        assert!(matches!(health.kind, Some(health_check::Kind::SystemdActive(true))));
        assert_eq!(health.timeout_seconds, 10);
        assert_eq!(health.retries, 3);
    }

    #[tokio::test]
    async fn an_explicit_release_root_is_kept() {
        let (store, target_id) = store_with_target().await;
        let created = store
            .create_service(&Service {
                release_root: "/srv/custom".to_string(),
                ..service(&target_id)
            })
            .await
            .expect("create");
        assert_eq!(created.release_root, "/srv/custom");
    }

    #[tokio::test]
    async fn a_service_requires_an_existing_target_and_a_name() {
        let (store, target_id) = store_with_target().await;

        let orphan = store
            .create_service(&Service {
                target_id: "tgt_missing".to_string(),
                ..service(&target_id)
            })
            .await;
        assert!(orphan.is_err());

        let nameless = store
            .create_service(&Service {
                name: "  ".to_string(),
                ..service(&target_id)
            })
            .await;
        assert!(nameless.is_err());
    }

    #[tokio::test]
    async fn two_services_on_one_target_cannot_share_a_name() {
        let (store, target_id) = store_with_target().await;
        store.create_service(&service(&target_id)).await.expect("first");
        let error = store
            .create_service(&service(&target_id))
            .await
            .expect_err("second");
        assert!(error.to_string().contains("already exists"), "got: {error}");
    }

    #[tokio::test]
    async fn the_same_service_name_is_allowed_on_a_different_target() {
        let (store, target_id) = store_with_target().await;
        let other = store
            .create_target(&TargetInput {
                name: "box-2".to_string(),
                host: "10.0.0.2".to_string(),
                ..Default::default()
            })
            .await
            .expect("target");

        store.create_service(&service(&target_id)).await.expect("first");
        store.create_service(&service(&other.id)).await.expect("second");
    }

    #[tokio::test]
    async fn each_artifact_kind_round_trips() {
        let (store, target_id) = store_with_target().await;

        let url_service = store
            .create_service(&Service {
                name: "from-url".to_string(),
                artifact: Some(ArtifactSource {
                    kind: Some(artifact_source::Kind::Url(
                        "https://example.com/bot".to_string(),
                    )),
                }),
                ..service(&target_id)
            })
            .await
            .expect("create");
        assert!(matches!(
            url_service.artifact.expect("artifact").kind,
            Some(artifact_source::Kind::Url(url)) if url == "https://example.com/bot"
        ));

        let git_service = store
            .create_service(&Service {
                name: "from-git".to_string(),
                artifact: Some(ArtifactSource {
                    kind: Some(artifact_source::Kind::Git(GitSource {
                        source_id: String::new(),
                        repo: "owner/bot".to_string(),
                        branch: "main".to_string(),
                        build_command: "cargo build --release".to_string(),
                        artifact_path: "target/release/bot".to_string(),
                        auto_deploy_on_push: true,
                    })),
                }),
                ..service(&target_id)
            })
            .await
            .expect("create");
        let git = match git_service.artifact.expect("artifact").kind {
            Some(artifact_source::Kind::Git(git)) => git,
            other => panic!("expected git, got {other:?}"),
        };
        assert_eq!(git.repo, "owner/bot");
        assert_eq!(git.branch, "main");
        assert_eq!(git.build_command, "cargo build --release");
        assert!(git.auto_deploy_on_push);
    }

    #[tokio::test]
    async fn each_health_check_kind_round_trips() {
        let (store, target_id) = store_with_target().await;

        let http = store
            .create_service(&Service {
                name: "http-checked".to_string(),
                health_check: Some(HealthCheck {
                    kind: Some(health_check::Kind::HttpUrl(
                        "http://127.0.0.1:9000/healthz".to_string(),
                    )),
                    timeout_seconds: 5,
                    retries: 10,
                    initial_delay_seconds: 1,
                }),
                ..service(&target_id)
            })
            .await
            .expect("create");
        let health = http.health_check.expect("health");
        assert!(matches!(
            health.kind,
            Some(health_check::Kind::HttpUrl(url)) if url.ends_with("/healthz")
        ));
        assert_eq!(health.timeout_seconds, 5);
        assert_eq!(health.retries, 10);
        assert_eq!(health.initial_delay_seconds, 1);

        let command = store
            .create_service(&Service {
                name: "cmd-checked".to_string(),
                health_check: Some(HealthCheck {
                    kind: Some(health_check::Kind::Command("/usr/bin/true".to_string())),
                    ..Default::default()
                }),
                ..service(&target_id)
            })
            .await
            .expect("create");
        assert!(matches!(
            command.health_check.expect("health").kind,
            Some(health_check::Kind::Command(c)) if c == "/usr/bin/true"
        ));
    }

    #[tokio::test]
    async fn the_full_unit_definition_including_latency_knobs_round_trips() {
        let (store, target_id) = store_with_target().await;
        let created = store
            .create_service(&Service {
                unit: Some(SystemdUnit {
                    unit_name: "bot.service".to_string(),
                    description: "The bot".to_string(),
                    exec_args: "--fast".to_string(),
                    working_directory: "/var/lib/bot".to_string(),
                    user: "bot".to_string(),
                    group: "bot".to_string(),
                    restart: "on-failure".to_string(),
                    restart_sec: 30,
                    after: vec!["postgresql.service".to_string()],
                    cpu_affinity: "4-7".to_string(),
                    nice: "-15".to_string(),
                    io_scheduling_class: "realtime".to_string(),
                    extra_directives: std::collections::HashMap::from([(
                        "LimitNOFILE".to_string(),
                        "1048576".to_string(),
                    )]),
                }),
                env: std::collections::HashMap::from([(
                    "LOG".to_string(),
                    "debug".to_string(),
                )]),
                secret_ids: vec!["sec_a".to_string()],
                keep_releases: 9,
                ..service(&target_id)
            })
            .await
            .expect("create");

        let unit = created.unit.expect("unit");
        assert_eq!(unit.cpu_affinity, "4-7");
        assert_eq!(unit.nice, "-15");
        assert_eq!(unit.io_scheduling_class, "realtime");
        assert_eq!(unit.restart, "on-failure");
        assert_eq!(unit.restart_sec, 30);
        assert_eq!(unit.after, vec!["postgresql.service".to_string()]);
        assert_eq!(
            unit.extra_directives.get("LimitNOFILE").map(String::as_str),
            Some("1048576")
        );
        assert_eq!(created.env.get("LOG").map(String::as_str), Some("debug"));
        assert_eq!(created.secret_ids, vec!["sec_a".to_string()]);
        assert_eq!(created.keep_releases, 9);
    }

    #[tokio::test]
    async fn listing_can_be_filtered_by_target() {
        let (store, target_id) = store_with_target().await;
        let other = store
            .create_target(&TargetInput {
                name: "other".to_string(),
                host: "10.0.0.9".to_string(),
                ..Default::default()
            })
            .await
            .expect("target");

        store.create_service(&service(&target_id)).await.expect("a");
        store
            .create_service(&Service {
                name: "elsewhere".to_string(),
                ..service(&other.id)
            })
            .await
            .expect("b");

        assert_eq!(store.list_services("", 50, 0).await.expect("all").len(), 2);
        let filtered = store.list_services(&target_id, 50, 0).await.expect("filtered");
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].name, "bot");
        assert_eq!(store.count_services().await.expect("count"), 2);
    }

    #[tokio::test]
    async fn deleting_a_target_deletes_its_services() {
        // The service's unit and releases live on that host; keeping the row
        // after the target is gone would leave an unreachable service.
        let (store, target_id) = store_with_target().await;
        store.create_service(&service(&target_id)).await.expect("create");

        store.delete_target(&target_id).await.expect("delete target");
        assert!(store.list_services("", 50, 0).await.expect("list").is_empty());
    }

    #[tokio::test]
    async fn a_masked_update_replaces_only_the_named_parts() {
        let (store, target_id) = store_with_target().await;
        let created = store.create_service(&service(&target_id)).await.expect("create");

        let updated = store
            .update_service(
                &created.id,
                &Service {
                    unit: Some(SystemdUnit {
                        cpu_affinity: "0-1".to_string(),
                        ..Default::default()
                    }),
                    release_root: "/ignored".to_string(),
                    ..Default::default()
                },
                &["unit".to_string()],
            )
            .await
            .expect("update");

        assert_eq!(updated.unit.expect("unit").cpu_affinity, "0-1");
        // Outside the mask.
        assert_eq!(updated.release_root, "/opt/bot");
    }

    #[tokio::test]
    async fn moving_a_service_to_another_target_is_refused() {
        let (store, target_id) = store_with_target().await;
        let created = store.create_service(&service(&target_id)).await.expect("create");
        let other = store
            .create_target(&TargetInput {
                name: "elsewhere".to_string(),
                host: "10.0.0.3".to_string(),
                ..Default::default()
            })
            .await
            .expect("target");

        let error = store
            .update_service(
                &created.id,
                &Service {
                    target_id: other.id,
                    ..Default::default()
                },
                &["target_id".to_string()],
            )
            .await
            .expect_err("must refuse");
        assert!(error.to_string().contains("cannot be moved"), "got: {error}");
    }

    #[tokio::test]
    async fn the_current_release_pointer_can_be_set() {
        let (store, target_id) = store_with_target().await;
        let created = store.create_service(&service(&target_id)).await.expect("create");

        store
            .set_current_release(&created.id, "rel_abc")
            .await
            .expect("set");
        let reloaded = store.get_service(&created.id).await.expect("get").expect("some");
        assert_eq!(reloaded.current_release_id, "rel_abc");
    }

    #[tokio::test]
    async fn a_push_only_matches_services_with_auto_deploy_enabled() {
        let (store, target_id) = store_with_target().await;

        let git = |auto: bool, name: &str, branch: &str| Service {
            name: name.to_string(),
            artifact: Some(ArtifactSource {
                kind: Some(artifact_source::Kind::Git(GitSource {
                    repo: "Owner/Bot".to_string(),
                    branch: branch.to_string(),
                    auto_deploy_on_push: auto,
                    ..Default::default()
                })),
            }),
            ..service(&target_id)
        };

        store.create_service(&git(true, "auto", "main")).await.expect("a");
        store.create_service(&git(false, "manual", "main")).await.expect("b");
        store.create_service(&git(true, "other-branch", "dev")).await.expect("c");

        // Source id is empty here (deploy-key style), which must still match.
        let matched = store.services_for_push("", "owner/bot", "main").await.expect("match");
        assert_eq!(matched.len(), 1, "only the auto-deploy service on that branch");
        assert_eq!(matched[0].name, "auto");
    }

    #[tokio::test]
    async fn repo_matching_is_case_insensitive_but_branch_matching_is_not() {
        // GitHub treats owner/repo case-insensitively; git refs are exact.
        let (store, target_id) = store_with_target().await;
        store
            .create_service(&Service {
                name: "svc".to_string(),
                artifact: Some(ArtifactSource {
                    kind: Some(artifact_source::Kind::Git(GitSource {
                        repo: "Owner/Bot".to_string(),
                        branch: "Main".to_string(),
                        auto_deploy_on_push: true,
                        ..Default::default()
                    })),
                }),
                ..service(&target_id)
            })
            .await
            .expect("create");

        assert_eq!(
            store.services_for_push("", "OWNER/BOT", "Main").await.expect("m").len(),
            1
        );
        assert!(
            store.services_for_push("", "owner/bot", "main").await.expect("m").is_empty(),
            "branch comparison must be exact"
        );
    }
}
