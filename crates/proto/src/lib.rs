//! Generated gRPC bindings for the control plane API, plus the small
//! conversion helpers every crate needs.
//!
//! `controlplane.proto` at the repo root is the authoritative contract. This
//! crate exists so the server, web tier, CLI and MCP server all speak the exact
//! same generated types.

pub mod v1 {
    // The generated oneof variants differ in size (a `Deployment` beside an
    // `i32`), which is inherent to the proto rather than something to fix here.
    #![allow(clippy::large_enum_variant)]

    tonic::include_proto!("controlplane.v1");
}

pub use v1::*;

/// Re-exported so downstream crates do not need a direct `prost-types` dep just
/// to build a `Timestamp`.
pub use prost_types::Timestamp;

/// Converts a chrono UTC timestamp into the protobuf representation.
///
/// Kept here rather than in each crate because the whole system stores times as
/// `chrono::DateTime<Utc>` in SQLite and speaks `prost_types::Timestamp` on the
/// wire.
pub fn to_timestamp(dt: chrono::DateTime<chrono::Utc>) -> Timestamp {
    Timestamp {
        seconds: dt.timestamp(),
        nanos: dt.timestamp_subsec_nanos() as i32,
    }
}

/// Converts an optional chrono timestamp, which is what most nullable database
/// columns produce.
pub fn to_timestamp_opt(dt: Option<chrono::DateTime<chrono::Utc>>) -> Option<Timestamp> {
    dt.map(to_timestamp)
}

/// Converts a protobuf timestamp back to chrono. Out-of-range values yield
/// `None` rather than panicking, since these arrive from the network.
pub fn from_timestamp(ts: &Timestamp) -> Option<chrono::DateTime<chrono::Utc>> {
    chrono::DateTime::from_timestamp(ts.seconds, ts.nanos.max(0) as u32)
}

// chrono is a dependency only for these helpers.
use chrono as _;

impl Actor {
    /// The actor recorded when the control plane acts on its own behalf — a
    /// health-check rollback, a retention sweep.
    pub fn system(label: impl Into<String>) -> Self {
        Self {
            kind: actor::Kind::System as i32,
            id: "system".to_string(),
            label: label.into(),
        }
    }

    /// A human acting through the dashboard or CLI.
    pub fn human(id: impl Into<String>, label: impl Into<String>) -> Self {
        Self {
            kind: actor::Kind::Human as i32,
            id: id.into(),
            label: label.into(),
        }
    }

    /// An LLM agent calling an MCP tool.
    pub fn agent(id: impl Into<String>, label: impl Into<String>) -> Self {
        Self {
            kind: actor::Kind::Agent as i32,
            id: id.into(),
            label: label.into(),
        }
    }

    /// A GitHub webhook delivery.
    pub fn webhook(delivery_id: impl Into<String>, label: impl Into<String>) -> Self {
        Self {
            kind: actor::Kind::Webhook as i32,
            id: delivery_id.into(),
            label: label.into(),
        }
    }

    /// Short lowercase name for the audit log's `actor_kind` column.
    pub fn kind_str(&self) -> &'static str {
        match actor::Kind::try_from(self.kind) {
            Ok(actor::Kind::Human) => "human",
            Ok(actor::Kind::Agent) => "agent",
            Ok(actor::Kind::Webhook) => "webhook",
            Ok(actor::Kind::System) => "system",
            _ => "unspecified",
        }
    }
}

impl Mutation {
    /// A non-dry-run mutation by the given actor, without the latency-critical
    /// override. This is the common case.
    pub fn by(actor: Actor) -> Self {
        Self {
            actor: Some(actor),
            dry_run: false,
            allow_latency_critical: false,
            idempotency_key: String::new(),
        }
    }

    /// The actor, or the `SYSTEM` actor when a client omitted one. Mutating RPCs
    /// use this so an unattributed call is still recorded rather than rejected.
    pub fn actor_or_system(&self) -> Actor {
        self.actor
            .clone()
            .unwrap_or_else(|| Actor::system("unattributed"))
    }
}

