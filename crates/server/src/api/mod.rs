//! The gRPC surface: one module per proto service.
//!
//! Everything mutating goes through [`Context::authorize`], which enforces the
//! latency-critical guardrail and writes the audit entry. That is the single
//! place those two rules live, so a new RPC cannot forget either.

mod audit;
mod deployments;
mod logs;
mod secrets;
mod services;
mod sources;
mod targets;
mod terminals;
#[cfg(test)]
pub(crate) mod test_support;

pub use audit::AuditService;
pub use deployments::DeploymentsService;
pub use logs::LogsService;
pub use secrets::SecretsService;
pub use services::ServicesApiService;
pub use sources::SourcesService;
pub use targets::TargetsService;
pub use terminals::TerminalsService;

use std::sync::Arc;

use nudo_proto::{Mutation, Target};
use tonic::Status;

use crate::crypto::SecretKey;
use crate::deploy::Engine;
use crate::events::Bus;
use crate::store::{NewAuditEntry, Store};

/// Shared state every service needs.
#[derive(Clone)]
pub struct Context {
    pub store: Store,
    pub bus: Bus,
    pub engine: Engine,
    pub secret_key: SecretKey,
    pub config: Arc<crate::Config>,
}

impl Context {
    pub fn new(store: Store, bus: Bus, secret_key: SecretKey, config: Arc<crate::Config>) -> Self {
        let engine = Engine {
            store: store.clone(),
            bus: bus.clone(),
            secret_key: secret_key.clone(),
            config: config.clone(),
        };
        Self {
            store,
            bus,
            engine,
            secret_key,
            config,
        }
    }

    /// Checks a mutation is permitted and records it.
    ///
    /// `subject_target` is the target the operation touches, when there is one:
    /// that is what the latency-critical guardrail is checked against. Pass
    /// `None` for operations that touch no specific host.
    ///
    /// Returns the resolved actor so the caller can attribute what follows.
    pub async fn authorize(
        &self,
        mutation: Option<&Mutation>,
        action: &str,
        subject_id: &str,
        subject_target: Option<&Target>,
        summary: impl Into<String>,
    ) -> Result<Authorized, Status> {
        let mutation = mutation.cloned().unwrap_or_default();
        let actor = mutation.actor_or_system();

        // The hard guardrail. A latency-critical host is one where an unattended
        // change is unacceptable — the HFT bot's machine — so touching it takes
        // an explicit per-request opt-in, and saying so is the caller's
        // responsibility rather than a default.
        if let Some(target) = subject_target
            && target.latency_critical
            && !mutation.allow_latency_critical
        {
            // Recorded even though it is refused: an agent repeatedly
            // trying to touch the hot-path box is something an operator
            // wants to see.
            self.store
                .audit(NewAuditEntry {
                    actor: actor.clone(),
                    action: format!("{action} (refused)"),
                    subject_id: subject_id.to_string(),
                    dry_run: mutation.dry_run,
                    summary: format!(
                        "refused: {} is latency-critical and allow_latency_critical was not set",
                        target.name
                    ),
                })
                .await;

            return Err(Status::failed_precondition(format!(
                "target {} is marked latency-critical; set allow_latency_critical \
                     on the request to mutate it",
                target.name
            )));
        }

        self.store
            .audit(NewAuditEntry {
                actor: actor.clone(),
                action: action.to_string(),
                subject_id: subject_id.to_string(),
                dry_run: mutation.dry_run,
                summary: summary.into(),
            })
            .await;

        Ok(Authorized {
            actor,
            dry_run: mutation.dry_run,
            idempotency_key: mutation.idempotency_key,
        })
    }

    /// Loads a target or returns a `NOT_FOUND` a client can act on.
    pub async fn require_target(&self, id: &str) -> Result<Target, Status> {
        self.store
            .get_target(id)
            .await
            .map_err(internal)?
            .ok_or_else(|| Status::not_found(format!("no such target: {id}")))
    }

    /// Loads a service.
    pub async fn require_service(&self, id: &str) -> Result<nudo_proto::Service, Status> {
        self.store
            .get_service(id)
            .await
            .map_err(internal)?
            .ok_or_else(|| Status::not_found(format!("no such service: {id}")))
    }

    /// Loads a service together with the target it runs on.
    pub async fn require_service_and_target(
        &self,
        service_id: &str,
    ) -> Result<(nudo_proto::Service, Target), Status> {
        let service = self.require_service(service_id).await?;
        let target = self.require_target(&service.target_id).await?;
        Ok((service, target))
    }

