//! The `Secrets` service. Values go in and never come back out.

use nudo_proto::secrets_server::Secrets;
use nudo_proto::*;
use tonic::{Request, Response, Status};

use super::{Context, internal};

pub struct SecretsService {
    context: Context,
}

impl SecretsService {
    pub fn new(context: Context) -> Self {
        Self { context }
    }
}

#[tonic::async_trait]
impl Secrets for SecretsService {
    async fn put(&self, request: Request<PutSecretRequest>) -> Result<Response<Secret>, Status> {
        let request = request.into_inner();

        // A secret scoped to a target is a change to what that host will run, so
        // the guardrail applies.
        let scope_target = if request.scope_target_id.trim().is_empty() {
            None
        } else {
            Some(
                self.context
                    .require_target(&request.scope_target_id)
                    .await?,
            )
        };

        // A service-scoped secret is checked against the target the service runs
        // on, which is the host the value ends up on.
        let service_target = if request.scope_service_id.trim().is_empty() {
            None
        } else {
            let (_, target) = self
                .context
                .require_service_and_target(&request.scope_service_id)
                .await?;
            Some(target)
        };

        let guardrail_target = scope_target.as_ref().or(service_target.as_ref());

        let authorized = self
            .context
            .authorize(
                request.mutation.as_ref(),
                "Secrets.Put",
                "",
                guardrail_target,
                // The name, never the value.
                format!("stored secret {}", request.name),
            )
            .await?;

        if request.value.is_empty() {
            return Err(Status::invalid_argument(
                "a secret needs a value; delete it instead of storing an empty one",
            ));
        }

        if authorized.dry_run {
            return Ok(Response::new(Secret {
                name: request.name,
                scope_target_id: request.scope_target_id,
                scope_service_id: request.scope_service_id,
                digest: crate::crypto::sha256_hex(&request.value),
                ..Default::default()
            }));
        }

        let secret = self
            .context
            .store
            .put_secret(
                &self.context.secret_key,
                &request.name,
                &request.value,
                &request.scope_target_id,
                &request.scope_service_id,
            )
            .await
            .map_err(super::invalid)?;

        Ok(Response::new(secret))
    }

    async fn list(
        &self,
        request: Request<ListSecretsRequest>,
    ) -> Result<Response<ListSecretsResponse>, Status> {
        let request = request.into_inner();

        // Metadata and a digest only. There is no RPC that returns a value.
        let secrets = self
            .context
            .store
            .list_secrets(&request.target_id, &request.service_id)
            .await
            .map_err(internal)?;

        Ok(Response::new(ListSecretsResponse { secrets }))
    }