impl deployment::Status {
    /// Whether the deployment has reached a state it will not leave.
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Succeeded | Self::Failed | Self::RolledBack | Self::Cancelled
        )
    }

    /// Lowercase name used in the database, the dashboard and the CLI.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Unspecified => "unspecified",
            Self::Queued => "queued",
            Self::Building => "building",
            Self::Uploading => "uploading",
            Self::Activating => "activating",
            Self::HealthChecking => "health_checking",
            Self::Succeeded => "succeeded",
            Self::Failed => "failed",
            Self::RolledBack => "rolled_back",
            Self::Cancelled => "cancelled",
        }
    }

    /// Parses the database representation.
    pub fn parse(s: &str) -> Self {
        match s {
            "queued" => Self::Queued,
            "building" => Self::Building,
            "uploading" => Self::Uploading,
            "activating" => Self::Activating,
            "health_checking" => Self::HealthChecking,
            "succeeded" => Self::Succeeded,
            "failed" => Self::Failed,
            "rolled_back" => Self::RolledBack,
            "cancelled" => Self::Cancelled,
            _ => Self::Unspecified,
        }
    }
}

impl target::Status {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Unspecified => "unspecified",
            Self::Unknown => "unknown",
            Self::Reachable => "reachable",
            Self::Unreachable => "unreachable",
        }
    }

    pub fn parse(s: &str) -> Self {
        match s {
            "reachable" => Self::Reachable,
            "unreachable" => Self::Unreachable,
            _ => Self::Unknown,
        }
    }
}

/// The `build_host_id` that means "build on the control plane".
///
/// Distinct from an empty id, which means "whatever the instance default is".
/// A service pinned to local stays local after an operator points the instance
/// at a build host; an empty string could not express that.
pub const LOCAL_BUILD_HOST_ID: &str = "local";

/// Where a build should run, once the service and the instance default have
/// both been taken into account.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BuildLocation {
    /// A subprocess of the control plane — the original behaviour, and what an
    /// instance that configures nothing keeps doing.
    ControlPlane,
    /// A build host, by id.
    Remote(String),
}

impl BuildLocation {
    /// Resolves a service's setting against the instance default.
    ///
    /// The precedence is the whole feature in one function, so the server, the
    /// dashboard's "where will this build" hint and the CLI cannot disagree
    /// about it:
    ///
    /// 1. A service naming a build host uses it.
    /// 2. A service pinned to [`LOCAL_BUILD_HOST_ID`] builds locally, whatever
    ///    the default is.
    /// 3. Otherwise the instance default applies.
    /// 4. With no default set, builds run on the control plane.
    pub fn resolve(service_build_host_id: &str, instance_default: &str) -> Self {
        let service = service_build_host_id.trim();
        if service == LOCAL_BUILD_HOST_ID {
            return Self::ControlPlane;
        }
        if !service.is_empty() {
            return Self::Remote(service.to_string());
        }

        let default = instance_default.trim();
        if default.is_empty() || default == LOCAL_BUILD_HOST_ID {
            return Self::ControlPlane;
        }
        Self::Remote(default.to_string())
    }

    /// The build host's id, or `None` when the build runs locally.
    pub fn remote_id(&self) -> Option<&str> {
        match self {
            Self::ControlPlane => None,
            Self::Remote(id) => Some(id.as_str()),
        }
    }

    pub fn is_remote(&self) -> bool {
        matches!(self, Self::Remote(_))
    }
}

impl build_host::Status {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Unspecified => "unspecified",
            Self::Unknown => "unknown",
            Self::Reachable => "reachable",
            Self::Unreachable => "unreachable",
        }
    }

    pub fn parse(s: &str) -> Self {
        match s {
            "reachable" => Self::Reachable,
            "unreachable" => Self::Unreachable,
            _ => Self::Unknown,
        }
    }
}