    /// Opens an SSH session to a target.
    pub async fn connect(&self, target: &Target) -> Result<crate::ssh::SshSession, Status> {
        let ssh_target = self
            .engine
            .ssh_target_for(target)
            .await
            .map_err(|e| Status::failed_precondition(format!("{e:#}")))?;

        crate::ssh::SshSession::connect(&ssh_target)
            .await
            .map_err(|e| Status::unavailable(format!("connecting to {}: {e:#}", target.host)))
    }
}

/// The outcome of a successful authorization.
#[derive(Debug, Clone)]
pub struct Authorized {
    pub actor: nudo_proto::Actor,
    pub dry_run: bool,
    pub idempotency_key: String,
}

impl Authorized {
    /// Whether this call has already been performed under its idempotency key.
    ///
    /// A CI job whose connection dropped will retry, and for an operation that is
    /// not naturally idempotent — running a command, opening a terminal — doing
    /// it twice is a real effect. Returns the recorded result id when the key has
    /// been seen, so the caller can return the original outcome instead of acting
    /// again.
    pub async fn replayed(&self, store: &Store, action: &str) -> Result<Option<String>, Status> {
        if self.idempotency_key.is_empty() {
            return Ok(None);
        }
        store
            .check_idempotency(&self.idempotency_key, action)
            .await
            .map_err(internal)
    }

    /// Records that this call was performed, against its idempotency key.
    ///
    /// Failures are logged rather than propagated: the operation has already
    /// happened, and failing the response would make a caller retry something
    /// that succeeded.
    pub async fn record(&self, store: &Store, action: &str, result_id: &str) {
        if self.idempotency_key.is_empty() {
            return;
        }
        if let Err(error) = store
            .record_idempotency(&self.idempotency_key, action, result_id)
            .await
        {
            tracing::error!(
                %error,
                key = %self.idempotency_key,
                %action,
                "recording an idempotency key failed; a retry may act twice"
            );
        }
    }
}

/// Wraps an internal failure as a gRPC status.
///
/// The message is included because this is an operator-facing tool: hiding why a
/// deploy failed would make it unusable. There is no multi-tenant boundary here
/// for the detail to leak across.
pub fn internal(error: impl std::fmt::Display) -> Status {
    Status::internal(format!("{error:#}"))
}

