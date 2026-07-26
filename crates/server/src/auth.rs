//! API-token authentication for the gRPC surface.
//!
//! Tokens are presented as `authorization: Bearer <token>` and verified against
//! their stored digest. Read RPCs accept any valid token; mutating ones need the
//! `write` scope.
//!
//! Authentication is **opt-in per instance**, via `--require-api-token`. The
//! deployment this tool is built for binds the API to loopback with the
//! dashboard in front of it, and silently starting to require tokens on upgrade
//! would lock an operator out of their own control plane. When it is off, the
//! server logs a warning at startup naming the exposure.

use std::task::{Context as TaskContext, Poll};

use http::{Request, Response};
use tower::{Layer, Service};

use crate::store::Store;

/// Which RPCs may be called with a read-only token.
///
/// Full method paths. Anything not listed is treated as mutating and needs
/// `write`, so a newly added RPC is closed by default — the safe direction for
/// this list to be incomplete in.
const READ_ONLY_METHODS: [&str; 14] = [
    "/controlplane.v1.Targets/Get",
    "/controlplane.v1.Targets/List",
    "/controlplane.v1.Targets/Check",
    "/controlplane.v1.ServicesApi/Get",
    "/controlplane.v1.ServicesApi/List",
    "/controlplane.v1.ServicesApi/RenderUnit",
    "/controlplane.v1.ServicesApi/GetUnitStatus",
    "/controlplane.v1.ServicesApi/WatchUnitStatus",
    "/controlplane.v1.Deployments/Get",
    "/controlplane.v1.Deployments/List",
    "/controlplane.v1.Deployments/ListReleases",
    "/controlplane.v1.Deployments/Watch",
    "/controlplane.v1.Logs/Stream",
    "/controlplane.v1.Audit/List",
];

/// Paths reachable without a token. The health service is what an orchestrator
/// polls, and it exposes nothing.
const UNAUTHENTICATED_PREFIXES: [&str; 1] = ["/grpc.health.v1.Health/"];

/// Whether a method may be called with a read-only token.
pub fn is_read_only(method: &str) -> bool {
    READ_ONLY_METHODS.contains(&method)
}

/// Whether a method needs no token at all.
pub fn is_public(method: &str) -> bool {
    UNAUTHENTICATED_PREFIXES
        .iter()
        .any(|prefix| method.starts_with(prefix))
}

/// Reads the bearer token from request headers.
pub fn bearer_token(headers: &http::HeaderMap) -> Option<String> {
    let raw = headers.get(http::header::AUTHORIZATION)?.to_str().ok()?;
    let token = raw
        .strip_prefix("Bearer ")
        .or_else(|| raw.strip_prefix("bearer "))?
        .trim();
    if token.is_empty() {
        None
    } else {
        Some(token.to_string())
    }
}

/// Why a request was refused.
#[derive(Debug, PartialEq, Eq)]
pub enum Refusal {
    /// No token, or one that does not verify.
    Unauthenticated(String),
    /// A valid token without the scope the method needs.
    Forbidden(String),
}

impl Refusal {
    /// The gRPC status a client should see.
    fn to_status(&self) -> tonic::Status {
        match self {
            Self::Unauthenticated(message) => tonic::Status::unauthenticated(message.clone()),
            Self::Forbidden(message) => tonic::Status::permission_denied(message.clone()),
        }
    }
}

/// Verifies a token and its scope against a method.
pub async fn check(
    store: &Store,
    token: Option<&str>,
    method: &str,
) -> Result<Option<String>, Refusal> {
    if is_public(method) {
        return Ok(None);
    }

    let Some(token) = token else {
        return Err(Refusal::Unauthenticated(
            "this control plane requires an API token: send it as \
             `authorization: Bearer <token>`, or set NUDO_TOKEN for the CLI"
                .to_string(),
        ));
    };

    let verified = match store.authenticate_api_token(token).await {
        Ok(Some(verified)) => verified,
        Ok(None) => {
            return Err(Refusal::Unauthenticated(
                "that API token is unknown, revoked, or expired".to_string(),
            ));
        }
        Err(error) => {
            // A database failure is not the caller's fault, but it must not fail
            // open either.
            tracing::error!(%error, "verifying an API token failed");
            return Err(Refusal::Unauthenticated(
                "the API token could not be verified".to_string(),
            ));
        }
    };

    if !is_read_only(method) && !verified.can_write() {
        return Err(Refusal::Forbidden(format!(
            "API token {:?} has only read scope, and {method} mutates state",
            verified.name
        )));
    }

    Ok(Some(verified.name))
}

