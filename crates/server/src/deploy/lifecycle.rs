use std::time::Duration;

use anyhow::bail;
use nudo_proto::deployment;

use crate::events::DeploymentEvent;

use super::Engine;

impl Engine {
    /// Fails the deploy if a cancel has been requested.
    ///
    /// Checked between steps rather than by killing the task, so a cancel never
    /// interrupts a symlink swap or leaves a half-written unit file.
    pub(super) async fn check_cancelled(&self, deployment_id: &str) -> anyhow::Result<()> {
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
    pub(super) async fn transition(&self, deployment_id: &str, status: deployment::Status) {
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
    pub(super) async fn finish(&self, deployment_id: &str, status: deployment::Status) {
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
    pub(super) async fn emit(&self, deployment_id: &str, line: &str, stderr: bool) {
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
