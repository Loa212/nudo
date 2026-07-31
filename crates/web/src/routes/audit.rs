use super::*;

// ---------------------------------------------------------------------------
// Audit and settings
// ---------------------------------------------------------------------------

#[derive(Debug, Default, serde::Deserialize)]
pub struct AuditQuery {
    #[serde(default)]
    pub subject: Option<String>,
}

pub async fn audit_list(
    State(state): State<AppState>,
    Query(query): Query<AuditQuery>,
    _user: CurrentUser,
) -> Response {
    let mut client = state.api.audit();

    let entries = client
        .list(ListAuditRequest {
            subject_id: query.subject.unwrap_or_default(),
            actor_kind: actor::Kind::Unspecified as i32,
            page_size: 100,
            page_token: String::new(),
        })
        .await
        .map(|response| response.into_inner().entries)
        .unwrap_or_default();

    page("Audit log", Nav::Audit, render::audit_list(&entries))
}
