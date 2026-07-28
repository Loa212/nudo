//! The web tier's gRPC client.
//!
//! Channels are dialed per request rather than held, so the dashboard starts and
//! keeps serving even when the control plane is not up yet — a page that renders
//! "unreachable" is far more useful than one that fails to load.

use nudo_proto::*;
use tonic::transport::Channel;

/// Attaches the dashboard's API token to every outbound call.
#[derive(Clone)]
pub struct BearerToken {
    token: Option<String>,
}

impl tonic::service::Interceptor for BearerToken {
    fn call(
        &mut self,
        mut request: tonic::Request<()>,
    ) -> Result<tonic::Request<()>, tonic::Status> {
        if let Some(token) = &self.token
            && let Ok(value) = format!("Bearer {token}").parse()
        {
            request.metadata_mut().insert("authorization", value);
        }
        Ok(request)
    }
}

// The generated clients are typed over their interceptor, so each gets an alias
// rather than repeating the full type at every call site.
type TargetsClient = targets_client::TargetsClient<
    tonic::service::interceptor::InterceptedService<Channel, BearerToken>,
>;
type BuildHostsClient = build_hosts_client::BuildHostsClient<
    tonic::service::interceptor::InterceptedService<Channel, BearerToken>,
>;
type ServicesApiClient = services_api_client::ServicesApiClient<
    tonic::service::interceptor::InterceptedService<Channel, BearerToken>,
>;
type DeploymentsClient = deployments_client::DeploymentsClient<
    tonic::service::interceptor::InterceptedService<Channel, BearerToken>,
>;
type LogsClient =
    logs_client::LogsClient<tonic::service::interceptor::InterceptedService<Channel, BearerToken>>;
type TerminalsClient = terminals_client::TerminalsClient<
    tonic::service::interceptor::InterceptedService<Channel, BearerToken>,
>;
type SourcesClient = sources_client::SourcesClient<
    tonic::service::interceptor::InterceptedService<Channel, BearerToken>,
>;
type SecretsClient = secrets_client::SecretsClient<
    tonic::service::interceptor::InterceptedService<Channel, BearerToken>,
>;
type AuditClient = audit_client::AuditClient<
    tonic::service::interceptor::InterceptedService<Channel, BearerToken>,
>;

/// A handle to the control plane.
#[derive(Clone)]
pub struct Api {
    endpoint: String,
    /// Presented as a bearer token when the control plane requires one.
    token: Option<String>,
}

impl Api {
    pub fn new(endpoint: impl Into<String>, token: Option<String>) -> Self {
        Self {
            endpoint: endpoint.into(),
            token: token.filter(|value| !value.trim().is_empty()),
        }
    }

    /// The bearer token to present, if one is configured.
    ///
    /// Exposed so callers building a client by hand attach the same credential;
    /// the typed accessors below do it for their own clients.
    pub fn token(&self) -> Option<&str> {
        self.token.as_deref()
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

    /// An interceptor attaching the bearer token, when one is configured.
    ///
    /// The generated clients are typed over their interceptor, so this is the one
    /// place the credential is applied — a new accessor cannot forget it.
    fn interceptor(&self) -> BearerToken {
        BearerToken {
            token: self.token.clone(),
        }
    }

    pub async fn targets(&self) -> Result<TargetsClient, tonic::Status> {
        Ok(targets_client::TargetsClient::with_interceptor(
            self.channel().await?,
            self.interceptor(),
        ))
    }

    pub async fn build_hosts(&self) -> Result<BuildHostsClient, tonic::Status> {
        Ok(build_hosts_client::BuildHostsClient::with_interceptor(
            self.channel().await?,
            self.interceptor(),
        ))
    }

    pub async fn services(&self) -> Result<ServicesApiClient, tonic::Status> {
        Ok(services_api_client::ServicesApiClient::with_interceptor(
            self.channel().await?,
            self.interceptor(),
        ))
    }

    pub async fn deployments(&self) -> Result<DeploymentsClient, tonic::Status> {
        Ok(deployments_client::DeploymentsClient::with_interceptor(
            self.channel().await?,
            self.interceptor(),
        ))
    }

    pub async fn logs(&self) -> Result<LogsClient, tonic::Status> {
        Ok(logs_client::LogsClient::with_interceptor(
            self.channel().await?,
            self.interceptor(),
        ))
    }

    pub async fn terminals(&self) -> Result<TerminalsClient, tonic::Status> {
        Ok(terminals_client::TerminalsClient::with_interceptor(
            self.channel().await?,
            self.interceptor(),
        ))
    }

    pub async fn sources(&self) -> Result<SourcesClient, tonic::Status> {
        Ok(sources_client::SourcesClient::with_interceptor(
            self.channel().await?,
            self.interceptor(),
        ))
    }

    pub async fn secrets(&self) -> Result<SecretsClient, tonic::Status> {
        Ok(secrets_client::SecretsClient::with_interceptor(
            self.channel().await?,
            self.interceptor(),
        ))
    }

    pub async fn audit(&self) -> Result<AuditClient, tonic::Status> {
        Ok(audit_client::AuditClient::with_interceptor(
            self.channel().await?,
            self.interceptor(),
        ))
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

    pub async fn list_build_hosts(&self) -> Vec<BuildHost> {
        match self.build_hosts().await {
            Ok(mut client) => client
                .list(ListBuildHostsRequest {
                    page_size: 200,
                    ..Default::default()
                })
                .await
                .map(|response| response.into_inner().build_hosts)
                .unwrap_or_default(),
            Err(_) => Vec::new(),
        }
    }

    /// The instance's default build host id, or empty for the control plane.
    ///
    /// Degrades to empty — the control plane — which is also what an instance
    /// that has configured nothing reports, so an unreachable control plane
    /// renders the page rather than a 500.
    pub async fn default_build_host_id(&self) -> String {
        match self.build_hosts().await {
            Ok(mut client) => client
                .get_defaults(GetBuildDefaultsRequest {})
                .await
                .map(|response| response.into_inner().build_host_id)
                .unwrap_or_default(),
            Err(_) => String::new(),
        }
    }

    /// Services that name this build host explicitly.
    ///
    /// Only explicit references: a service falling back to the instance default
    /// is not listed, because it is not tied to this host and would move with
    /// the default. What this answers is "what breaks if I delete this".
    pub async fn services_building_on(&self, build_host_id: &str) -> Vec<Service> {
        if build_host_id.trim().is_empty() {
            return Vec::new();
        }

        let all = match self.services().await {
            Ok(mut client) => client
                .list(ListServicesRequest {
                    page_size: 200,
                    ..Default::default()
                })
                .await
                .map(|response| response.into_inner().services)
                .unwrap_or_default(),
            Err(_) => Vec::new(),
        };

        all.into_iter()
            .filter(|service| {
                matches!(
                    service.artifact.as_ref().and_then(|a| a.kind.as_ref()),
                    Some(artifact_source::Kind::Git(git)) if git.build_host_id == build_host_id
                )
            })
            .collect()
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
        let api = Api::new("not a url", None);
        assert!(api.targets().await.is_err());
    }

    #[tokio::test]
    async fn an_unreachable_control_plane_degrades_reads_to_empty_lists() {
        // The dashboard has to render something when the server is down; a
        // banner plus an empty table beats a 500 page.
        let api = Api::new("http://127.0.0.1:1", None);

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
        let api = Api::new("http://127.0.0.1:1", None);
        let error = api.targets().await.expect_err("must fail");
        assert_eq!(error.code(), tonic::Code::Unavailable);
        assert!(error.message().contains("not reachable"));
    }
}