impl ingress::Mode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Unspecified => "none",
            Self::Managed => "managed",
            Self::External => "external",
        }
    }

    pub fn parse(s: &str) -> Self {
        match s {
            "managed" => Self::Managed,
            "external" => Self::External,
            _ => Self::Unspecified,
        }
    }
}

impl ingress::Status {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Unspecified => "unspecified",
            Self::Pending => "pending",
            Self::Active => "active",
            Self::Degraded => "degraded",
        }
    }

    pub fn parse(s: &str) -> Self {
        match s {
            "pending" => Self::Pending,
            "active" => Self::Active,
            "degraded" => Self::Degraded,
            _ => Self::Unspecified,
        }
    }
}

/// Caddy's own default admin port, used when a target does not name one.
pub const DEFAULT_ADMIN_PORT: u32 = 2019;

/// Checks a domain is one that can safely be written into a proxy config.
///
/// This is a validator rather than a sanitiser on purpose. The domain ends up
/// inside a Caddyfile as a site address, so a value containing whitespace, a
/// brace or a newline would not merely be wrong — it would let whoever set it
/// write arbitrary directives into the config of a proxy running as root. There
/// is no useful "clean up and continue" for that, so anything not obviously a
/// hostname is refused here, at the edge, and the renderer downstream can treat
/// what it receives as safe.
///
/// Deliberately stricter than the DNS specification: no trailing dot, no
/// underscores, no IP literals. Someone with an exotic hostname is inconvenienced
/// and can say so; the alternative failure mode is a config injection.
pub fn validate_domain(domain: &str) -> Result<(), String> {
    let domain = domain.trim();

    if domain.is_empty() {
        return Err("a domain is required".to_string());
    }
    if domain.len() > 253 {
        return Err(format!(
            "{domain:?} is longer than the 253 characters DNS allows"
        ));
    }

    // A wildcard is legitimate and Caddy supports it, but only as the leftmost
    // label — "*.example.com", never "a.*.example.com".
    let body = domain.strip_prefix("*.").unwrap_or(domain);
    if body.contains('*') {
        return Err(format!(
            "{domain:?} may only use a wildcard as its first label, as in \
             \"*.example.com\""
        ));
    }

    if !body.contains('.') {
        return Err(format!(
            "{domain:?} is not a fully qualified domain — a certificate cannot \
             be issued for a bare name"
        ));
    }

    for label in body.split('.') {
        if label.is_empty() {
            return Err(format!("{domain:?} has an empty label"));
        }
        if label.len() > 63 {
            return Err(format!(
                "{domain:?} has a label longer than the 63 characters DNS allows"
            ));
        }
        if label.starts_with('-') || label.ends_with('-') {
            return Err(format!(
                "{domain:?} has a label starting or ending with '-'"
            ));
        }
        if !label.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
            return Err(format!(
                "{domain:?} contains something other than letters, digits, \
                 '-' and '.'"
            ));
        }
    }

    Ok(())
}

/// Checks a port is one a service can actually be routed to.
///
/// Zero means unset rather than a port, and anything above 65535 cannot be
/// bound. Both are refused here so the renderer never has to consider them.
pub fn validate_port(port: u32) -> Result<(), String> {
    match port {
        0 => Err("a port is required to route to a service".to_string()),
        1..=65535 => Ok(()),
        _ => Err(format!("{port} is not a port; the highest is 65535")),
    }
}

