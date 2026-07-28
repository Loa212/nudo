use super::*;

/// The service listing across all targets.
pub fn services_list(
    services: &[Service],
    targets: &[Target],
    statuses: &HashMap<String, UnitStatus>,
) -> Markup {
    html! {
        (topbar("Services", Some("One systemd unit each, deployed and health-checked"), html! {
            a .btn.primary href="/services/new" { "Add service" }
        }))
        div .content {
            div .card.pad-0 {
                @if services.is_empty() {
                    (empty_state(
                        "No services",
                        "A service ties a binary to a systemd unit on one target, with a health check that decides whether a deploy stands or rolls back.",
                        Some(("Add service", "/services/new")),
                    ))
                } @else {
                    // Live unit status: the web tier holds the WatchUnitStatus
                    // stream and pushes the rendered table, so a service that
                    // failed since the page loaded shows it without a reload.
                    // The whole table is replaced because each frame is a full
                    // snapshot, not a delta.
                    div #service-table hx-ext="sse" sse-connect="/services/stream"
                        sse-swap="rows" hx-swap="innerHTML" {
                        (services_rows(services, targets, statuses, true))
                    }
                }
            }
        }
    }
}

/// The shared service table. `with_target` adds the target column, which is
/// noise on a target's own detail page.
pub fn services_rows(
    services: &[Service],
    targets: &[Target],
    statuses: &HashMap<String, UnitStatus>,
    with_target: bool,
) -> Markup {
    html! {
        div .table-scroll {
            table {
                thead {
                    tr {
                        th { "Service" }
                        @if with_target { th { "Target" } }
                        th { "Status" }
                        th { "Source" }
                        th { "Release" }
                        th { "Memory" }
                        th { "Since" }
                    }
                }
                tbody {
                    @for service in services {
                        @let status = statuses.get(&service.id);
                        tr {
                            td {
                                a href=(format!("/services/{}", service.id)) { (service.name) }
                                @let unit = service.unit.as_ref().map(|u| u.unit_name.clone()).unwrap_or_default();
                                @if !unit.is_empty() {
                                    div .small.faint.mono { (unit) }
                                }
                            }
                            @if with_target {
                                td {
                                    a href=(format!("/targets/{}", service.target_id)) {
                                        (target_name(&service.target_id, targets))
                                    }
                                }
                            }
                            td {
                                div .row {
                                    @match status {
                                        Some(status) => (unit_badge(status)),
                                        // No status yet means the watch stream has
                                        // not reported; say so rather than guessing.
                                        None => (badge("no data", BadgeKind::Neutral)),
                                    }
                                    @if status.is_some_and(|s| s.restart_count > 0) {
                                        span .small.muted {
                                            (status.map(|s| s.restart_count).unwrap_or(0)) " restarts"
                                        }
                                    }
                                }
                            }
                            td .small { (artifact_summary(service)) }
                            td .mono.small {
                                @if service.current_release_id.is_empty() {
                                    span .muted { "never deployed" }
                                } @else {
                                    (service.current_release_id)
                                }
                            }
                            td .num.small {
                                @match status.map(|s| s.memory_bytes).filter(|b| *b > 0) {
                                    Some(memory) => (bytes(memory)),
                                    None => "-",
                                }
                            }
                            td .nowrap.small.muted {
                                (status.map(|s| ago(s.since.as_ref())).unwrap_or_else(|| "-".to_string()))
                            }
                        }
                    }
                }
            }
        }
    }
}
