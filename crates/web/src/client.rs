//! The web tier's gRPC client.
//!
//! Channels are dialed per request rather than held, so the dashboard starts and
//! keeps serving even when the control plane is not up yet — a page that renders
//! "unreachable" is far more useful than one that fails to load.

use nudo_proto::*;
use tonic::transport::Channel;

/// A handle to the control plane.
#[derive(Clone)]
pub struct Api {
    endpoint: String,
}

impl Api {
    pub fn new(endpoint: impl Into<String>) -> Self {
        Self {
            endpoint: endpoint.into(),
        }
    }

    async fn channel(&self) -> Result<Channel, tonic::Status> {
        Channel::from_shared(self.endpoint.clone())
            .map_err(|error| Status::internal(format!("bad gRPC endpoint: {error}")))?
            .connect()
            .await
            .map_err(|error| {
                Status::unavailable(format!(
                    "the control plane at {} is not reachable: {error}",
                    self.endpoint
                ))
            })
    }

    pub async fn targets(&self) -> Result<targets_client::TargetsClient<Channel>, tonic::Status> {
        Ok(targets_client::TargetsClient::new(self.channel().await?))
    }

    pub async fn services(
        &self,
    ) -> Result<services_api_client::ServicesApiClient<Channel>, tonic::Status> {
        Ok(services_api_client::ServicesApiClient::new(
            self.channel().await?,
        ))
    }

    pub async fn deployments(
        &self,
    ) -> Result<deployments_client::DeploymentsClient<Channel>, tonic::Status> {
        Ok(deployments_client::DeploymentsClient::new(
            self.channel().await?,
        ))
    }

    pub async fn logs(&self) -> Result<logs_client::LogsClient<Channel>, tonic::Status> {
        Ok(logs_client::LogsClient::new(self.channel().await?))
    }

    pub async fn terminals(
        &self,
    ) -> Result<terminals_client::TerminalsClient<Channel>, tonic::Status> {
        Ok(terminals_client::TerminalsClient::new(
            self.channel().await?,
        ))
    }

    pub async fn sources(&self) -> Result<sources_client::SourcesClient<Channel>, tonic::Status> {
        Ok(sources_client::SourcesClient::new(self.channel().await?))
    }

    pub async fn secrets(&self) -> Result<secrets_client::SecretsClient<Channel>, tonic::Status> {
        Ok(secrets_client::SecretsClient::new(self.channel().await?))
    }

    pub async fn audit(&self) -> Result<audit_client::AuditClient<Channel>, tonic::Status> {
        Ok(audit_client::AuditClient::new(self.channel().await?))
    }

    // ---- convenience reads ----
    //
    // Each degrades to an empty list when the control plane is unreachable, so a
    // page renders with an explanatory banner rather than a 500. A caller that
    // needs to distinguish "none" from "unreachable" uses the client directly.

    pub async fn list_targets(&self) -> Vec<Target> {
        match self.targets().await {
            Ok(mut client) => client
                .list(ListTargetsRequest {
                    page_size: 200,
                    ..Default::default()
                })
                .await
                .map(|response| response.into_inner().targets)
                .unwrap_or_default(),
            Err(_) => Vec::new(),
        }
    }

    pub async fn list_services(&self, target_id: &str) -> Vec<Service> {
        match self.services().await {
            Ok(mut client) => client
                .list(ListServicesRequest {
                    target_id: target_id.to_string(),
                    page_size: 200,
                    ..Default::default()
                })
                .await
                .map(|response| response.into_inner().services)
                .unwrap_or_default(),
            Err(_) => Vec::new(),
        }
    }

    pub async fn list_secrets(&self) -> Vec<Secret> {
        match self.secrets().await {
            Ok(mut client) => client
                .list(ListSecretsRequest::default())
                .await
                .map(|response| response.into_inner().secrets)
                .unwrap_or_default(),
            Err(_) => Vec::new(),
        }
    }

    pub async fn list_sources(&self) -> Vec<Source> {
        match self.sources().await {
            Ok(mut client) => client
                .list(ListSourcesRequest {})
                .await
                .map(|response| response.into_inner().sources)
                .unwrap_or_default(),
            Err(_) => Vec::new(),
        }
    }

    pub async fn list_deployments(&self, service_id: &str, limit: u32) -> Vec<Deployment> {
        match self.deployments().await {
            Ok(mut client) => client
                .list(ListDeploymentsRequest {
                    service_id: service_id.to_string(),
                    page_size: limit,
                    ..Default::default()
                })
                .await
                .map(|response| response.into_inner().deployments)
                .unwrap_or_default(),
            Err(_) => Vec::new(),
        }
    }

    pub async fn list_releases(&self, service_id: &str) -> Vec<Release> {
        match self.deployments().await {
            Ok(mut client) => client
                .list_releases(ListReleasesRequest {
                    service_id: service_id.to_string(),
                })
                .await
                .map(|response| response.into_inner().releases)
                .unwrap_or_default(),
            Err(_) => Vec::new(),
        }
    }

    /// Live unit state for a set of services, keyed by service id.
    ///
    /// Each is fetched independently so one unreachable target does not blank the
    /// whole list; a service whose status could not be read is simply absent, and
    /// the renderer shows it as unknown.
    pub async fn unit_statuses(
        &self,
        services: &[Service],
    ) -> std::collections::HashMap<String, UnitStatus> {
        let mut statuses = std::collections::HashMap::new();

        let Ok(mut client) = self.services().await else {
            return statuses;
        };

        for service in services {
            if let Ok(response) = client
                .get_unit_status(GetUnitStatusRequest {
                    service_id: service.id.clone(),
                })
                .await
            {
                let status = response.into_inner();
                statuses.insert(service.id.clone(), status);
            }
        }

        statuses
    }
}

/// Re-exported so callers can build statuses without a direct tonic dependency.
pub use tonic::Status;

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_bad_endpoint_is_reported_rather_than_panicking() {
        let api = Api::new("not a url");
        assert!(api.targets().await.is_err());
    }

    #[tokio::test]
    async fn an_unreachable_control_plane_degrades_reads_to_empty_lists() {
        // The dashboard has to render something when the server is down; a
        // banner plus an empty table beats a 500 page.
        let api = Api::new("http://127.0.0.1:1");

        assert!(api.list_targets().await.is_empty());
        assert!(api.list_services("").await.is_empty());
        assert!(api.list_secrets().await.is_empty());
        assert!(api.list_sources().await.is_empty());
        assert!(api.list_deployments("", 10).await.is_empty());
        assert!(api.list_releases("svc_1").await.is_empty());
        assert!(api.unit_statuses(&[Service::default()]).await.is_empty());
    }

    #[tokio::test]
    async fn an_unreachable_control_plane_still_surfaces_an_error_to_callers_that_want_one() {
        // A page that needs to distinguish "none" from "unreachable" uses the
        // typed client, which returns a status.
        let api = Api::new("http://127.0.0.1:1");
        let error = api.targets().await.expect_err("must fail");
        assert_eq!(error.code(), tonic::Code::Unavailable);
        assert!(error.message().contains("not reachable"));
    }
}