/// Normalises and checks a route's path prefix.
///
/// Returns the canonical form: empty for "the whole domain", otherwise a
/// leading slash and no trailing one, so `/api/`, `api` and `/api` are one
/// route rather than three that collide confusingly.
///
/// Validated for the same reason the domain is: the path is written into a
/// Caddyfile matcher, so a value carrying a brace, whitespace or a newline
/// could end the block and start directives of its own. The character set is
/// deliberately narrow — the unreserved URL characters plus `/`, `%` and a few
/// separators. A path needing more than that is rare enough to be worth an
/// error rather than a hole.
pub fn normalize_path(path: &str) -> Result<String, String> {
    let path = path.trim();

    // The whole domain. Both spellings mean the same thing and normalise to the
    // same stored value.
    if path.is_empty() || path == "/" {
        return Ok(String::new());
    }

    let trimmed = path.trim_end_matches('/');
    let with_slash = if trimmed.starts_with('/') {
        trimmed.to_string()
    } else {
        format!("/{trimmed}")
    };

    if with_slash.len() > 255 {
        return Err(format!("the path {path:?} is longer than 255 characters"));
    }
    // `..` would let a route claim a prefix it does not look like it claims.
    if with_slash.contains("..") {
        return Err(format!("the path {path:?} may not contain '..'"));
    }
    if with_slash.contains("//") {
        return Err(format!("the path {path:?} has an empty segment"));
    }

    if !with_slash
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || "/-._~%+".contains(c))
    {
        return Err(format!(
            "the path {path:?} contains something other than letters, digits and \
             '/-._~%+'"
        ));
    }

    Ok(with_slash)
}

impl route::Protocol {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Unspecified => "http",
            Self::H2c => "h2c",
        }
    }

    pub fn parse(s: &str) -> Self {
        match s {
            // `grpc` is what an operator calls it; `h2c` is what it is. Both
            // accepted so nobody has to know the second one.
            "h2c" | "grpc" => Self::H2c,
            _ => Self::Unspecified,
        }
    }

    /// How Caddy is told to reach the backend.
    ///
    /// The whole of gRPC support: gRPC needs HTTP/2 end to end, and a proxy
    /// that downgrades to HTTP/1.1 breaks every call.
    pub fn upstream_scheme(self) -> &'static str {
        match self {
            Self::Unspecified => "",
            Self::H2c => "h2c://",
        }
    }
}

/// Every route a set of services defines, in the order they are rendered.
///
/// Here rather than in the server because the dashboard shows the same list and
/// the two must agree: a table whose order differs from the config it describes
/// is a table nobody can check against the config.
///
/// Sorted by domain, then longest path first — Caddy matches its `handle`
/// blocks in order, so `/api/v2` has to be tried before `/api`, and the domain
/// root is the fallback.
pub fn routes_of(services: &[Service]) -> Vec<Route> {
    let mut routes: Vec<Route> = services
        .iter()
        .flat_map(|service| {
            service.routes.iter().map(|route| Route {
                // Stamped here rather than trusted from the caller, so a route
                // always reports the service it actually belongs to.
                service_id: service.id.clone(),
                service_name: service.name.clone(),
                domain: route.domain.trim().to_string(),
                // The *original* path when it does not normalise, not an empty
                // one: falling back to empty would turn "this path is not one
                // nudo will write" into "route the whole domain", quietly
                // widening what the service receives instead of dropping it.
                path: normalize_path(&route.path).unwrap_or_else(|_| route.path.clone()),
                port: route.port,
                protocol: route.protocol,
            })
        })
        .collect();

    routes.sort_by(|a, b| {
        a.domain
            .cmp(&b.domain)
            .then_with(|| b.path.len().cmp(&a.path.len()))
            .then_with(|| a.path.cmp(&b.path))
    });
    routes
}

impl Route {
    /// The protocol, defaulting to HTTP when unset or unrecognised.
    pub fn protocol_or_default(&self) -> route::Protocol {
        route::Protocol::try_from(self.protocol).unwrap_or(route::Protocol::Unspecified)
    }

    /// Checks a route is one that can safely be written into a proxy config.
    ///
    /// The single place a route is judged, so the store, the API and the
    /// renderer cannot disagree about what is acceptable.
    pub fn validate(&self) -> Result<(), String> {
        validate_domain(&self.domain)?;
        validate_port(self.port)?;
        normalize_path(&self.path)?;
        Ok(())
    }

