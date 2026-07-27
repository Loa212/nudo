use super::*;

// ---------------------------------------------------------------------------
// Deployments
// ---------------------------------------------------------------------------

/// The deployment history across services.
pub fn deployments_list(deployments: &[Deployment], services: &[Service]) -> Markup {
    html! {
        (topbar("Deployments", Some("Every deploy, rollback and cancellation"), html! {}))
        div .content {
            div .card.pad-0 {
                @if deployments.is_empty() {
                    (empty_state(
                        "No deployments",
                        "Deploying a service records a build, an upload, a symlink swap and a health check here.",
                        Some(("Go to services", "/services")),
                    ))
                } @else {
                    (deployments_table(deployments, services))
                }
            }
        }
    }
}

/// The shared deployment table, reused by the dashboard and the service page.
pub(super) fn deployments_table(deployments: &[Deployment], services: &[Service]) -> Markup {
    html! {
        div .table-scroll {
            table {
                thead {
                    tr {
                        th { "Deployment" }
                        th { "Service" }
                        th { "Status" }
                        th { "Actor" }
                        th { "Started" }
                        th { "Duration" }
                        th { "Error" }
                    }
                }
                tbody {
                    @for deployment in deployments {
                        tr {
                            td .mono.small {
                                a href=(format!("/deployments/{}", deployment.id)) { (deployment.id) }
                            }
                            td {
                                a href=(format!("/services/{}", deployment.service_id)) {
                                    (service_name(&deployment.service_id, services))
                                }
                            }
                            td { (deployment_badge(deployment.status)) }
                            td .small {
                                @match deployment.actor.as_ref() {
                                    Some(actor) => {
                                        (or_dash(&actor.label))
                                        span .sep { " · " }
                                        span .faint { (actor.kind_str()) }
                                    },
                                    None => span .muted { "-" },
                                }
                            }
                            td .nowrap.small.muted { (ago(deployment.started_at.as_ref())) }
                            td .nowrap.small {
                                (duration(deployment.started_at.as_ref(), deployment.finished_at.as_ref()))
                            }
                            // Truncated: a multi-line build error would make the
                            // row taller than the screen.
                            td .small { (truncate(&deployment.error, 60)) }
                        }
                    }
                }
            }
        }
    }
}

/// A single deployment, with its output.
///
/// While the deployment is live the log pane is an SSE target: htmx's sse
/// extension connects to the stream and swaps [`deployment_log_lines`] into
/// `#deploy-log`. Once terminal there is nothing to subscribe to, so the same
/// markup is rendered statically and no connection is opened.
pub fn deployment_detail(
    deployment: &Deployment,
    service: &Service,
    lines: &[(chrono::DateTime<chrono::Utc>, bool, String)],
    live: bool,
    csrf: &str,
) -> Markup {
    let status =
        deployment::Status::try_from(deployment.status).unwrap_or(deployment::Status::Unspecified);
    let running = !status.is_terminal();

    html! {
        (topbar(
            &format!("Deploy {}", deployment.id),
            Some(&service.name),
            html! {
                (deployment_badge(deployment.status))
                @if running {
                    form method="post" action=(format!("/deployments/{}/cancel", deployment.id)) {
                        (csrf_input(csrf))
                        button .btn.danger type="submit"
                            onclick="return confirm('Cancel this deployment? A partially uploaded release is left in place and the unit keeps running its current version.')" {
                            "Cancel"
                        }
                    }
                }
                a .btn href=(format!("/services/{}", service.id)) { "Service" }
            },
        ))
        div .content {
            @if !deployment.error.is_empty() {
                (callout("bad", "Deployment failed", html! {
                    // Full text, not truncated: this is the one place the whole
                    // error belongs.
                    pre .unit style="margin-top:6px" { (deployment.error) }
                }))
            }

            div .card {
                dl .dl {
                    dt { "Service" }
                    dd { a href=(format!("/services/{}", service.id)) { (service.name) } }
                    dt { "Status" }   dd { (deployment_badge(deployment.status)) }
                    dt { "Release" }  dd .mono { (or_dash(&deployment.release_id)) }
                    dt { "Previous" } dd .mono {
                        (or_dash(&deployment.previous_release_id))
                        @if !deployment.previous_release_id.is_empty() {
                            span .sep { " · " }
                            span .small.faint { "what a rollback returns to" }
                        }
                    }
                    dt { "Actor" }    dd {
                        @match deployment.actor.as_ref() {
                            Some(actor) => {
                                (or_dash(&actor.label))
                                span .sep { " · " }
                                span .small.faint { (actor.kind_str()) }
                            },
                            None => span .muted { "-" },
                        }
                    }
                    dt { "Started" }  dd { (ago(deployment.started_at.as_ref())) }
                    dt { "Duration" } dd {
                        (duration(deployment.started_at.as_ref(), deployment.finished_at.as_ref()))
                    }
                }
            }

            div .card.pad-0 {
                div .card-head {
                    h2 { "Output" }
                    div .actions {
                        @if live {
                            (badge("live", BadgeKind::Info))
                        }
                    }
                }
                div .card-body {
                    @if live {
                        // The wrapper carries the subscription so the swapped
                        // fragment stays plain `.line` divs.
                        div hx-ext="sse" sse-connect=(format!("/deployments/{}/stream", deployment.id)) {
                            // The stream sends the whole pane each tick, not a delta — that is what
                            // lets a reconnect resynchronise instead of interleaving.
                            // So the swap replaces rather than appends; appending would
                            // re-add every line already shown, on every tick.
                            div #deploy-log .logs sse-swap="log" hx-swap="innerHTML" {
                                (deployment_log_lines(lines))
                            }
                        }
                    } @else {
                        div #deploy-log .logs {
                            (deployment_log_lines(lines))
                        }
                    }
                }
            }
        }
    }
}

/// The deployment log fragment: `.line` divs and nothing else.
///
/// Kept separate from [`deployment_detail`] because the SSE stream sends exactly
/// this, appended into `#deploy-log`. A fragment that carried any wrapper would
/// nest a new `.logs` box on every event.
pub fn deployment_log_lines(lines: &[(chrono::DateTime<chrono::Utc>, bool, String)]) -> Markup {
    html! {
        @if lines.is_empty() {
            div .line { span .msg .placeholder { "Waiting for output…" } }
        }
        @for (at, stderr, text) in lines {
            // Build output is untrusted: it can contain anything a compiler or a
            // remote shell decides to print. Maud escapes it.
            div class=(log_line_class(*stderr, text)) {
                span .at { (at.format("%H:%M:%S")) }
                span .msg { (text) }
            }
        }
    }
}

/// Classifies a deployment output line.
///
/// stderr is highlighted as an error even though many build tools log progress
/// there — being told about a line that turned out to be fine is cheaper than
/// missing the one that was not. Lines the deploy driver prefixes with `---`
/// are its own step markers, so they read as commands rather than output.
fn log_line_class(stderr: bool, text: &str) -> &'static str {
    if text.starts_with("---") {
        "line cmd"
    } else if stderr {
        "line err"
    } else {
        "line"
    }
}
