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
