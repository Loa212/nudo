//! How every nudo client reaches the control plane.
//!
//! The dashboard, the CLI and the MCP server are all gRPC clients of the same
//! server, presenting the same credential to the same endpoint. They had three
//! implementations of that, which agreed on nothing: three ways to dial, two
//! ways to attach a token and one crate that never attached one at all.
//!
//! ## One channel, connected lazily and held
//!
//! Lazy because the dashboard has to start and keep serving when the control
//! plane is not up yet — a page that renders "unreachable" is far more useful
//! than one that fails to load. A lazy channel hands back a usable handle
//! immediately, dials on first use, and redials by itself afterwards.
//!
//! Held because the alternative — which all three did — was dialing per
//! request: a dashboard page that read four things opened four TCP connections
//! to answer one navigation and then threw them away. A tonic [`Channel`] is a
//! handle to one multiplexed HTTP/2 connection, so cloning it per call is how
//! it is meant to be shared.
//!
//! Together those mean obtaining a client cannot fail, which is why the
//! accessors below are plain functions. Callers are spared a connect-error
//! branch whose only possible outcome was "unavailable" — which the call
//! itself reports anyway, at the point where it is actually actionable.
//!
//! ## One place the credential is attached
//!
//! Via an interceptor on the channel rather than per request, so a new call
//! site cannot forget it. [`Client::new`] takes the token, and every client
//! built from it carries it.

use nudo_proto::*;
use tonic::service::interceptor::InterceptedService;
use tonic::transport::{Channel, Endpoint};

/// Attaches the API token to every outbound call.
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

/// The transport every generated client below is built over.
pub type Authenticated = InterceptedService<Channel, BearerToken>;

// The generated clients are typed over their transport, so each gets an alias
// rather than repeating the full type at every call site.
pub type TargetsClient = targets_client::TargetsClient<Authenticated>;
pub type BuildHostsClient = build_hosts_client::BuildHostsClient<Authenticated>;
pub type ServicesClient = services_api_client::ServicesApiClient<Authenticated>;
pub type DeploymentsClient = deployments_client::DeploymentsClient<Authenticated>;
pub type LogsClient = logs_client::LogsClient<Authenticated>;
pub type TerminalsClient = terminals_client::TerminalsClient<Authenticated>;
pub type SourcesClient = sources_client::SourcesClient<Authenticated>;
pub type SecretsClient = secrets_client::SecretsClient<Authenticated>;
pub type AuditClient = audit_client::AuditClient<Authenticated>;
pub type SelfUpgradeClient = self_upgrade_client::SelfUpgradeClient<Authenticated>;

/// A handle to the control plane.
#[derive(Clone)]
pub struct Client {
    channel: Channel,
    token: Option<String>,
}

impl Client {
    /// Builds the handle.
    ///
    /// Fails only on an endpoint that is not a URL — a configuration error
    /// worth refusing to start over, rather than rediscovering on every call.
    /// Nothing is dialed here, so a control plane that is not up yet is not an
    /// error.
    ///
    /// Must be called from inside a Tokio runtime: the lazy channel registers
    /// with the reactor when it is built, even though it connects later.
    pub fn new(endpoint: impl Into<String>, token: Option<String>) -> anyhow::Result<Self> {
        let endpoint = endpoint.into();
        let channel = Endpoint::from_shared(endpoint.clone())
            .map_err(|error| anyhow::anyhow!("bad gRPC endpoint {endpoint:?}: {error}"))?
            .connect_lazy();

        Ok(Self {
            channel,
            token: token.filter(|value| !value.trim().is_empty()),
        })
    }

    /// The bearer token presented, if one is configured.
    ///
    /// Exposed for the one caller that builds a request outside these
    /// accessors — the dashboard's terminal websocket — so it presents the same
    /// credential.
    pub fn token(&self) -> Option<&str> {
        self.token.as_deref()
    }

    /// The transport, with the credential attached.
    fn transport(&self) -> Authenticated {
        InterceptedService::new(
            self.channel.clone(),
            BearerToken {
                token: self.token.clone(),
            },
        )
    }

    pub fn targets(&self) -> TargetsClient {
        TargetsClient::new(self.transport())
    }

    pub fn build_hosts(&self) -> BuildHostsClient {
        BuildHostsClient::new(self.transport())
    }

    pub fn services(&self) -> ServicesClient {
        ServicesClient::new(self.transport())
    }

    pub fn deployments(&self) -> DeploymentsClient {
        DeploymentsClient::new(self.transport())
    }

    pub fn logs(&self) -> LogsClient {
        LogsClient::new(self.transport())
    }

    pub fn terminals(&self) -> TerminalsClient {
        TerminalsClient::new(self.transport())
    }

    pub fn sources(&self) -> SourcesClient {
        SourcesClient::new(self.transport())
    }

    pub fn secrets(&self) -> SecretsClient {
        SecretsClient::new(self.transport())
    }

    pub fn audit(&self) -> AuditClient {
        AuditClient::new(self.transport())
    }

    pub fn self_upgrade(&self) -> SelfUpgradeClient {
        SelfUpgradeClient::new(self.transport())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A handle pointing at a port nothing is listening on.
    fn unreachable() -> Client {
        Client::new("http://127.0.0.1:1", None).expect("a valid endpoint")
    }

    #[test]
    fn an_endpoint_that_is_not_a_url_is_refused_at_construction() {
        // Caught once, at startup, rather than identically on every call.
        let error = Client::new("not a url", None)
            .err()
            .expect("not a url is not an endpoint");
        assert!(error.to_string().contains("bad gRPC endpoint"), "{error}");
    }

    #[tokio::test]
    async fn a_handle_is_usable_before_the_control_plane_is_listening() {
        // The property the dashboard depends on: building the handle must not
        // require the server to be up, because the dashboard starts first.
        let client = unreachable();
        let _ = client.targets();
        let _ = client.services();
    }

    #[tokio::test]
    async fn an_unreachable_control_plane_reports_unavailable_from_the_call() {
        // Obtaining a client cannot fail, so this is where the error belongs —
        // and it is the only place a caller could act on it.
        let error = unreachable()
            .targets()
            .list(ListTargetsRequest::default())
            .await
            .expect_err("nothing is listening");
        assert_eq!(error.code(), tonic::Code::Unavailable);
    }

    /// What the interceptor puts on a request, if anything.
    fn header_for(token: Option<&str>) -> Option<String> {
        use tonic::service::Interceptor;

        let mut interceptor = BearerToken {
            token: token.map(str::to_string),
        };
        let request = interceptor
            .call(tonic::Request::new(()))
            .expect("the interceptor never rejects");
        request
            .metadata()
            .get("authorization")
            .and_then(|value| value.to_str().ok())
            .map(str::to_string)
    }

    #[test]
    fn a_token_becomes_a_bearer_authorization_header() {
        assert_eq!(
            header_for(Some("nudo_secret")).as_deref(),
            Some("Bearer nudo_secret")
        );
    }

    #[test]
    fn no_authorization_header_is_sent_without_a_token() {
        // A control plane that requires no token must not be handed an empty
        // credential to reject.
        assert_eq!(header_for(None), None);
    }

    #[tokio::test]
    async fn a_blank_token_is_treated_as_no_token() {
        // An unset environment variable arrives as an empty string, and an
        // empty `Bearer ` header is worse than none.
        assert!(
            Client::new("http://x", Some("   ".to_string()))
                .unwrap()
                .token()
                .is_none()
        );
        assert_eq!(
            Client::new("http://x", Some("tok".to_string()))
                .unwrap()
                .token(),
            Some("tok")
        );
    }
}
