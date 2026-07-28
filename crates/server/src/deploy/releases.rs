use anyhow::{anyhow, bail};
use nudo_proto::Service;

use crate::ssh::{HostKeyChanged, HostKeyOutcome, SshSession, SshTarget, quote};
use crate::systemd::{self, BINARY_NAME, ReleasePaths};

use super::Engine;

impl Engine {
    /// Points the symlink back at the previous release and restarts.
    pub(super) async fn rollback_after_failure(&self, deployment_id: &str) -> anyhow::Result<()> {
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

        let session = self.connect(&target).await?;

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
    pub(super) async fn prune_releases(
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
            host_key: target
                .host_key
                .as_ref()
                .map(|k| k.key.clone())
                .unwrap_or_default(),
        })
    }

    /// Connects to a target and persists what the connection learned about its
    /// host key.
    ///
    /// Every path that reaches a target goes through here rather than calling
    /// [`SshSession::connect`] directly, so first-use pinning happens wherever
    /// the first connection happens — a deploy, a probe, a terminal — and not
    /// only where someone remembered to record it.
    pub async fn connect(&self, target: &nudo_proto::Target) -> anyhow::Result<SshSession> {
        let ssh_target = self.ssh_target_for(target).await?;
        self.connect_ssh(&target.id, &ssh_target).await
    }

    /// Connects to already-assembled SSH details, recording the host-key
    /// outcome against `target_id`.
    ///
    /// Split from [`Engine::connect`] for the callers that have an [`SshTarget`]
    /// in hand already — the preflight check builds one to probe with.
    pub async fn connect_ssh(
        &self,
        target_id: &str,
        ssh_target: &SshTarget,
    ) -> anyhow::Result<SshSession> {
        match SshSession::connect(ssh_target).await {
            Ok(session) => {
                self.record_host_key(target_id, session.host_key()).await;
                Ok(session)
            }
            Err(error) => {
                // A changed key is held for review rather than only reported, so
                // it can be accepted from the dashboard or the CLI.
                if let Some(changed) = error.downcast_ref::<HostKeyChanged>() {
                    tracing::warn!(
                        target = %target_id,
                        expected = %changed.expected_fingerprint,
                        presented = %changed.fingerprint,
                        "refused a connection: the ssh host key has changed"
                    );
                    if let Err(error) = self
                        .store
                        .record_pending_host_key(target_id, &changed.key, &changed.fingerprint)
                        .await
                    {
                        tracing::warn!(%error, target = %target_id, "recording the changed host key failed");
                    }
                }
                Err(error)
            }
        }
    }

    /// Writes back the outcome of a successful host-key check.
    ///
    /// Failures are logged rather than propagated: the connection has already
    /// been made and verified, and failing the operation because the bookkeeping
    /// write failed would be worse than re-pinning on the next connection.
    async fn record_host_key(&self, target_id: &str, outcome: &HostKeyOutcome) {
        let result = match outcome {
            HostKeyOutcome::Pinned { key, fingerprint } => {
                tracing::info!(
                    target = %target_id,
                    %fingerprint,
                    "pinned this target's ssh host key on first use"
                );
                self.store.pin_host_key(target_id, key, fingerprint).await
            }
            // A host presenting the key it should clears any change that was
            // waiting to be reviewed — there is nothing left to review.
            HostKeyOutcome::Matched { .. } => self.store.clear_pending_host_key(target_id).await,
            // Unreachable: a change fails the connect, so there is no session to
            // ask. Handled on the error path in `connect_ssh`.
            HostKeyOutcome::Changed { .. } => Ok(()),
        };

        if let Err(error) = result {
            tracing::warn!(%error, target = %target_id, "recording the host key failed");
        }
    }

    /// Assembles the SSH connection details for a build host.
    ///
    /// The same shape as [`Engine::ssh_target_for`], against the build-host
    /// table. A build host is handed repository credentials, so its identity is
    /// verified on exactly the same terms as a deploy target's.
    pub async fn ssh_target_for_build_host(
        &self,
        build_host: &nudo_proto::BuildHost,
    ) -> anyhow::Result<SshTarget> {
        if build_host.ssh_key_id.trim().is_empty() {
            bail!(
                "build host {} has no SSH key: create a secret holding the private key \
                 and set it as the build host's ssh_key_id",
                build_host.name
            );
        }

        let private_key = self
            .store
            .reveal_secret(&self.secret_key, build_host.ssh_key_id.trim())
            .await?
            .ok_or_else(|| {
                anyhow!(
                    "build host {}'s SSH key (secret {}) does not exist",
                    build_host.name,
                    build_host.ssh_key_id
                )
            })?;

        Ok(SshTarget {
            host: build_host.host.clone(),
            port: if build_host.port == 0 {
                22
            } else {
                build_host.port as u16
            },
            user: if build_host.user.trim().is_empty() {
                "root".to_string()
            } else {
                build_host.user.clone()
            },
            private_key,
            passphrase: None,
            host_key: build_host
                .host_key
                .as_ref()
                .map(|k| k.key.clone())
                .unwrap_or_default(),
        })
    }

    /// Connects to a build host, persisting what the connection learned about
    /// its host key.
    ///
    /// The build-host counterpart of [`Engine::connect`]: every path that
    /// reaches a build host goes through here, so first-use pinning happens
    /// wherever the first connection happens rather than only where someone
    /// remembered to record it.
    pub async fn connect_build_host(
        &self,
        build_host: &nudo_proto::BuildHost,
    ) -> anyhow::Result<SshSession> {
        let ssh_target = self.ssh_target_for_build_host(build_host).await?;

        match SshSession::connect(&ssh_target).await {
            Ok(session) => {
                let result = match session.host_key() {
                    HostKeyOutcome::Pinned { key, fingerprint } => {
                        tracing::info!(
                            build_host = %build_host.id,
                            %fingerprint,
                            "pinned this build host's ssh host key on first use"
                        );
                        self.store
                            .pin_build_host_key(&build_host.id, key, fingerprint)
                            .await
                    }
                    HostKeyOutcome::Matched { .. } => {
                        self.store
                            .clear_pending_build_host_key(&build_host.id)
                            .await
                    }
                    // Unreachable: a change fails the connect.
                    HostKeyOutcome::Changed { .. } => Ok(()),
                };
                if let Err(error) = result {
                    tracing::warn!(
                        %error,
                        build_host = %build_host.id,
                        "recording the build host's host key failed"
                    );
                }
                Ok(session)
            }
            Err(error) => {
                if let Some(changed) = error.downcast_ref::<HostKeyChanged>() {
                    tracing::warn!(
                        build_host = %build_host.id,
                        expected = %changed.expected_fingerprint,
                        presented = %changed.fingerprint,
                        "refused a connection: the build host's ssh host key has changed"
                    );
                    if let Err(error) = self
                        .store
                        .record_pending_build_host_key(
                            &build_host.id,
                            &changed.key,
                            &changed.fingerprint,
                        )
                        .await
                    {
                        tracing::warn!(
                            %error,
                            build_host = %build_host.id,
                            "recording the changed host key failed"
                        );
                    }
                }
                Err(error)
            }
        }
    }
}
