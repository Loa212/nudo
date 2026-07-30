use super::*;

// ---------------------------------------------------------------------------
// Logs
// ---------------------------------------------------------------------------

#[derive(Debug, Default, serde::Deserialize)]
pub struct LogsQuery {
    #[serde(default)]
    pub grep: Option<String>,
    #[serde(default)]
    pub lines: Option<u32>,
    #[serde(default)]
    pub follow: Option<String>,
}

pub async fn logs_view(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(query): Query<LogsQuery>,
    _user: CurrentUser,
) -> Response {
    let mut client = state.api.services();
    let service = match client.get(GetServiceRequest { id: id.clone() }).await {
        Ok(response) => response.into_inner(),
        Err(status) => return grpc_error(status),
    };

    let grep = query.grep.clone().unwrap_or_default();
    let follow = query.follow.is_some();
    let tail = query.lines.unwrap_or(200);

    // A one-shot read for the initial paint; following is the SSE stream's job.
    let lines = read_logs_once(&state, &id, tail, &grep).await;

    page(
        &format!("{} — logs", service.name),
        Nav::Services,
        render::logs_view(&service, &lines, &grep, follow),
    )
}

/// Reads a bounded batch of log lines.
async fn read_logs_once(state: &AppState, service_id: &str, tail: u32, grep: &str) -> Vec<LogLine> {
    let mut client = state.api.logs();

    let Ok(response) = client
        .stream(StreamLogsRequest {
            service_id: service_id.to_string(),
            follow: false,
            tail_lines: tail,
            since_cursor: String::new(),
            since: None,
            grep: grep.to_string(),
        })
        .await
    else {
        return Vec::new();
    };

    let mut stream = response.into_inner();
    let mut lines = Vec::new();

    // A non-following read ends on its own, but a slow or wedged target should
    // not hold the page load, so the wait is bounded.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            break;
        }
        match tokio::time::timeout(remaining, stream.next()).await {
            Ok(Some(Ok(line))) => {
                lines.push(line);
                if lines.len() >= tail.max(1) as usize {
                    break;
                }
            }
            // End of stream, an error, or the deadline.
            _ => break,
        }
    }

    lines
}

/// The live log stream, folded and rendered on a tick.
pub async fn logs_stream(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(query): Query<LogsQuery>,
    _user: CurrentUser,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let grep = query.grep.unwrap_or_default();
    let tail = query.lines.unwrap_or(200);

    let stream = async_stream::stream! {
        let mut client = state.api.logs();
        let Ok(response) = client
            .stream(StreamLogsRequest {
                service_id: id.clone(),
                follow: true,
                tail_lines: tail,
                since_cursor: String::new(),
                since: None,
                grep,
            })
            .await
        else {
            return;
        };
        let mut upstream = response.into_inner();

        // A window rather than everything: a service running for a week must not
        // grow this task's memory without bound.
        const WINDOW: usize = 1_000;
        let mut lines: std::collections::VecDeque<LogLine> = std::collections::VecDeque::new();
        let mut dirty = false;
        let mut ticker = tokio::time::interval(RENDER_INTERVAL);

        loop {
            tokio::select! {
                biased;

                _ = ticker.tick() => {
                    if dirty {
                        dirty = false;
                        let snapshot: Vec<LogLine> = lines.iter().cloned().collect();
                        let html = render::log_lines(&snapshot).into_string();
                        yield Ok(Event::default().event("log").data(html));
                    }
                }

                frame = upstream.next() => {
                    match frame {
                        Some(Ok(line)) => {
                            if lines.len() >= WINDOW {
                                lines.pop_front();
                            }
                            lines.push_back(line);
                            dirty = true;
                        }
                        _ => break,
                    }
                }
            }
        }
    };

    Sse::new(stream).keep_alive(KeepAlive::default())
}
