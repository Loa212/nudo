use anyhow::{Context, anyhow, bail};
use nudo_proto::{Release, deployment};
use tokio::sync::mpsc;

use crate::crypto::sha256_hex;
use crate::health;
use crate::ssh::{OutputLine, quote};
use crate::systemd::{self, BINARY_NAME, ReleasePaths};

use super::{DeployOptions, Engine};

impl Engine {
    /// Runs a deployment to completion, recording every transition.
    ///
    /// Never returns an error: a deployment's outcome belongs in its row and its
    /// event stream, not in a caller that has already been handed an id.
    pub(super) async fn run_deploy(&self, deployment_id: &str, options: DeployOptions) {
        match self.try_deploy(deployment_id, &options).await {
            Ok(()) => {}
            Err(error) => {
                let message = format!("{error:#}");
                tracing::warn!(deployment = %deployment_id, %message, "deployment failed");

                self.emit(deployment_id, &message, true).await;
                let _ = self
                    .store
                    .set_deployment_error(deployment_id, &message)
                    .await;

                // Distinguish a cancel from a genuine failure so the history
                // does not report user intent as a fault.
                let cancelled = self
                    .store
                    .cancel_requested(deployment_id)
                    .await
                    .unwrap_or(false);
                let final_status = if cancelled {
                    deployment::Status::Cancelled
                } else {
                    deployment::Status::Failed
                };

                if !cancelled && options.auto_rollback_on_failure {
                    if let Err(rollback_error) = self.rollback_after_failure(deployment_id).await {
                        self.emit(
                            deployment_id,
                            &format!("rollback also failed: {rollback_error:#}"),
                            true,
                        )
                        .await;
                    } else {
                        self.finish(deployment_id, deployment::Status::RolledBack)
                            .await;
                        return;
                    }
                }

                self.finish(deployment_id, final_status).await;
            }
        }
    }

