//! The deploy engine.
//!
//! One deploy is: obtain the artifact, upload it into a fresh release
//! directory, write the unit and the secrets file, swap the `current` symlink,
//! `daemon-reload` and restart, then health-check — and if the check fails, put
//! the symlink back and restart again.
//!
//! Two properties shape the code. First, the symlink swap is the only moment the
//! running service changes, and it happens after the artifact is fully on disk
//! and verified, so a failed upload never produces a half-deployed service.
//! Second, the previous release is untouched until the new one is healthy, so
//! rollback is always available and is the same operation as activation.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, anyhow, bail};
use nudo_proto::{Release, Service, artifact_source, deployment};
use tokio::sync::mpsc;

use crate::crypto::{SecretKey, sha256_hex};
use crate::events::{Bus, DeploymentEvent};
use crate::git;
use crate::health;
use crate::ssh::{OutputLine, SshSession, SshTarget, quote};
use crate::store::{DeployTrigger, NewDeployment, Store};
use crate::systemd::{self, BINARY_NAME, ReleasePaths};

/// How long to allow a build to run before giving up.
const BUILD_TIMEOUT: Duration = Duration::from_secs(60 * 60);

/// How long to allow an artifact download to take.
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(30 * 60);

/// Shared state the engine needs.
#[derive(Clone)]
pub struct Engine {
    pub store: Store,
    pub bus: Bus,
    pub secret_key: SecretKey,
    pub config: Arc<crate::Config>,
}

/// What a deploy should ship.
#[derive(Debug, Clone, Default)]
pub struct DeployOptions {
    /// Overrides the service's configured branch/tag/sha for this deploy.
    pub git_ref: String,
    /// Overrides the service's artifact URL for this deploy.
    pub artifact_url: String,
    /// A path on the control plane holding a CLI-uploaded binary.
    pub uploaded_artifact: Option<std::path::PathBuf>,
    pub skip_health_check: bool,
    pub auto_rollback_on_failure: bool,
}

impl Engine {
    /// Queues a deployment and starts it in the background.
    ///
    /// Returns as soon as the row exists so the caller gets an id to watch;
    /// the work happens in a task.
    pub async fn start_deploy(
        &self,
        service_id: &str,
        actor: nudo_proto::Actor,
        options: DeployOptions,
    ) -> anyhow::Result<nudo_proto::Deployment> {
        let service = self
            .store
            .get_service(service_id)
            .await?
            .ok_or_else(|| anyhow!("no such service: {service_id}"))?;

        // Recorded up front: if this deploy fails, this is where we go back to.
        let previous_release_id = service.current_release_id.clone();

        let deployment = self
            .store
            .create_deployment(&NewDeployment {
                service_id: service_id.to_string(),
                actor: actor.clone(),
                previous_release_id,
                git_ref: options.git_ref.clone(),
                trigger: DeployTrigger::from_actor(&actor),
            })
            .await?;

        let engine = self.clone();
        let deployment_id = deployment.id.clone();
        tokio::spawn(async move {
            engine.run_deploy(&deployment_id, options).await;
        });

        Ok(deployment)
    }

