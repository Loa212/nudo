//! The `Audit` service.

use nudo_proto::audit_server::Audit;
use nudo_proto::*;
use tonic::{Request, Response, Status};

use super::{Context, internal};
use crate::store::{page_offset, page_size};

pub struct AuditService {
    context: Context,
}

impl AuditService {
    pub fn new(context: Context) -> Self {
        Self { context }
    }
}

#[tonic::async_trait]
impl Audit for AuditService {
    async fn list(
        &self,
        request: Request<ListAuditRequest>,
    ) -> Result<Response<ListAuditResponse>, Status> {
        let request = request.into_inner();
        let limit = page_size(request.page_size);
        let offset = page_offset(&request.page_token);

        // An unknown enum value filters nothing rather than erroring, so a newer
        // client cannot make this endpoint unusable.
        let actor_kind =
            actor::Kind::try_from(request.actor_kind).unwrap_or(actor::Kind::Unspecified);

        let entries = self
            .context
            .store
            .list_audit(&request.subject_id, actor_kind, limit, offset)
            .await
            .map_err(internal)?;

        let next_page_token = crate::store::next_page_token(offset, entries.len(), limit);
        Ok(Response::new(ListAuditResponse {
            entries,
            next_page_token,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::Bus;
    use crate::store::{NewAuditEntry, Store};
    use std::sync::Arc;

    async fn service() -> AuditService {
        AuditService::new(Context::new(
            Store::open_in_memory().await.expect("store"),
            Bus::default(),
            crate::crypto::SecretKey::generate(),
            Arc::new(crate::Config::default()),
        ))
    }

    async fn record(service: &AuditService, action: &str, subject: &str, actor: Actor) {
        service
            .context
            .store
            .audit(NewAuditEntry {
                actor,
                action: action.to_string(),
                subject_id: subject.to_string(),
                dry_run: false,
                summary: format!("did {action}"),
            })
            .await;
        // Distinct stored timestamps so ordering is deterministic.
        tokio::time::sleep(std::time::Duration::from_millis(2)).await;
    }

    #[tokio::test]
    async fn entries_are_returned_newest_first() {
        let service = service().await;
        record(&service, "First", "svc_1", Actor::human("u", "alice")).await;
        record(&service, "Second", "svc_1", Actor::human("u", "alice")).await;

        let listed = service
            .list(Request::new(ListAuditRequest::default()))
            .await
            .expect("list")
            .into_inner();
        assert_eq!(listed.entries.len(), 2);
        assert_eq!(listed.entries[0].action, "Second");
    }

    #[tokio::test]
    async fn entries_can_be_filtered_by_subject_and_by_actor_kind() {
        let service = service().await;
        record(&service, "ByHuman", "svc_1", Actor::human("u", "alice")).await;
        record(&service, "ByAgent", "svc_1", Actor::agent("s", "claude")).await;
        record(&service, "Elsewhere", "svc_2", Actor::agent("s", "claude")).await;

        let by_subject = service
            .list(Request::new(ListAuditRequest {
                subject_id: "svc_1".to_string(),
                ..Default::default()
            }))
            .await
            .expect("list")
            .into_inner();
        assert_eq!(by_subject.entries.len(), 2);

        let by_kind = service
            .list(Request::new(ListAuditRequest {
                actor_kind: actor::Kind::Agent as i32,
                ..Default::default()
            }))
            .await
            .expect("list")
            .into_inner();
        assert_eq!(by_kind.entries.len(), 2);

        let both = service
            .list(Request::new(ListAuditRequest {
                subject_id: "svc_1".to_string(),
                actor_kind: actor::Kind::Agent as i32,
                ..Default::default()
            }))
            .await
            .expect("list")
            .into_inner();
        assert_eq!(both.entries.len(), 1);
        assert_eq!(both.entries[0].action, "ByAgent");
    }

    #[tokio::test]
    async fn an_unknown_actor_kind_filters_nothing_rather_than_erroring() {
        // A newer client sending a kind this build does not know must not make
        // the audit log unreadable.
        let service = service().await;
        record(&service, "Something", "svc_1", Actor::human("u", "alice")).await;

        let listed = service
            .list(Request::new(ListAuditRequest {
                actor_kind: 999,
                ..Default::default()
            }))
            .await
            .expect("list")
            .into_inner();
        assert_eq!(listed.entries.len(), 1);
    }

    #[tokio::test]
    async fn listing_paginates() {
        let service = service().await;
        for i in 0..5 {
            record(
                &service,
                &format!("A{i}"),
                "svc_1",
                Actor::human("u", "alice"),
            )
            .await;
        }

        let first = service
            .list(Request::new(ListAuditRequest {
                page_size: 2,
                ..Default::default()
            }))
            .await
            .expect("list")
            .into_inner();
        assert_eq!(first.entries.len(), 2);
        assert!(!first.next_page_token.is_empty());

        let second = service
            .list(Request::new(ListAuditRequest {
                page_size: 2,
                page_token: first.next_page_token,
                ..Default::default()
            }))
            .await
            .expect("list")
            .into_inner();
        assert!(
            first
                .entries
                .iter()
                .all(|a| second.entries.iter().all(|b| a.id != b.id))
        );
    }

    #[tokio::test]
    async fn an_empty_log_lists_cleanly() {
        let service = service().await;
        let listed = service
            .list(Request::new(ListAuditRequest::default()))
            .await
            .expect("list")
            .into_inner();
        assert!(listed.entries.is_empty());
        assert!(listed.next_page_token.is_empty());
    }

    #[tokio::test]
    async fn each_entry_carries_its_actor() {
        let service = service().await;
        record(
            &service,
            "Deploy",
            "svc_1",
            Actor::agent("sess_1", "claude"),
        )
        .await;

        let listed = service
            .list(Request::new(ListAuditRequest::default()))
            .await
            .expect("list")
            .into_inner();
        let entry_actor = listed.entries[0].actor.clone().expect("actor");
        assert_eq!(entry_actor.kind, actor::Kind::Agent as i32);
        assert_eq!(entry_actor.label, "claude");
        assert_eq!(entry_actor.id, "sess_1");
    }
}