    /// The deploy proper.
    async fn try_deploy(&self, deployment_id: &str, options: &DeployOptions) -> anyhow::Result<()> {
        let deployment = self
            .store
            .get_deployment(deployment_id)
            .await?
            .ok_or_else(|| anyhow!("deployment {deployment_id} disappeared"))?;

        let service = self
            .store
            .get_service(&deployment.service_id)
            .await?
            .ok_or_else(|| anyhow!("service {} disappeared", deployment.service_id))?;

        let target = self
            .store
            .get_target(&service.target_id)
            .await?
            .ok_or_else(|| anyhow!("target {} disappeared", service.target_id))?;

        let paths = ReleasePaths::new(&service.release_root);
        let unit_name = systemd::unit_file_name(&service);
        let release_id = crate::store::new_id("rel");

        self.emit(
            deployment_id,
            &format!(
                "deploying {} to {} ({}) as release {release_id}",
                service.name, target.name, target.host
            ),
            false,
        )
        .await;

        // ---- obtain the artifact ----
        self.check_cancelled(deployment_id).await?;
        let artifact = self
            .obtain_artifact(deployment_id, &service, options)
            .await?;

        let digest = sha256_hex(&artifact.bytes);
        self.emit(
            deployment_id,
            &format!(
                "artifact ready: {} bytes, sha256 {}",
                artifact.bytes.len(),
                &digest[..16]
            ),
            false,
        )
        .await;

        // ---- connect ----
        self.check_cancelled(deployment_id).await?;
        self.transition(deployment_id, deployment::Status::Uploading)
            .await;

        let session = self
            .connect(&target)
            .await
            .with_context(|| format!("connecting to {}", target.host))?;

        // ---- stage and upload ----
        // The binary lands in a staging directory first and is moved into
        // `releases/` only once it is complete and verified, so an interrupted
        // upload can never leave something the symlink could point at.
        let staging = paths.staging_dir(&release_id);
        let staged_binary = format!("{staging}/{BINARY_NAME}");

        session
            .exec(&format!(
                "mkdir -p {} {} {}",
                quote(&paths.releases_dir()),
                quote(&staging),
                quote(paths.root())
            ))
            .await?
            .require_success("preparing the release directories")?;

        self.emit(
            deployment_id,
            &format!("uploading to {staged_binary}"),
            false,
        )
        .await;

        let (progress_tx, mut progress_rx) = mpsc::channel::<OutputLine>(64);
        let progress_engine = self.clone();
        let progress_deployment = deployment_id.to_string();
        let progress_task = tokio::spawn(async move {
            while let Some(line) = progress_rx.recv().await {
                progress_engine
                    .emit(&progress_deployment, &line.text, line.stderr)
                    .await;
            }
        });

        let upload = session
            .upload_file(
                &staged_binary,
                &artifact.bytes,
                Some("0755"),
                Some(&progress_tx),
            )
            .await;
        drop(progress_tx);
        let _ = progress_task.await;
        upload.context("uploading the release binary")?;

        // Move into place as one rename, which is atomic within a filesystem.
        let release_dir = paths.release_dir(&release_id);
        session
            .exec(&format!(
                "rm -rf {dir} && mv {staging} {dir}",
                dir = quote(&release_dir),
                staging = quote(&staging)
            ))
            .await?
            .require_success("moving the release into place")?;

        let release = self
            .store
            .create_release(&Release {
                id: release_id.clone(),
                service_id: service.id.clone(),
                git_sha: artifact.git_sha.clone(),
                git_ref: artifact.git_ref.clone(),
                artifact_digest: digest.clone(),
                artifact_bytes: artifact.bytes.len() as u64,
                path: release_dir.clone(),
                created_at: None,
            })
            .await?;
        self.store
            .set_deployment_release(deployment_id, &release.id, &artifact.git_sha)
            .await?;

        // ---- secrets ----
        // Written before activation so the unit's EnvironmentFile is correct the
        // first time it starts, and 0600 owned by the service user so another
        // account on the box cannot read them.
        self.check_cancelled(deployment_id).await?;
        if !service.secret_ids.is_empty() {
            let resolved = self
                .store
                .resolve_service_secrets(&self.secret_key, &service.secret_ids)
                .await
                .context("resolving the service's secrets")?;

            session
                .write_file(
                    &paths.env_file(),
                    systemd::render_env_file(&resolved).as_bytes(),
                    Some("0600"),
                )
                .await
                .context("writing the environment file")?;

            let owner = service
                .unit
                .as_ref()
                .map(|u| u.user.trim())
                .unwrap_or_default();
            if !owner.is_empty() {
                let group = service
                    .unit
                    .as_ref()
                    .map(|u| u.group.trim())
                    .filter(|g| !g.is_empty())
                    .unwrap_or(owner);
                session
                    .exec(&format!(
                        "chown {}:{} {}",
                        quote(owner),
                        quote(group),
                        quote(&paths.env_file())
                    ))
                    .await?
                    .require_success("setting ownership of the environment file")?;
            }

            self.emit(
                deployment_id,
                &format!("wrote {} secret(s) to {}", resolved.len(), paths.env_file()),
                false,
            )
            .await;
        }

        // ---- unit file ----
        let unit_body = systemd::render_unit(&service);
        let unit_path = systemd::unit_file_path(&service);
        session
            .write_file(&unit_path, unit_body.as_bytes(), Some("0644"))
            .await
            .context("writing the systemd unit")?;
        self.emit(deployment_id, &format!("wrote {unit_path}"), false)
            .await;

        // ---- activate ----
        self.check_cancelled(deployment_id).await?;
        self.transition(deployment_id, deployment::Status::Activating)
            .await;

        // `ln -sfn` onto a temporary name followed by a rename is what makes the
        // swap atomic: a plain `ln -sf` over an existing symlink is unlink-then-
        // create, and a process starting in that window finds nothing.
        let link = paths.current_link();
        let temp_link = format!("{link}.new");
        session
            .exec(&format!(
                "ln -sfn {release} {temp} && mv -T {temp} {link} \
                 || (ln -sfn {release} {temp} && mv {temp} {link})",
                release = quote(&release_dir),
                temp = quote(&temp_link),
                link = quote(&link)
            ))
            .await?
            .require_success("swapping the current symlink")?;
        self.emit(deployment_id, &format!("{link} -> {release_dir}"), false)
            .await;

        session
            .exec("systemctl daemon-reload")
            .await?
            .require_success("systemctl daemon-reload")?;

        // Enable so the service survives a reboot; a deploy that does not come
        // back after a restart is not a deploy.
        session
            .exec(&format!("systemctl enable {}", quote(&unit_name)))
            .await?
            .require_success("systemctl enable")?;

        let restart = session
            .exec(&format!("systemctl restart {}", quote(&unit_name)))
            .await?;
        if !restart.ok() {
            // Pull the unit's own diagnostics in rather than reporting a bare
            // exit code, since that is what an operator needs.
            let status = session
                .exec(&format!(
                    "systemctl status --no-pager --lines=20 {} 2>&1 || true",
                    quote(&unit_name)
                ))
                .await
                .map(|r| r.stdout)
                .unwrap_or_default();
            for line in status.lines() {
                self.emit(deployment_id, line, true).await;
            }
            restart.require_success("systemctl restart")?;
        }
        self.emit(deployment_id, &format!("restarted {unit_name}"), false)
            .await;

        // ---- health check ----
        if options.skip_health_check {
            self.emit(deployment_id, "health check skipped by request", false)
                .await;
        } else {
            self.check_cancelled(deployment_id).await?;
            self.transition(deployment_id, deployment::Status::HealthChecking)
                .await;

            let engine = self.clone();
            let deployment_for_health = deployment_id.to_string();
            let outcome = health::evaluate(
                &session,
                service.health_check.as_ref(),
                &unit_name,
                move |attempt, total, detail| {
                    let engine = engine.clone();
                    let deployment_id = deployment_for_health.clone();
                    let detail = detail.to_string();
                    // The callback is sync, so hand the emit to the runtime.
                    tokio::spawn(async move {
                        engine
                            .emit(
                                &deployment_id,
                                &format!("health check {attempt}/{total}: {detail}"),
                                detail != "ok",
                            )
                            .await;
                    });
                },
            )
            .await;

            if !outcome.healthy() {
                bail!("{}", outcome.detail());
            }
            self.emit(deployment_id, "health check passed", false).await;
        }

        // ---- commit ----
        self.store
            .set_current_release(&service.id, &release.id)
            .await?;

        // Only now, with the new release healthy and live, is it safe to remove
        // old ones.
        if let Err(error) = self.prune_releases(&session, &service, &release.id).await {
            // A retention failure leaves disk usage higher than configured; it
            // does not make the deploy unsuccessful.
            self.emit(
                deployment_id,
                &format!("note: pruning old releases failed: {error:#}"),
                true,
            )
            .await;
        }

        self.finish(deployment_id, deployment::Status::Succeeded)
            .await;
        let _ = session.close().await;
        Ok(())
    }
}