    /// Runs a deployment to completion, recording every transition.
    ///
    /// Never returns an error: a deployment's outcome belongs in its row and its
    /// event stream, not in a caller that has already been handed an id.
    async fn run_deploy(&self, deployment_id: &str, options: DeployOptions) {
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

        let ssh_target = self.ssh_target_for(&target).await?;
        let session = SshSession::connect(&ssh_target)
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

    /// Points the symlink back at the previous release and restarts.
    async fn rollback_after_failure(&self, deployment_id: &str) -> anyhow::Result<()> {
        let deployment = self
            .store
            .get_deployment(deployment_id)
            .await?
            .ok_or_else(|| anyhow!("deployment {deployment_id} disappeared"))?;

        if deployment.previous_release_id.trim().is_empty() {
            bail!("nothing to roll back to: this was the first release");
        }

        self.emit(
            deployment_id,
            &format!("rolling back to release {}", deployment.previous_release_id),
            true,
        )
        .await;

        let messages = self
            .activate_release(&deployment.service_id, &deployment.previous_release_id)
            .await?;
        for message in messages {
            self.emit(deployment_id, &message, false).await;
        }

        self.emit(deployment_id, "rollback complete", false).await;
        Ok(())
    }

    /// Points a service's `current` symlink at a release and restarts it.
    ///
    /// Shared by automatic rollback, manual rollback, and nothing else — so
    /// there is exactly one implementation of "make this release live".
    pub async fn activate_release(
        &self,
        service_id: &str,
        release_id: &str,
    ) -> anyhow::Result<Vec<String>> {
        let service = self
            .store
            .get_service(service_id)
            .await?
            .ok_or_else(|| anyhow!("no such service: {service_id}"))?;
        let target = self
            .store
            .get_target(&service.target_id)
            .await?
            .ok_or_else(|| anyhow!("no such target: {}", service.target_id))?;

        let release = self
            .store
            .get_release(release_id)
            .await?
            .ok_or_else(|| anyhow!("no such release: {release_id}"))?;
        if release.service_id != service_id {
            bail!("release {release_id} does not belong to service {service_id}");
        }

        let paths = ReleasePaths::new(&service.release_root);
        let unit_name = systemd::unit_file_name(&service);
        let release_dir = paths.release_dir(release_id);
        let link = paths.current_link();

        let ssh_target = self.ssh_target_for(&target).await?;
        let session = SshSession::connect(&ssh_target).await?;

        // Refuse to point the symlink at a directory that is not there — that
        // would leave the service unable to start with no obvious cause.
        let exists = session
            .exec(&format!(
                "test -x {} && echo yes || echo no",
                quote(&format!("{release_dir}/{BINARY_NAME}"))
            ))
            .await?;
        if exists.trimmed() != "yes" {
            bail!(
                "release {release_id} is no longer on {}: {release_dir}/{BINARY_NAME} is missing",
                target.host
            );
        }

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

        session
            .exec("systemctl daemon-reload")
            .await?
            .require_success("systemctl daemon-reload")?;
        session
            .exec(&format!("systemctl restart {}", quote(&unit_name)))
            .await?
            .require_success("systemctl restart")?;

        self.store
            .set_current_release(service_id, release_id)
            .await?;
        let _ = session.close().await;

        Ok(vec![
            format!("{link} -> {release_dir}"),
            format!("restarted {unit_name}"),
        ])
    }

    /// Removes releases outside the retention window from the target.
    async fn prune_releases(
        &self,
        session: &SshSession,
        service: &Service,
        current_release_id: &str,
    ) -> anyhow::Result<()> {
        let releases = self.store.list_releases(&service.id).await?;
        let ids: Vec<&str> = releases.iter().map(|r| r.id.as_str()).collect();
        let doomed = systemd::releases_to_prune(&ids, service.keep_releases, current_release_id);

        let paths = ReleasePaths::new(&service.release_root);
        for release_id in doomed {
            let dir = paths.release_dir(release_id);
            // Only ever a path this code composed from the release root and a
            // generated id, never a client-supplied string.
            session
                .exec(&format!("rm -rf {}", quote(&dir)))
                .await?
                .require_success(&format!("removing {dir}"))?;
            self.store.mark_release_pruned(release_id).await?;
        }

        Ok(())
    }

    /// Fetches, builds or reads the artifact this deploy should ship.
    async fn obtain_artifact(
        &self,
        deployment_id: &str,
        service: &Service,
        options: &DeployOptions,
    ) -> anyhow::Result<Artifact> {
        // An explicit URL on the request wins, so a CI job can push a specific
        // build to a service that is otherwise git-backed.
        if !options.artifact_url.trim().is_empty() {
            return self
                .download_artifact(deployment_id, options.artifact_url.trim())
                .await;
        }

        if let Some(path) = &options.uploaded_artifact {
            let bytes = tokio::fs::read(path)
                .await
                .with_context(|| format!("reading the uploaded artifact {}", path.display()))?;
            self.emit(
                deployment_id,
                &format!("using uploaded artifact ({} bytes)", bytes.len()),
                false,
            )
            .await;
            return Ok(Artifact {
                bytes,
                git_sha: String::new(),
                git_ref: String::new(),
            });
        }

        match service.artifact.as_ref().and_then(|a| a.kind.as_ref()) {
            Some(artifact_source::Kind::Url(url)) if !url.trim().is_empty() => {
                self.download_artifact(deployment_id, url.trim()).await
            }
            Some(artifact_source::Kind::Git(git_source)) => {
                self.transition(deployment_id, deployment::Status::Building)
                    .await;
                self.build_from_git(deployment_id, git_source, &options.git_ref)
                    .await
            }
            _ => bail!(
                "this service has no artifact to deploy: give it a URL, connect a git \
                 source, or push a binary with `nudo deploy --artifact`"
            ),
        }
    }

    /// Downloads a prebuilt binary.
    async fn download_artifact(&self, deployment_id: &str, url: &str) -> anyhow::Result<Artifact> {
        self.emit(deployment_id, &format!("downloading {url}"), false)
            .await;

        // Only http(s): without this a `file://` URL would make the control
        // plane read its own filesystem and ship the result to a target.
        let scheme = url.split("://").next().unwrap_or_default().to_lowercase();
        if scheme != "http" && scheme != "https" {
            bail!("artifact URLs must be http or https, not {scheme:?}");
        }

        let client = reqwest::Client::builder()
            .timeout(DOWNLOAD_TIMEOUT)
            // Bounded rather than disabled: a release asset URL redirects to
            // object storage, so following some hops is required. A chain this
            // long is a loop or a redirect-based probe.
            .redirect(reqwest::redirect::Policy::limited(5))
            .build()?;
        let response = client
            .get(url)
            .send()
            .await
            .with_context(|| format!("downloading {url}"))?;

        let status = response.status();
        if !status.is_success() {
            bail!("downloading {url} returned HTTP {status}");
        }

        // A binary this large is a misconfiguration or a decompression bomb, and
        // it would be buffered in memory before ever reaching a target.
        const MAX_ARTIFACT_BYTES: u64 = 2 * 1024 * 1024 * 1024;
        if let Some(length) = response.content_length()
            && length > MAX_ARTIFACT_BYTES
        {
            bail!(
                "{url} is {length} bytes, over the {MAX_ARTIFACT_BYTES} byte limit \
                 for a deployable artifact"
            );
        }

        let bytes = response
            .bytes()
            .await
            .context("reading the artifact body")?;
        if bytes.is_empty() {
            bail!("{url} returned an empty artifact");
        }

        Ok(Artifact {
            bytes: bytes.to_vec(),
            git_sha: String::new(),
            git_ref: String::new(),
        })
    }

    /// Clones and builds from a connected git source, on the control plane.
    ///
    /// Building here rather than on the target is deliberate: a latency-critical
    /// host must not run a compiler.
    async fn build_from_git(
        &self,
        deployment_id: &str,
        git_source: &nudo_proto::GitSource,
        git_ref_override: &str,
    ) -> anyhow::Result<Artifact> {
        let workspace = self.config.workspace_dir().join(deployment_id);
        tokio::fs::create_dir_all(&workspace)
            .await
            .with_context(|| format!("creating {}", workspace.display()))?;

        let engine = self.clone();
        let deployment_for_output = deployment_id.to_string();
        let (tx, mut rx) = mpsc::channel::<OutputLine>(256);
        let pump = tokio::spawn(async move {
            while let Some(line) = rx.recv().await {
                engine
                    .emit(&deployment_for_output, &line.text, line.stderr)
                    .await;
            }
        });

        let result = git::clone_and_build(
            &self.store,
            &self.secret_key,
            git_source,
            git_ref_override,
            &workspace,
            BUILD_TIMEOUT,
            tx.clone(),
        )
        .await;

        drop(tx);
        let _ = pump.await;

        // The checkout can be large; a failed build should not leave it behind.
        let cleanup = tokio::fs::remove_dir_all(&workspace).await;
        if let Err(error) = cleanup {
            tracing::debug!(%error, path = %workspace.display(), "could not clean the workspace");
        }

        result
    }

    /// Assembles the SSH connection details for a target from the secret store.
    ///
    /// The key never leaves the server, and a client cannot influence any of
    /// these fields.
    pub async fn ssh_target_for(&self, target: &nudo_proto::Target) -> anyhow::Result<SshTarget> {
        if target.ssh_key_id.trim().is_empty() {
            bail!(
                "target {} has no SSH key: create a secret holding the private key \
                 and set it as the target's ssh_key_id",
                target.name
            );
        }

        let private_key = self
            .store
            .reveal_secret(&self.secret_key, target.ssh_key_id.trim())
            .await?
            .ok_or_else(|| {
                anyhow!(
                    "target {}'s SSH key (secret {}) does not exist",
                    target.name,
                    target.ssh_key_id
                )
            })?;

        Ok(SshTarget {
            host: target.host.clone(),
            port: if target.port == 0 {
                22
            } else {
                target.port as u16
            },
            user: if target.user.trim().is_empty() {
                "root".to_string()
            } else {
                target.user.clone()
            },
            private_key,
            passphrase: None,
        })
    }

    /// Fails the deploy if a cancel has been requested.
    ///
    /// Checked between steps rather than by killing the task, so a cancel never
    /// interrupts a symlink swap or leaves a half-written unit file.
    async fn check_cancelled(&self, deployment_id: &str) -> anyhow::Result<()> {
        if self
            .store
            .cancel_requested(deployment_id)
            .await
            .unwrap_or(false)
        {
            bail!("deployment cancelled");
        }
        Ok(())
    }

    /// Records a status transition and tells watchers.
    async fn transition(&self, deployment_id: &str, status: deployment::Status) {
        if let Err(error) = self
            .store
            .set_deployment_status(deployment_id, status)
            .await
        {
            tracing::error!(%error, deployment = %deployment_id, "recording status failed");
        }
        self.bus
            .publish_deployment(deployment_id, DeploymentEvent::Status(status));
    }

    /// Records a terminal status, tells watchers, and releases the channel.
    async fn finish(&self, deployment_id: &str, status: deployment::Status) {
        if let Err(error) = self
            .store
            .set_deployment_status(deployment_id, status)
            .await
        {
            tracing::error!(%error, deployment = %deployment_id, "recording status failed");
        }
        self.bus
            .publish_deployment(deployment_id, DeploymentEvent::Finished(status));
        // Give a watcher a moment to receive the final event before the channel
        // goes away, so a deployment never appears to end without a verdict.
        let bus = self.bus.clone();
        let deployment_id = deployment_id.to_string();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(5)).await;
            bus.close_deployment(&deployment_id);
        });
    }

    /// Records a line of output and forwards it to watchers.
    async fn emit(&self, deployment_id: &str, line: &str, stderr: bool) {
        if line.trim().is_empty() {
            return;
        }
        if let Err(error) = self
            .store
            .append_deployment_log(deployment_id, line, stderr)
            .await
        {
            tracing::error!(%error, "recording deployment output failed");
        }
        self.bus.publish_deployment(
            deployment_id,
            DeploymentEvent::Output {
                line: line.to_string(),
                stderr,
            },
        );
    }
}

/// A binary to deploy, plus what it was built from.
#[derive(Debug, Clone)]
pub struct Artifact {
    pub bytes: Vec<u8>,
    pub git_sha: String,
    pub git_ref: String,
}

#[cfg(test)]
mod tests;
