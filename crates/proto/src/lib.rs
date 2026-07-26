//! Generated gRPC bindings for the control plane API, plus the small
//! conversion helpers every crate needs.
//!
//! `controlplane.proto` at the repo root is the authoritative contract. This
//! crate exists so the server, web tier, CLI and MCP server all speak the exact
//! same generated types.

pub mod v1 {
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
    fn timestamps_survive_a_round_trip() {
        let now = chrono::Utc::now();
        let restored = from_timestamp(&to_timestamp(now)).expect("in range");
        assert_eq!(now.timestamp(), restored.timestamp());
        assert_eq!(now.timestamp_subsec_nanos(), restored.timestamp_subsec_nanos());
    }
}
