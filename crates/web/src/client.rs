//! The dashboard's reads.
//!
//! Reaching the control plane is [`nudo_client::Client`]'s job — one lazily
//! connected channel, held and cloned per call, with the credential attached
//! by an interceptor. What is left here is the policy that is the dashboard's
//! alone: a read that cannot reach the control plane yields nothing rather
//! than an error, so a page renders with a banner instead of a 500.
//!
//! The CLI wants the opposite (an error a script can act on) and the MCP
//! server wants a third thing (a fixable request error for the agent), which
//! is why this degrading policy lives here and not in the shared crate.

use nudo_client::Client;
use nudo_proto::*;

/// Re-exported so callers can build statuses without a direct tonic dependency.
pub use tonic::Status;

/// The reads the dashboard makes, each degrading to empty when the control
/// plane cannot be reached.
///
/// An extension trait rather than a wrapper type: a wrapper would need ten
/// delegating methods to forward the typed accessors, and callers that want to
/// distinguish "none" from "unreachable" use those accessors directly.
///
/// `async_fn_in_trait` warns that callers cannot add a `Send` bound to these
/// futures. That only matters for a trait meant to be used generically; this
/// one is implemented for exactly one concrete type and awaited inside the
/// handlers, so the bound is never needed.
#[allow(async_fn_in_trait)]
pub trait DashboardReads {
    async fn list_targets(&self) -> Vec<Target>;
    async fn list_build_hosts(&self) -> Vec<BuildHost>;
    async fn default_build_host_id(&self) -> String;
    async fn services_building_on(&self, build_host_id: &str) -> Vec<Service>;
    async fn list_services(&self, target_id: &str) -> Vec<Service>;
    async fn list_secrets(&self) -> Vec<Secret>;
    async fn list_sources(&self) -> Vec<Source>;
    async fn list_deployments(&self, service_id: &str, limit: u32) -> Vec<Deployment>;
    async fn list_releases(&self, service_id: &str) -> Vec<Release>;
    async fn unit_statuses(
        &self,
        services: &[Service],
    ) -> std::collections::HashMap<String, UnitStatus>;
}

impl DashboardReads for Client {
    async fn list_targets(&self) -> Vec<Target> {
        self.targets()
            .list(ListTargetsRequest {
                page_size: 200,
                ..Default::default()
            })
            .await
            .map(|response| response.into_inner().targets)
            .unwrap_or_default()
    }

    async fn list_build_hosts(&self) -> Vec<BuildHost> {
        self.build_hosts()
            .list(ListBuildHostsRequest {
                page_size: 200,
                ..Default::default()
            })
            .await
            .map(|response| response.into_inner().build_hosts)
            .unwrap_or_default()
    }

    /// The instance's default build host id, or empty for the control plane.
    ///
    /// Degrades to empty — the control plane — which is also what an instance
    /// that has configured nothing reports, so an unreachable control plane
    /// renders the page rather than a 500.
    async fn default_build_host_id(&self) -> String {
        self.build_hosts()
            .get_defaults(GetBuildDefaultsRequest {})
            .await
            .map(|response| response.into_inner().build_host_id)
            .unwrap_or_default()
    }

    /// Services that name this build host explicitly.
    ///
    /// Only explicit references: a service falling back to the instance default
    /// is not listed, because it is not tied to this host and would move with
    /// the default. What this answers is "what breaks if I delete this".
    async fn services_building_on(&self, build_host_id: &str) -> Vec<Service> {
        if build_host_id.trim().is_empty() {
            return Vec::new();
        }

        let all = self
            .services()
            .list(ListServicesRequest {
                page_size: 200,
                ..Default::default()
            })
            .await
            .map(|response| response.into_inner().services)
            .unwrap_or_default();

        all.into_iter()
            .filter(|service| {
                matches!(
                    service.artifact.as_ref().and_then(|a| a.kind.as_ref()),
                    Some(artifact_source::Kind::Git(git)) if git.build_host_id == build_host_id
                )
            })
            .collect()
    }

    async fn list_services(&self, target_id: &str) -> Vec<Service> {
        self.services()
            .list(ListServicesRequest {
                target_id: target_id.to_string(),
                page_size: 200,
                ..Default::default()
            })
            .await
            .map(|response| response.into_inner().services)
            .unwrap_or_default()
    }

    async fn list_secrets(&self) -> Vec<Secret> {
        self.secrets()
            .list(ListSecretsRequest::default())
            .await
            .map(|response| response.into_inner().secrets)
            .unwrap_or_default()
    }

    async fn list_sources(&self) -> Vec<Source> {
        self.sources()
            .list(ListSourcesRequest {})
            .await
            .map(|response| response.into_inner().sources)
            .unwrap_or_default()
    }

    async fn list_deployments(&self, service_id: &str, limit: u32) -> Vec<Deployment> {
        self.deployments()
            .list(ListDeploymentsRequest {
                service_id: service_id.to_string(),
                page_size: limit,
                ..Default::default()
            })
            .await
            .map(|response| response.into_inner().deployments)
            .unwrap_or_default()
    }

    async fn list_releases(&self, service_id: &str) -> Vec<Release> {
        self.deployments()
            .list_releases(ListReleasesRequest {
                service_id: service_id.to_string(),
            })
            .await
            .map(|response| response.into_inner().releases)
            .unwrap_or_default()
    }

    /// Live unit state for a set of services, keyed by service id.
    ///
    /// Each is fetched independently so one unreachable target does not blank the
    /// whole list; a service whose status could not be read is simply absent, and
    /// the renderer shows it as unknown.
    async fn unit_statuses(
        &self,
        services: &[Service],
    ) -> std::collections::HashMap<String, UnitStatus> {
        let mut statuses = std::collections::HashMap::new();
        let mut client = self.services();

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

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn an_unreachable_control_plane_degrades_every_read_to_empty() {
        // The policy this module exists for. The dashboard has to render
        // something when the control plane is down; a banner plus an empty
        // table beats a 500 page.
        //
        // That the handle itself survives an unreachable endpoint, and that a
        // caller wanting the error can still get one, are `nudo_client`'s
        // properties and are tested there.
        let api = Client::new("http://127.0.0.1:1", None).expect("a valid endpoint");

        assert!(api.list_targets().await.is_empty());
        assert!(api.list_build_hosts().await.is_empty());
        assert!(api.default_build_host_id().await.is_empty());
        assert!(api.list_services("").await.is_empty());
        assert!(api.list_secrets().await.is_empty());
        assert!(api.list_sources().await.is_empty());
        assert!(api.list_deployments("", 10).await.is_empty());
        assert!(api.list_releases("svc_1").await.is_empty());
        assert!(api.unit_statuses(&[Service::default()]).await.is_empty());
    }
}
