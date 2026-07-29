//! The `SelfUpgrade` service.
//!
//! Thin on purpose: the gates, the version validation and the work all live
//! in [`crate::self_upgrade::SelfUpgrader`], and every refusal reads the same
//! whether it came from this RPC or from a future caller. What belongs here
//! is the audit entry — the click that authorised an instance to replace its
//! own binaries is exactly what the audit trail is for.

use nudo_proto::self_upgrade_server::SelfUpgrade;
use nudo_proto::*;
use tonic::{Request, Response, Status};

use super::Context;
use crate::self_upgrade::{SelfUpgrader, StatusView};

pub struct SelfUpgradeService {
    context: Context,
    upgrader: SelfUpgrader,
}

impl SelfUpgradeService {
    pub fn new(context: Context) -> Self {
        let upgrader = SelfUpgrader::new(context.store.clone(), context.config.clone());
        Self { context, upgrader }
    }
}

fn to_proto(view: StatusView) -> SelfUpgradeStatus {
    SelfUpgradeStatus {
        state: view.state,
        from_version: view.from_version,
        to_version: view.to_version,
        error: view.error,
        allowed_by_config: view.allowed_by_config,
        enabled_in_settings: view.enabled_in_settings,
        eligible: view.eligible,
        updated_at: view.updated_at,
    }
}

#[tonic::async_trait]
impl SelfUpgrade for SelfUpgradeService {
    async fn start(
        &self,
        request: Request<StartSelfUpgradeRequest>,
    ) -> Result<Response<SelfUpgradeStatus>, Status> {
        let request = request.into_inner();
        if request.target_version.trim().is_empty() {
            return Err(Status::invalid_argument("target_version is required"));
        }

        let authorized = self
            .context
            .authorize(
                request.mutation.as_ref(),
                "SelfUpgrade.Start",
                &format!("release/{}", request.target_version),
                None,
                format!(
                    "asked this instance to upgrade itself to {}",
                    request.target_version
                ),
            )
            .await?;

        if !authorized.dry_run {
            self.upgrader
                .start(&request.target_version)
                .await
                .map_err(|error| Status::failed_precondition(format!("{error:#}")))?;
        }

        Ok(Response::new(to_proto(self.upgrader.status().await)))
    }

    async fn get_status(
        &self,
        _request: Request<()>,
    ) -> Result<Response<SelfUpgradeStatus>, Status> {
        Ok(Response::new(to_proto(self.upgrader.status().await)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::test_support;

    fn service(context: Context) -> SelfUpgradeService {
        SelfUpgradeService::new(context)
    }

    #[tokio::test]
    async fn get_status_reports_the_closed_gates() {
        let service = service(test_support::context().await);
        let status = service
            .get_status(Request::new(()))
            .await
            .expect("status")
            .into_inner();
        assert_eq!(status.state, "idle");
        assert!(!status.allowed_by_config, "default config allows nothing");
        assert!(!status.enabled_in_settings);
        assert!(!status.eligible);
    }

    #[tokio::test]
    async fn start_without_a_version_is_invalid() {
        let service = service(test_support::context().await);
        let error = service
            .start(Request::new(StartSelfUpgradeRequest {
                target_version: "  ".to_string(),
                mutation: None,
            }))
            .await
            .expect_err("must refuse");
        assert_eq!(error.code(), tonic::Code::InvalidArgument);
    }

    #[tokio::test]
    async fn start_is_refused_by_the_config_gate_and_audited_anyway() {
        let context = test_support::context().await;
        let service = service(context.clone());
        let error = service
            .start(Request::new(StartSelfUpgradeRequest {
                target_version: "99.0.0".to_string(),
                mutation: None,
            }))
            .await
            .expect_err("must refuse");
        assert_eq!(error.code(), tonic::Code::FailedPrecondition);
        assert!(error.message().contains("--allow-self-upgrade"));

        // The attempt itself is on the record: asking an instance to replace
        // its binaries is audit-worthy even when refused.
        let entries = context
            .store
            .list_audit("", nudo_proto::actor::Kind::Unspecified, 10, 0)
            .await
            .expect("audit");
        assert!(
            entries
                .iter()
                .any(|entry| entry.action == "SelfUpgrade.Start"),
            "the start attempt is audited"
        );
    }

    #[tokio::test]
    async fn a_dry_run_authorizes_and_reports_without_starting_anything() {
        let context = test_support::context().await;
        let service = service(context.clone());
        let status = service
            .start(Request::new(StartSelfUpgradeRequest {
                target_version: "99.0.0".to_string(),
                mutation: Some(Mutation {
                    dry_run: true,
                    ..Mutation::default()
                }),
            }))
            .await
            .expect("a dry run succeeds even though the gates are closed")
            .into_inner();
        assert_eq!(status.state, "idle", "nothing actually started");
    }
}
