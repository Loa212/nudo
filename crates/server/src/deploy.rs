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

use anyhow::anyhow;

use crate::crypto::SecretKey;
use crate::events::Bus;
use crate::store::{DeployTrigger, NewDeployment, SshHost, Store};

mod artifact;
mod execution;
mod lifecycle;
mod releases;

/// Shared state the engine needs.
#[derive(Clone)]
pub struct Engine {
    pub store: Store,
    pub bus: Bus,
    pub secret_key: SecretKey,
    pub config: Arc<crate::Config>,
}

/// The parts of a host the SSH layer needs, borrowed from either kind of row.
///
/// A target and a build host are different tables and different messages, and
/// they stay that way — a build host is never deployed to. But *connecting* to
/// one is the same operation on the same five fields, host-key verification
/// included: a build host is handed repository credentials, so its identity
/// matters at least as much as a deploy target's. Borrowing both into one shape
/// is what lets [`Engine::connect`] be written once instead of once per table,
/// which is what keeps trust-on-first-use from drifting between them.
#[derive(Debug, Clone, Copy)]
pub struct SshHostRef<'a> {
    pub kind: SshHost,
    pub id: &'a str,
    pub name: &'a str,
    pub host: &'a str,
    pub port: u32,
    pub user: &'a str,
    pub ssh_key_id: &'a str,
    /// The pinned key, or empty when nothing is pinned yet.
    pub pinned_key: &'a str,
}

impl<'a> From<&'a nudo_proto::Target> for SshHostRef<'a> {
    fn from(target: &'a nudo_proto::Target) -> Self {
        Self {
            kind: SshHost::Target,
            id: &target.id,
            name: &target.name,
            host: &target.host,
            port: target.port,
            user: &target.user,
            ssh_key_id: &target.ssh_key_id,
            pinned_key: pinned_key(&target.host_key),
        }
    }
}

impl<'a> From<&'a nudo_proto::BuildHost> for SshHostRef<'a> {
    fn from(build_host: &'a nudo_proto::BuildHost) -> Self {
        Self {
            kind: SshHost::BuildHost,
            id: &build_host.id,
            name: &build_host.name,
            host: &build_host.host,
            port: build_host.port,
            user: &build_host.user,
            ssh_key_id: &build_host.ssh_key_id,
            pinned_key: pinned_key(&build_host.host_key),
        }
    }
}

/// The pinned key out of a host-key record, or empty when nothing is pinned.
fn pinned_key(host_key: &Option<nudo_proto::HostKey>) -> &str {
    host_key.as_ref().map_or("", |key| key.key.as_str())
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