    async fn delete(&self, request: Request<DeleteSecretRequest>) -> Result<Response<()>, Status> {
        let request = request.into_inner();
        let secret = self
            .context
            .store
            .get_secret(&request.id)
            .await
            .map_err(internal)?
            .ok_or_else(|| Status::not_found(format!("no such secret: {}", request.id)))?;

        // A service that references this secret would fail its next deploy, so
        // report that rather than letting it be discovered later.
        let referencing = self
            .context
            .store
            .secret_referenced_by(&request.id)
            .await
            .map_err(internal)?;

        let guardrail_target = if secret.scope_target_id.trim().is_empty() {
            None
        } else {
            self.context
                .store
                .get_target(&secret.scope_target_id)
                .await
                .map_err(internal)?
        };

        let authorized = self
            .context
            .authorize(
                request.mutation.as_ref(),
                "Secrets.Delete",
                &request.id,
                guardrail_target.as_ref(),
                format!(
                    "deleted secret {} ({} service(s) referenced it)",
                    secret.name,
                    referencing.len()
                ),
            )
            .await?;

        // Checked before the dry-run return, so a dry run reports the refusal it
        // would actually hit rather than a false success.
        if !referencing.is_empty() {
            return Err(Status::failed_precondition(format!(
                "{} service(s) still reference this secret; \
                 remove it from them first",
                referencing.len()
            )));
        }

        // A target's SSH key is also a secret; removing it would leave the host
        // unreachable with no obvious cause. Checked before the dry-run return so
        // a dry run surfaces it.
        let targets = self
            .context
            .store
            .list_targets("", 500, 0)
            .await
            .map_err(internal)?;
        if let Some(target) = targets.iter().find(|t| t.ssh_key_id == request.id) {
            return Err(Status::failed_precondition(format!(
                "target {} uses this secret as its SSH key; \
                 point it at another key first",
                target.name
            )));
        }

        if authorized.dry_run {
            return Ok(Response::new(()));
        }

        self.context
            .store
            .delete_secret(&request.id)
            .await
            .map_err(super::invalid)?;
        Ok(Response::new(()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::Bus;
    use crate::store::{Store, TargetInput};
    use std::sync::Arc;

    async fn fixture() -> (SecretsService, String, String) {
        let context = Context::new(
            Store::open_in_memory().await.expect("store"),
            Bus::default(),
            crate::crypto::SecretKey::generate(),
            Arc::new(crate::Config::default()),
        );
        let target = context
            .store
            .create_target(&TargetInput {
                name: "box".to_string(),
                host: "10.0.0.1".to_string(),
                ..Default::default()
            })
            .await
            .expect("target");
        let service = context
            .store
            .create_service(&Service {
                target_id: target.id.clone(),
                name: "bot".to_string(),
                ..Default::default()
            })
            .await
            .expect("service");
        (SecretsService::new(context), target.id, service.id)
    }

    fn put(name: &str, value: &str) -> PutSecretRequest {
        PutSecretRequest {
            mutation: Some(Mutation::by(Actor::human("usr_1", "alice"))),
            name: name.to_string(),
            value: value.to_string(),
            scope_target_id: String::new(),
            scope_service_id: String::new(),
        }
    }

    #[tokio::test]
    async fn a_stored_secret_returns_only_metadata_and_a_digest() {
        let service = fixture().await.0;
        let secret = service
            .put(Request::new(put("API_KEY", "super-secret-value")))
            .await
            .expect("put")
            .into_inner();

        assert_eq!(secret.name, "API_KEY");
        assert_eq!(
            secret.digest,
            crate::crypto::sha256_hex("super-secret-value")
        );
        assert!(secret.updated_at.is_some());

        // Nothing in the response carries the value.
        assert!(!format!("{secret:?}").contains("super-secret-value"));
    }

    #[tokio::test]
    async fn no_rpc_returns_a_secret_value() {
        // The listing is the only read path, and it is metadata only.
        let service = fixture().await.0;
        service
            .put(Request::new(put("API_KEY", "super-secret-value")))
            .await
            .expect("put");

        let listed = service
            .list(Request::new(ListSecretsRequest::default()))
            .await
            .expect("list")
            .into_inner();

        assert_eq!(listed.secrets.len(), 1);
        assert!(
            !format!("{listed:?}").contains("super-secret-value"),
            "a value must never appear on the API"
        );
    }

    #[tokio::test]
    async fn an_empty_value_is_refused_rather_than_silently_storing_nothing() {
        let service = fixture().await.0;
        let status = service
            .put(Request::new(put("API_KEY", "")))
            .await
            .expect_err("err");
        assert_eq!(status.code(), tonic::Code::InvalidArgument);
    }

    #[tokio::test]
    async fn a_name_that_is_not_a_valid_environment_variable_is_refused() {
        // The name becomes a key in the target's EnvironmentFile.
        let service = fixture().await.0;
        let status = service
            .put(Request::new(put("has space", "v")))
            .await
            .expect_err("err");
        assert_eq!(status.code(), tonic::Code::InvalidArgument);
    }

    #[tokio::test]
    async fn writing_the_same_name_twice_rotates_it() {
        let service = fixture().await.0;
        let first = service
            .put(Request::new(put("ROTATE", "v1")))
            .await
            .expect("put")
            .into_inner();
        let second = service
            .put(Request::new(put("ROTATE", "v2")))
            .await
            .expect("put")
            .into_inner();

        assert_eq!(first.id, second.id);
        assert_ne!(first.digest, second.digest);

        let listed = service
            .list(Request::new(ListSecretsRequest::default()))
            .await
            .expect("list")
            .into_inner();
        assert_eq!(listed.secrets.len(), 1, "no duplicate row");
    }

    #[tokio::test]
    async fn a_secret_scoped_to_a_latency_critical_target_needs_the_opt_in() {
        // Changing what the hot-path box will run on its next start is a
        // mutation of that box.
        let (service, _, _) = fixture().await;
        let hot = service
            .context
            .store
            .create_target(&TargetInput {
                name: "hot-box".to_string(),
                host: "10.0.0.2".to_string(),
                latency_critical: true,
                ..Default::default()
            })
            .await
            .expect("target");

        let status = service
            .put(Request::new(PutSecretRequest {
                scope_target_id: hot.id.clone(),
                ..put("KEY", "v")
            }))
            .await
            .expect_err("must be refused");
        assert_eq!(status.code(), tonic::Code::FailedPrecondition);

        // Allowed with the opt-in.
        service
            .put(Request::new(PutSecretRequest {
                mutation: Some(Mutation {
                    actor: Some(Actor::human("u", "alice")),
                    allow_latency_critical: true,
                    ..Default::default()
                }),
                scope_target_id: hot.id,
                ..put("KEY", "v")
            }))
            .await
            .expect("must be allowed");
    }

    #[tokio::test]
    async fn a_service_scoped_secret_is_checked_against_the_host_it_lands_on() {
        let (service, _, _) = fixture().await;
        let hot = service
            .context
            .store
            .create_target(&TargetInput {
                name: "hot-box".to_string(),
                host: "10.0.0.2".to_string(),
                latency_critical: true,
                ..Default::default()
            })
            .await
            .expect("target");
        let hot_service = service
            .context
            .store
            .create_service(&Service {
                target_id: hot.id,
                name: "hft".to_string(),
                ..Default::default()
            })
            .await
            .expect("service");

        let status = service
            .put(Request::new(PutSecretRequest {
                scope_service_id: hot_service.id,
                ..put("KEY", "v")
            }))
            .await
            .expect_err("must be refused");
        assert_eq!(status.code(), tonic::Code::FailedPrecondition);
    }

    #[tokio::test]
    async fn scoping_to_something_that_does_not_exist_is_not_found() {
        let service = fixture().await.0;
        assert_eq!(
            service
                .put(Request::new(PutSecretRequest {
                    scope_target_id: "tgt_nope".to_string(),
                    ..put("KEY", "v")
                }))
                .await
                .expect_err("err")
                .code(),
            tonic::Code::NotFound
        );
        assert_eq!(
            service
                .put(Request::new(PutSecretRequest {
                    scope_service_id: "svc_nope".to_string(),
                    ..put("KEY", "v")
                }))
                .await
                .expect_err("err")
                .code(),
            tonic::Code::NotFound
        );
    }

    #[tokio::test]
    async fn a_dry_run_put_returns_the_digest_without_storing_anything() {
        let service = fixture().await.0;
        let planned = service
            .put(Request::new(PutSecretRequest {
                mutation: Some(Mutation {
                    actor: Some(Actor::agent("s", "claude")),
                    dry_run: true,
                    ..Default::default()
                }),
                ..put("PHANTOM", "value")
            }))
            .await
            .expect("dry run")
            .into_inner();

        assert!(planned.id.is_empty());
        assert_eq!(planned.digest, crate::crypto::sha256_hex("value"));

        let listed = service
            .list(Request::new(ListSecretsRequest::default()))
            .await
            .expect("list")
            .into_inner();
        assert!(listed.secrets.is_empty());
    }

    #[tokio::test]
    async fn the_audit_entry_names_the_secret_but_not_its_value() {
        let service = fixture().await.0;
        service
            .put(Request::new(put("API_KEY", "super-secret-value")))
            .await
            .expect("put");

        let audit = service
            .context
            .store
            .list_audit("", actor::Kind::Unspecified, 50, 0)
            .await
            .expect("audit");
        let entry = audit
            .iter()
            .find(|e| e.action == "Secrets.Put")
            .expect("entry");
        assert!(entry.summary.contains("API_KEY"));
        assert!(
            !entry.summary.contains("super-secret-value"),
            "the audit log must not record secret values"
        );
    }

    #[tokio::test]
    async fn listing_can_be_narrowed_to_a_scope() {
        let (service, target_id, service_id) = fixture().await;
        service
            .put(Request::new(put("GLOBAL", "v")))
            .await
            .expect("put");
        service
            .put(Request::new(PutSecretRequest {
                scope_target_id: target_id.clone(),
                ..put("TARGET_SCOPED", "v")
            }))
            .await
            .expect("put");
        service
            .put(Request::new(PutSecretRequest {
                scope_service_id: service_id.clone(),
                ..put("SERVICE_SCOPED", "v")
            }))
            .await
            .expect("put");

        let all = service
            .list(Request::new(ListSecretsRequest::default()))
            .await
            .expect("list")
            .into_inner();
        assert_eq!(all.secrets.len(), 3);

        let scoped = service
            .list(Request::new(ListSecretsRequest {
                target_id,
                service_id: String::new(),
            }))
            .await
            .expect("list")
            .into_inner();
        assert_eq!(scoped.secrets.len(), 1);
        assert_eq!(scoped.secrets[0].name, "TARGET_SCOPED");
    }

    #[tokio::test]
    async fn a_secret_a_service_references_cannot_be_deleted() {
        let (service, target_id, _) = fixture().await;
        let secret = service
            .put(Request::new(put("IN_USE", "v")))
            .await
            .expect("put")
            .into_inner();

        service
            .context
            .store
            .create_service(&Service {
                target_id,
                name: "consumer".to_string(),
                secret_ids: vec![secret.id.clone()],
                ..Default::default()
            })
            .await
            .expect("service");

        let status = service
            .delete(Request::new(DeleteSecretRequest {
                mutation: Some(Mutation::by(Actor::human("u", "alice"))),
                id: secret.id,
            }))
            .await
            .expect_err("must be refused");
        assert_eq!(status.code(), tonic::Code::FailedPrecondition);
        assert!(status.message().contains("still reference"));
    }

    #[tokio::test]
    async fn a_secret_serving_as_a_targets_ssh_key_cannot_be_deleted() {
        // Removing it would leave the host unreachable with no obvious cause.
        let (service, target_id, _) = fixture().await;
        let key = service
            .put(Request::new(put(
                "SSH_KEY",
                "-----BEGIN OPENSSH PRIVATE KEY-----",
            )))
            .await
            .expect("put")
            .into_inner();

        service
            .context
            .store
            .update_target(
                &target_id,
                &Target {
                    ssh_key_id: key.id.clone(),
                    ..Default::default()
                },
                &["ssh_key_id".to_string()],
            )
            .await
            .expect("update");

        let status = service
            .delete(Request::new(DeleteSecretRequest {
                mutation: Some(Mutation::by(Actor::human("u", "alice"))),
                id: key.id,
            }))
            .await
            .expect_err("must be refused");
        assert_eq!(status.code(), tonic::Code::FailedPrecondition);
        assert!(status.message().contains("SSH key"));
    }

    #[tokio::test]
    async fn a_dry_run_delete_reports_a_refusal_rather_than_a_false_success() {
        // A dry run exists to tell the caller what would happen. Returning Ok for
        // something that would be rejected is worse than not offering a dry run.
        let (service, target_id, _) = fixture().await;
        let secret = service
            .put(Request::new(put("IN_USE", "v")))
            .await
            .expect("put")
            .into_inner();

        service
            .context
            .store
            .create_service(&Service {
                target_id,
                name: "consumer".to_string(),
                secret_ids: vec![secret.id.clone()],
                ..Default::default()
            })
            .await
            .expect("service");

        let status = service
            .delete(Request::new(DeleteSecretRequest {
                mutation: Some(Mutation {
                    actor: Some(Actor::agent("s", "claude")),
                    dry_run: true,
                    ..Default::default()
                }),
                id: secret.id,
            }))
            .await
            .expect_err("a dry run must report the refusal");
        assert_eq!(status.code(), tonic::Code::FailedPrecondition);
        assert!(status.message().contains("still reference"));
    }

    #[tokio::test]
    async fn a_dry_run_delete_of_a_targets_ssh_key_also_reports_the_refusal() {
        let (service, target_id, _) = fixture().await;
        let key = service
            .put(Request::new(put(
                "SSH_KEY",
                "-----BEGIN OPENSSH PRIVATE KEY-----",
            )))
            .await
            .expect("put")
            .into_inner();

        service
            .context
            .store
            .update_target(
                &target_id,
                &Target {
                    ssh_key_id: key.id.clone(),
                    ..Default::default()
                },
                &["ssh_key_id".to_string()],
            )
            .await
            .expect("update");

        let status = service
            .delete(Request::new(DeleteSecretRequest {
                mutation: Some(Mutation {
                    actor: Some(Actor::agent("s", "claude")),
                    dry_run: true,
                    ..Default::default()
                }),
                id: key.id,
            }))
            .await
            .expect_err("a dry run must report the refusal");
        assert!(status.message().contains("SSH key"));
    }

    #[tokio::test]
    async fn an_unreferenced_secret_can_be_deleted() {
        let service = fixture().await.0;
        let secret = service
            .put(Request::new(put("GONE", "v")))
            .await
            .expect("put")
            .into_inner();

        service
            .delete(Request::new(DeleteSecretRequest {
                mutation: Some(Mutation::by(Actor::human("u", "alice"))),
                id: secret.id,
            }))
            .await
            .expect("delete");

        let listed = service
            .list(Request::new(ListSecretsRequest::default()))
            .await
            .expect("list")
            .into_inner();
        assert!(listed.secrets.is_empty());
    }

    #[tokio::test]
    async fn deleting_a_missing_secret_is_not_found() {
        let service = fixture().await.0;
        let status = service
            .delete(Request::new(DeleteSecretRequest {
                mutation: Some(Mutation::by(Actor::human("u", "alice"))),
                id: "sec_nope".to_string(),
            }))
            .await
            .expect_err("err");
        assert_eq!(status.code(), tonic::Code::NotFound);
    }
}