    /// How this route is identified when reporting a collision: the domain, and
    /// the path when it has one.
    pub fn label(&self) -> String {
        match normalize_path(&self.path) {
            Ok(path) if !path.is_empty() => format!("{}{}", self.domain.trim(), path),
            _ => self.domain.trim().to_string(),
        }
    }
}

impl source::Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Unspecified => "unspecified",
            Self::GithubApp => "github_app",
            Self::DeployKey => "deploy_key",
        }
    }

    pub fn parse(s: &str) -> Self {
        match s {
            "github_app" => Self::GithubApp,
            "deploy_key" => Self::DeployKey,
            _ => Self::Unspecified,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deployment_status_round_trips_through_its_string_form() {
        for status in [
            deployment::Status::Queued,
            deployment::Status::Building,
            deployment::Status::Uploading,
            deployment::Status::Activating,
            deployment::Status::HealthChecking,
            deployment::Status::Succeeded,
            deployment::Status::Failed,
            deployment::Status::RolledBack,
            deployment::Status::Cancelled,
        ] {
            assert_eq!(deployment::Status::parse(status.as_str()), status);
        }
    }

    #[test]
    fn only_finished_deployment_states_are_terminal() {
        assert!(deployment::Status::Succeeded.is_terminal());
        assert!(deployment::Status::Failed.is_terminal());
        assert!(deployment::Status::RolledBack.is_terminal());
        assert!(deployment::Status::Cancelled.is_terminal());
        assert!(!deployment::Status::Queued.is_terminal());
        assert!(!deployment::Status::Building.is_terminal());
        assert!(!deployment::Status::HealthChecking.is_terminal());
    }

    #[test]
    fn an_instance_that_configures_nothing_still_builds_on_the_control_plane() {
        // The compatibility promise in the issue: upgrading and setting nothing
        // must not move anybody's builds.
        assert_eq!(BuildLocation::resolve("", ""), BuildLocation::ControlPlane);
    }

    #[test]
    fn a_service_naming_a_build_host_uses_it() {
        assert_eq!(
            BuildLocation::resolve("bh_gpu", ""),
            BuildLocation::Remote("bh_gpu".to_string())
        );
        // And overrides the instance default rather than being overridden by it.
        assert_eq!(
            BuildLocation::resolve("bh_gpu", "bh_default"),
            BuildLocation::Remote("bh_gpu".to_string())
        );
    }

    #[test]
    fn a_service_with_no_setting_falls_back_to_the_instance_default() {
        assert_eq!(
            BuildLocation::resolve("", "bh_default"),
            BuildLocation::Remote("bh_default".to_string())
        );
    }

    #[test]
    fn a_service_pinned_to_local_stays_local_despite_the_instance_default() {
        // The reason "local" is a sentinel rather than an empty string: without
        // it, pointing the instance at a build host would silently move a
        // service that was deliberately building on the control plane.
        assert_eq!(
            BuildLocation::resolve(LOCAL_BUILD_HOST_ID, "bh_default"),
            BuildLocation::ControlPlane
        );
    }

    #[test]
    fn an_instance_default_of_local_is_the_same_as_no_default() {
        assert_eq!(
            BuildLocation::resolve("", LOCAL_BUILD_HOST_ID),
            BuildLocation::ControlPlane
        );
    }

    #[test]
    fn surrounding_whitespace_does_not_create_a_phantom_build_host() {
        // These arrive from a text field in the dashboard.
        assert_eq!(
            BuildLocation::resolve("   ", "  "),
            BuildLocation::ControlPlane
        );
        assert_eq!(
            BuildLocation::resolve(" bh_gpu ", ""),
            BuildLocation::Remote("bh_gpu".to_string())
        );
    }

    #[test]
    fn a_resolved_location_reports_its_remote_id() {
        assert_eq!(BuildLocation::ControlPlane.remote_id(), None);
        assert!(!BuildLocation::ControlPlane.is_remote());

        let remote = BuildLocation::Remote("bh_1".to_string());
        assert_eq!(remote.remote_id(), Some("bh_1"));
        assert!(remote.is_remote());
    }

    #[test]
    fn build_host_status_round_trips_through_its_string_form() {
        for status in [
            build_host::Status::Unknown,
            build_host::Status::Reachable,
            build_host::Status::Unreachable,
        ] {
            assert_eq!(build_host::Status::parse(status.as_str()), status);
        }
    }

    #[test]
    fn ingress_mode_and_status_round_trip_through_their_string_forms() {
        for mode in [
            ingress::Mode::Unspecified,
            ingress::Mode::Managed,
            ingress::Mode::External,
        ] {
            assert_eq!(ingress::Mode::parse(mode.as_str()), mode);
        }
        for status in [
            ingress::Status::Pending,
            ingress::Status::Active,
            ingress::Status::Degraded,
        ] {
            assert_eq!(ingress::Status::parse(status.as_str()), status);
        }
    }

    #[test]
    fn ordinary_domains_are_accepted() {
        for domain in [
            "example.com",
            "api.example.com",
            "a.b.c.d.example.com",
            "xn--bcher-kva.example.com", // punycode, already ascii
            "*.example.com",
            "service-1.example.com",
        ] {
            assert!(
                validate_domain(domain).is_ok(),
                "{domain} should be accepted: {:?}",
                validate_domain(domain)
            );
        }
    }

    #[test]
    fn a_domain_that_could_inject_caddy_directives_is_refused() {
        // The whole reason this validator exists. Each of these, written into a
        // Caddyfile unescaped, ends the site block and starts something the
        // operator did not ask for — in the config of a proxy running as root.
        for attack in [
            "example.com {\n  respond \"pwned\"\n}",
            "example.com }\nimport /etc/passwd\n{",
            "example.com\nexample.org",
            "example.com respond",
            "example.com # comment",
            "exam ple.com",
            "example.com\t{",
        ] {
            assert!(
                validate_domain(attack).is_err(),
                "{attack:?} must be refused — it can rewrite the proxy config"
            );
        }
    }

    #[test]
    fn a_domain_that_is_merely_malformed_is_refused_with_a_reason() {
        assert!(validate_domain("").is_err());
        assert!(validate_domain("   ").is_err());
        // A bare name cannot be issued a certificate.
        assert!(validate_domain("localhost").is_err());
        // A wildcard is only legitimate as the leftmost label.
        assert!(validate_domain("a.*.example.com").is_err());
        assert!(validate_domain("*example.com").is_err());
        // Empty labels, and the trailing dot this deliberately does not accept.
        assert!(validate_domain("example..com").is_err());
        assert!(validate_domain("example.com.").is_err());
        // Labels may not start or end with a hyphen.
        assert!(validate_domain("-bad.example.com").is_err());
        assert!(validate_domain("bad-.example.com").is_err());
        // Length limits.
        assert!(validate_domain(&format!("{}.example.com", "a".repeat(64))).is_err());
        assert!(validate_domain(&format!("{}.com", "a.".repeat(200))).is_err());
    }

    #[test]
    fn a_port_outside_the_bindable_range_is_refused() {
        assert!(validate_port(0).is_err(), "zero means unset, not a port");
        assert!(validate_port(65536).is_err());
        assert!(validate_port(1).is_ok());
        assert!(validate_port(8080).is_ok());
        assert!(validate_port(65535).is_ok());
    }

    #[test]
    fn a_path_is_normalised_to_one_canonical_form() {
        // So '/api/', 'api' and '/api' are one route rather than three that
        // collide confusingly.
        for input in ["/api", "api", "/api/", "api/", "  /api  "] {
            assert_eq!(normalize_path(input).expect("valid"), "/api", "{input:?}");
        }
        // The whole domain, both spellings.
        assert_eq!(normalize_path("").expect("valid"), "");
        assert_eq!(normalize_path("/").expect("valid"), "");
    }

    #[test]
    fn a_path_that_could_inject_caddy_directives_is_refused() {
        // The path lands in a Caddyfile matcher, so it is a second way in
        // beside the domain.
        for attack in [
            "/api {\n\trespond \"owned\"\n}",
            "/api\nrespond",
            "/a b",
            "/api\t{",
            "/api#comment",
        ] {
            assert!(
                normalize_path(attack).is_err(),
                "{attack:?} must be refused — it can rewrite the proxy config"
            );
        }
    }

    #[test]
    fn a_path_that_is_merely_malformed_is_refused() {
        assert!(normalize_path("/api/../admin").is_err(), "traversal");
        assert!(normalize_path("/api//v2").is_err(), "empty segment");
        assert!(normalize_path(&format!("/{}", "a".repeat(300))).is_err());
    }

    #[test]
    fn grpc_and_h2c_both_name_the_same_protocol() {
        // `grpc` is what an operator calls it; `h2c` is what it is.
        assert_eq!(route::Protocol::parse("grpc"), route::Protocol::H2c);
        assert_eq!(route::Protocol::parse("h2c"), route::Protocol::H2c);
        assert_eq!(route::Protocol::parse("http"), route::Protocol::Unspecified);
        assert_eq!(route::Protocol::parse(""), route::Protocol::Unspecified);
    }

    #[test]
    fn only_a_grpc_route_gets_the_h2c_upstream_scheme() {
        // h2c on an ordinary HTTP backend breaks it in the other direction.
        assert_eq!(route::Protocol::H2c.upstream_scheme(), "h2c://");
        assert_eq!(route::Protocol::Unspecified.upstream_scheme(), "");
    }

    #[test]
    fn routes_are_ordered_so_the_longest_path_matches_first() {
        // Caddy tries handle blocks in order, so /api/v2 has to come before
        // /api, and the domain root is the fallback.
        let service = Service {
            id: "svc_1".to_string(),
            name: "api".to_string(),
            routes: vec![
                Route {
                    domain: "example.com".into(),
                    path: String::new(),
                    port: 8080,
                    ..Default::default()
                },
                Route {
                    domain: "example.com".into(),
                    path: "/api/v2".into(),
                    port: 8082,
                    ..Default::default()
                },
                Route {
                    domain: "example.com".into(),
                    path: "/api".into(),
                    port: 8081,
                    ..Default::default()
                },
                Route {
                    domain: "a.example.com".into(),
                    path: String::new(),
                    port: 8083,
                    ..Default::default()
                },
            ],
            ..Default::default()
        };

        let ordered = routes_of(&[service]);
        let labels: Vec<String> = ordered
            .iter()
            .map(|r| format!("{}{}", r.domain, r.path))
            .collect();
        assert_eq!(
            labels,
            [
                "a.example.com",
                "example.com/api/v2",
                "example.com/api",
                "example.com",
            ]
        );
        // And every route reports the service it came from.
        assert!(ordered.iter().all(|r| r.service_name == "api"));
    }

    #[test]
    fn an_unwritable_path_is_kept_rather_than_silently_widened() {
        // Falling back to an empty path would turn "this path is not one nudo
        // will write" into "route the whole domain", quietly widening what the
        // service receives instead of dropping the route.
        let service = Service {
            id: "svc_1".to_string(),
            name: "api".to_string(),
            routes: vec![Route {
                domain: "example.com".into(),
                path: "/api {\n\trespond \"owned\"\n}".into(),
                port: 8080,
                ..Default::default()
            }],
            ..Default::default()
        };

        let ordered = routes_of(&[service]);
        assert!(
            !ordered[0].path.is_empty(),
            "the invalid path must survive so validation drops the route"
        );
        assert!(ordered[0].validate().is_err());
    }

    #[test]
    fn timestamps_survive_a_round_trip() {
        let now = chrono::Utc::now();
        let restored = from_timestamp(&to_timestamp(now)).expect("in range");
        assert_eq!(now.timestamp(), restored.timestamp());
        assert_eq!(
            now.timestamp_subsec_nanos(),
            restored.timestamp_subsec_nanos()
        );
    }
}