/// Wraps a client mistake as `INVALID_ARGUMENT`.
pub fn invalid(error: impl std::fmt::Display) -> Status {
    Status::invalid_argument(format!("{error:#}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::TargetInput;
    use nudo_proto::{Actor, actor};

    async fn context() -> Context {
        Context::new(
            Store::open_in_memory().await.expect("store"),
            Bus::default(),
            SecretKey::generate(),
            Arc::new(crate::Config::default()),
        )
    }

    async fn target(context: &Context, latency_critical: bool) -> Target {
        context
            .store
            .create_target(&TargetInput {
                name: if latency_critical {
                    "hot-box"
                } else {
                    "normal-box"
                }
                .to_string(),
                host: "10.0.0.1".to_string(),
                latency_critical,
                ..Default::default()
            })
            .await
            .expect("target")
    }

    #[tokio::test]
    async fn a_normal_target_can_be_mutated_without_an_opt_in() {
        let context = context().await;
        let target = target(&context, false).await;

        let authorized = context
            .authorize(
                Some(&Mutation::by(Actor::human("usr_1", "alice"))),
                "Targets.Update",
                &target.id,
                Some(&target),
                "renamed the target",
            )
            .await
            .expect("must be allowed");

        assert_eq!(authorized.actor.label, "alice");
        assert!(!authorized.dry_run);
    }

    #[tokio::test]
    async fn a_latency_critical_target_is_refused_without_the_explicit_opt_in() {
        // The guardrail this tool exists for: nothing touches the hot-path box
        // unattended.
        let context = context().await;
        let target = target(&context, true).await;

        let status = context
            .authorize(
                Some(&Mutation::by(Actor::agent("sess_1", "claude"))),
                "Deployments.Deploy",
                &target.id,
                Some(&target),
                "deploy",
            )
            .await
            .expect_err("must be refused");

        assert_eq!(status.code(), tonic::Code::FailedPrecondition);
        assert!(status.message().contains("latency-critical"));
        // The message says what to do about it.
        assert!(status.message().contains("allow_latency_critical"));
    }

    #[tokio::test]
    async fn a_latency_critical_target_can_be_mutated_with_the_opt_in() {
        let context = context().await;
        let target = target(&context, true).await;

        context
            .authorize(
                Some(&Mutation {
                    actor: Some(Actor::human("usr_1", "alice")),
                    allow_latency_critical: true,
                    ..Default::default()
                }),
                "Deployments.Deploy",
                &target.id,
                Some(&target),
                "deploy",
            )
            .await
            .expect("must be allowed with the opt-in");
    }

    #[tokio::test]
    async fn a_refused_mutation_is_still_recorded() {
        // An agent repeatedly probing the hot-path box is exactly what an
        // operator wants to find in the audit log.
        let context = context().await;
        let target = target(&context, true).await;

        let _ = context
            .authorize(
                Some(&Mutation::by(Actor::agent("sess_1", "claude"))),
                "Deployments.Deploy",
                &target.id,
                Some(&target),
                "deploy",
            )
            .await;

        let entries = context
            .store
            .list_audit("", actor::Kind::Unspecified, 50, 0)
            .await
            .expect("list");
        assert_eq!(entries.len(), 1);
        assert!(entries[0].action.contains("refused"));
        assert!(entries[0].summary.contains("latency-critical"));
    }

    #[tokio::test]
    async fn an_operation_with_no_specific_target_skips_the_guardrail() {
        // Listing sources or putting a global secret touches no host.
        let context = context().await;
        context
            .authorize(
                Some(&Mutation::by(Actor::agent("s", "claude"))),
                "Secrets.Put",
                "sec_1",
                None,
                "stored a secret",
            )
            .await
            .expect("must be allowed");
    }

    #[tokio::test]
    async fn every_authorized_mutation_lands_in_the_audit_log() {
        let context = context().await;
        let target = target(&context, false).await;

        context
            .authorize(
                Some(&Mutation::by(Actor::human("usr_1", "alice"))),
                "Targets.Delete",
                &target.id,
                Some(&target),
                "deleted normal-box",
            )
            .await
            .expect("allowed");

        let entries = context
            .store
            .list_audit(&target.id, actor::Kind::Unspecified, 50, 0)
            .await
            .expect("list");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].action, "Targets.Delete");
        assert_eq!(entries[0].summary, "deleted normal-box");
        assert!(!entries[0].dry_run);
    }

    #[tokio::test]
    async fn a_dry_run_is_recorded_as_such() {
        let context = context().await;
        let target = target(&context, false).await;

        let authorized = context
            .authorize(
                Some(&Mutation {
                    actor: Some(Actor::agent("sess_1", "claude")),
                    dry_run: true,
                    ..Default::default()
                }),
                "Deployments.Deploy",
                &target.id,
                Some(&target),
                "would deploy",
            )
            .await
            .expect("allowed");
        assert!(authorized.dry_run);

        let entries = context
            .store
            .list_audit("", actor::Kind::Agent, 50, 0)
            .await
            .expect("list");
        assert!(entries[0].dry_run);
    }

    #[tokio::test]
    async fn a_mutation_with_no_actor_is_attributed_to_the_system_rather_than_refused() {
        // A missing actor is a client bug, not a reason to fail the operation
        // and leave no record of it.
        let context = context().await;
        let authorized = context
            .authorize(None, "Secrets.Put", "sec_1", None, "stored")
            .await
            .expect("allowed");
        assert_eq!(authorized.actor.kind, actor::Kind::System as i32);

        let entries = context
            .store
            .list_audit("", actor::Kind::System, 50, 0)
            .await
            .expect("list");
        assert_eq!(entries.len(), 1);
    }

    #[tokio::test]
    async fn an_idempotency_key_is_passed_through_to_the_caller() {
        let context = context().await;
        let authorized = context
            .authorize(
                Some(&Mutation {
                    actor: Some(Actor::human("u", "alice")),
                    idempotency_key: "deploy-run-42".to_string(),
                    ..Default::default()
                }),
                "Deployments.Deploy",
                "svc_1",
                None,
                "deploy",
            )
            .await
            .expect("allowed");
        assert_eq!(authorized.idempotency_key, "deploy-run-42");
    }

    #[tokio::test]
    async fn missing_entities_produce_not_found_rather_than_internal() {
        let context = context().await;
        assert_eq!(
            context
                .require_target("tgt_nope")
                .await
                .expect_err("err")
                .code(),
            tonic::Code::NotFound
        );
        assert_eq!(
            context
                .require_service("svc_nope")
                .await
                .expect_err("err")
                .code(),
            tonic::Code::NotFound
        );
    }

    #[tokio::test]
    async fn loading_a_service_also_loads_its_target() {
        let context = context().await;
        let target = target(&context, false).await;
        let service = context
            .store
            .create_service(&nudo_proto::Service {
                target_id: target.id.clone(),
                name: "bot".to_string(),
                ..Default::default()
            })
            .await
            .expect("service");

        let (loaded_service, loaded_target) = context
            .require_service_and_target(&service.id)
            .await
            .expect("load");
        assert_eq!(loaded_service.id, service.id);
        assert_eq!(loaded_target.id, target.id);
    }
}
