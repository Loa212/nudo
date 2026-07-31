use super::*;

// ---------------------------------------------------------------------------
// Terminal
// ---------------------------------------------------------------------------

/// Mints a terminal grant and renders the page that will spend it.
pub async fn terminal_page(
    State(state): State<AppState>,
    Path(id): Path<String>,
    user: CurrentUser,
) -> Response {
    let mut targets = state.api.targets();
    let target = match targets.get(GetTargetRequest { id: id.clone() }).await {
        Ok(response) => response.into_inner(),
        Err(status) => return grpc_error(status),
    };

    let mut client = state.api.terminals();

    let session = match client
        .create_session(CreateTerminalSessionRequest {
            mutation: Some(mutation(
                &user,
                // Opening a shell on a latency-critical box is a deliberate act,
                // so it is allowed here — the operator navigated to this page
                // for that target on purpose, and the grant is audited.
                &MutationFlags {
                    allow_latency_critical: target.latency_critical.then(|| "on".to_string()),
                },
            )),
            target_id: id,
            initial_command: String::new(),
            cols: 120,
            rows: 32,
        })
        .await
    {
        Ok(response) => response.into_inner(),
        Err(status) => return grpc_error(status),
    };

    page(
        &format!("{} — terminal", target.name),
        Nav::Terminal,
        render::terminal_page(&target, &session.id, &session.token),
    )
}

/// The target chooser, when no target was named.
pub async fn terminal_index(State(state): State<AppState>, _user: CurrentUser) -> Response {
    let targets = state.api.list_targets().await;
    page("Terminal", Nav::Terminal, render::targets_list(&targets))
}

pub use terminal::websocket as terminal_websocket;
