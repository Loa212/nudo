use super::*;

// ---------------------------------------------------------------------------
// Dashboard
// ---------------------------------------------------------------------------

/// The overview screen: aggregate counts, a tile per target, recent deployments.
pub fn dashboard(
    targets: &[Target],
    services: &[Service],
    statuses: &HashMap<String, UnitStatus>,
    recent: &[Deployment],
) -> Markup {
    let running = services
        .iter()
        .filter(|s| {
            statuses
                .get(&s.id)
                .is_some_and(|u| u.active_state == "active")
        })
        .count();
    let failed = services
        .iter()
        .filter(|s| {
            statuses
                .get(&s.id)
                .is_some_and(|u| u.active_state == "failed")
        })
        .count();

    html! {
        (topbar("Overview", Some("Every target and service the control plane manages"), html! {
            a .btn href="/targets/new" { "Add target" }
            a .btn.primary href="/services/new" { "Add service" }
        }))
        div .content {
            div .stats {
                (stat("Targets", &targets.len().to_string(), false))
                (stat("Services", &services.len().to_string(), false))
                (stat("Running", &running.to_string(), false))
                // The only count worth colouring: a failed unit needs someone.
                (stat("Failed", &failed.to_string(), failed > 0))
            }

            // Until something has been deployed, the four zeroes above say
            // nothing about what to do next. The checklist does: it names the
            // whole sequence, so the order (a target before a service, a
            // service before a deploy) is visible rather than inferred.
            @if recent.is_empty() {
                (first_run_checklist(!targets.is_empty(), !services.is_empty()))
            }

            // The checklist already asks for a target in its first step, so
            // this card would be the same request twice on a new instance. It
            // is still the right thing to show once someone has deployed and
            // later removed every target.
            @if targets.is_empty() && !recent.is_empty() {
                div .card {
                    (empty_state(
                        "No targets yet",
                        "A target is a machine reachable over ssh. Add one and nudo will check ssh, sudo, systemd and the release directory before you deploy anything.",
                        Some(("Add your first target", "/targets/new")),
                    ))
                }
            } @else if !targets.is_empty() {
                div .grid {
                    @for target in targets {
                        (target_tile(target, services, statuses))
                    }
                }
            }

            div .card.pad-0 {
                div .card-head {
                    h2 { "Recent deployments" }
                    div .actions { a .btn.small href="/deployments" { "All deployments" } }
                }
                @if recent.is_empty() {
                    div .card-body {
                        p .muted { "Nothing has been deployed yet." }
                    }
                } @else {
                    (deployments_table(recent, services))
                }
            }
        }
    }
}

/// One `.stat` cell.
fn stat(label: &str, value: &str, bad: bool) -> Markup {
    html! {
        div .stat .is-bad[bad] {
            div .stat-value { (value) }
            div .stat-label { (label) }
        }
    }
}

/// A target as a clickable tile. Unreachable targets get `.alert` so a red
/// border is visible from across the room.
fn target_tile(
    target: &Target,
    services: &[Service],
    statuses: &HashMap<String, UnitStatus>,
) -> Markup {
    let owned: Vec<&Service> = services
        .iter()
        .filter(|s| s.target_id == target.id)
        .collect();
    let failed = owned
        .iter()
        .filter(|s| {
            statuses
                .get(&s.id)
                .is_some_and(|u| u.active_state == "failed")
        })
        .count();
    let unreachable = target::Status::try_from(target.status) == Ok(target::Status::Unreachable);

    html! {
        a .tile .alert[unreachable || failed > 0] href=(format!("/targets/{}", target.id)) {
            div .tile-title {
                (target.name)
                (target_badges(target))
            }
            div .tile-meta {
                span .mono { (address(target)) }
                span .sep { " · " }
                (owned.len()) " service" @if owned.len() != 1 { "s" }
                @if failed > 0 {
                    span .sep { " · " }
                    (badge(&format!("{failed} failed"), BadgeKind::Bad))
                }
                span .sep { " · " }
                "seen " (ago(target.last_seen_at.as_ref()))
            }
        }
    }
}