/// Requires a valid API token on every non-public RPC.
#[derive(Clone)]
pub struct RequireApiToken {
    store: Store,
}

impl RequireApiToken {
    pub fn new(store: Store) -> Self {
        Self { store }
    }
}

impl<S> Layer<S> for RequireApiToken {
    type Service = ApiTokenService<S>;

    fn layer(&self, inner: S) -> Self::Service {
        ApiTokenService {
            inner,
            store: self.store.clone(),
        }
    }
}

/// The service the layer wraps.
#[derive(Clone)]
pub struct ApiTokenService<S> {
    inner: S,
    store: Store,
}

impl<S, ReqBody, ResBody> Service<Request<ReqBody>> for ApiTokenService<S>
where
    S: Service<Request<ReqBody>, Response = Response<ResBody>> + Clone + Send + 'static,
    S::Future: Send + 'static,
    ReqBody: Send + 'static,
    ResBody: Default + Send + 'static,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<Self::Response, Self::Error>> + Send>,
    >;

    fn poll_ready(&mut self, cx: &mut TaskContext<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, request: Request<ReqBody>) -> Self::Future {
        // Cloned because the inner service is moved into the future, and
        // `poll_ready` was called on `self` rather than the clone.
        let clone = self.inner.clone();
        let mut inner = std::mem::replace(&mut self.inner, clone);
        let store = self.store.clone();

        Box::pin(async move {
            let method = request.uri().path().to_string();
            let token = bearer_token(request.headers());

            match check(&store, token.as_deref(), &method).await {
                Ok(name) => {
                    if let Some(name) = name {
                        tracing::debug!(%method, token = %name, "authenticated an API call");
                    }
                    inner.call(request).await
                }
                Err(refusal) => {
                    tracing::warn!(%method, "refused an API call: {refusal:?}");
                    // A gRPC error is a 200 with a trailer, not an HTTP error
                    // status — so the status is rendered into a response the
                    // generated client understands.
                    Ok(refusal.to_status().into_http())
                }
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headers_with(value: Option<&str>) -> http::HeaderMap {
        let mut headers = http::HeaderMap::new();
        if let Some(value) = value {
            headers.insert(
                http::header::AUTHORIZATION,
                value.parse().expect("header value"),
            );
        }
        headers
    }

    #[test]
    fn a_bearer_token_is_read_whatever_the_scheme_casing() {
        assert_eq!(
            bearer_token(&headers_with(Some("Bearer nudo_abc"))),
            Some("nudo_abc".to_string())
        );
        assert_eq!(
            bearer_token(&headers_with(Some("bearer nudo_abc"))),
            Some("nudo_abc".to_string())
        );
    }

    #[test]
    fn a_missing_or_malformed_header_yields_no_token() {
        assert!(bearer_token(&headers_with(None)).is_none());
        assert!(bearer_token(&headers_with(Some("Basic abc"))).is_none());
        assert!(bearer_token(&headers_with(Some("Bearer "))).is_none());
        assert!(bearer_token(&headers_with(Some("Bearer    "))).is_none());
        assert!(bearer_token(&headers_with(Some("nudo_abc"))).is_none());
    }

    #[test]
    fn the_read_only_list_is_exactly_the_non_mutating_rpcs() {
        // A read RPC missing here is inconvenient; a mutating one wrongly
        // present would let a read-only token deploy. So both directions are
        // pinned.
        for method in [
            "/controlplane.v1.Targets/List",
            "/controlplane.v1.Targets/Check",
            "/controlplane.v1.ServicesApi/RenderUnit",
            "/controlplane.v1.ServicesApi/WatchUnitStatus",
            "/controlplane.v1.Deployments/Watch",
            "/controlplane.v1.Deployments/ListReleases",
            "/controlplane.v1.Logs/Stream",
            "/controlplane.v1.Audit/List",
        ] {
            assert!(is_read_only(method), "{method} should be read-only");
        }

        for method in [
            "/controlplane.v1.Targets/Create",
            "/controlplane.v1.Targets/Update",
            "/controlplane.v1.Targets/Delete",
            "/controlplane.v1.ServicesApi/Create",
            "/controlplane.v1.ServicesApi/UnitAction",
            "/controlplane.v1.Deployments/Deploy",
            "/controlplane.v1.Deployments/Rollback",
            "/controlplane.v1.Deployments/Cancel",
            // Running a command changes the target, whatever the command is.
            "/controlplane.v1.Logs/RunCommand",
            "/controlplane.v1.Secrets/Put",
            "/controlplane.v1.Secrets/Delete",
            "/controlplane.v1.Terminals/CreateSession",
            "/controlplane.v1.Terminals/Attach",
            "/controlplane.v1.Sources/CreateGithubAppManifest",
            "/controlplane.v1.Sources/Delete",
        ] {
            assert!(!is_read_only(method), "{method} must require write scope");
        }
    }

    #[test]
    fn an_unknown_method_requires_write_scope() {
        // A new RPC should be closed by default, not open.
        assert!(!is_read_only("/controlplane.v1.Something/New"));
    }

    #[test]
    fn only_the_health_service_is_public() {
        assert!(is_public("/grpc.health.v1.Health/Check"));
        assert!(is_public("/grpc.health.v1.Health/Watch"));
        assert!(!is_public("/controlplane.v1.Targets/List"));
    }

    #[tokio::test]
    async fn the_health_service_needs_no_token() {
        let store = Store::open_in_memory().await.expect("store");
        assert_eq!(
            check(&store, None, "/grpc.health.v1.Health/Check").await,
            Ok(None)
        );
    }

    #[tokio::test]
    async fn a_request_with_no_token_is_refused_with_advice() {
        let store = Store::open_in_memory().await.expect("store");
        let refusal = check(&store, None, "/controlplane.v1.Targets/List")
            .await
            .expect_err("must be refused");

        match &refusal {
            Refusal::Unauthenticated(message) => {
                // The message says how to fix it.
                assert!(message.contains("Bearer"));
                assert!(message.contains("NUDO_TOKEN"));
            }
            other => panic!("expected unauthenticated, got {other:?}"),
        }
        assert_eq!(refusal.to_status().code(), tonic::Code::Unauthenticated);
    }

    #[tokio::test]
    async fn an_unknown_token_is_refused() {
        let store = Store::open_in_memory().await.expect("store");
        let refusal = check(&store, Some("nudo_bogus"), "/controlplane.v1.Targets/List")
            .await
            .expect_err("must be refused");
        assert!(matches!(refusal, Refusal::Unauthenticated(_)));
    }

    #[tokio::test]
    async fn a_read_token_may_read_but_not_mutate() {
        let store = Store::open_in_memory().await.expect("store");
        let (_, plaintext) = store
            .create_api_token("ci-read", &["read".to_string()], "usr_1")
            .await
            .expect("token");

        assert_eq!(
            check(&store, Some(&plaintext), "/controlplane.v1.Targets/List").await,
            Ok(Some("ci-read".to_string()))
        );

        let refusal = check(
            &store,
            Some(&plaintext),
            "/controlplane.v1.Deployments/Deploy",
        )
        .await
        .expect_err("mutations must be refused");

        match &refusal {
            // Naming the token tells an operator which one to re-scope.
            Refusal::Forbidden(message) => assert!(message.contains("ci-read")),
            other => panic!("expected forbidden, got {other:?}"),
        }
        assert_eq!(refusal.to_status().code(), tonic::Code::PermissionDenied);
    }

    #[tokio::test]
    async fn a_write_token_may_do_both() {
        let store = Store::open_in_memory().await.expect("store");
        let (_, plaintext) = store
            .create_api_token("ci-write", &["write".to_string()], "usr_1")
            .await
            .expect("token");

        assert!(
            check(&store, Some(&plaintext), "/controlplane.v1.Targets/List")
                .await
                .is_ok()
        );
        assert!(
            check(
                &store,
                Some(&plaintext),
                "/controlplane.v1.Deployments/Deploy"
            )
            .await
            .is_ok()
        );
    }

    #[tokio::test]
    async fn a_revoked_token_stops_working() {
        let store = Store::open_in_memory().await.expect("store");
        let (token, plaintext) = store
            .create_api_token("ci", &["write".to_string()], "usr_1")
            .await
            .expect("token");
        store.revoke_api_token(&token.id).await.expect("revoke");

        assert!(matches!(
            check(&store, Some(&plaintext), "/controlplane.v1.Targets/List").await,
            Err(Refusal::Unauthenticated(_))
        ));
    }
}
