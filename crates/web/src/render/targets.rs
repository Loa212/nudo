use super::*;

// ---------------------------------------------------------------------------
// Targets
// ---------------------------------------------------------------------------

/// The target listing.
pub fn targets_list(targets: &[Target]) -> Markup {
    html! {
        (topbar("Targets", Some("Machines nudo can reach over ssh"), html! {
            a .btn.primary href="/targets/new" { "Add target" }
        }))
        div .content {
            div .card.pad-0 {
                @if targets.is_empty() {
                    (empty_state(
                        "No targets",
                        "Add the first machine you want to deploy to. Only an ssh host, a user and a stored key are needed.",
                        Some(("Add target", "/targets/new")),
                    ))
                } @else {
                    div .table-scroll {
                        table {
                            thead {
                                tr {
                                    th { "Name" }
                                    th { "Address" }
                                    th { "Status" }
                                    th { "Agent" }
                                    th { "Labels" }
                                    th { "Last seen" }
                                }
                            }
                            tbody {
                                @for target in targets {
                                    tr {
                                        td {
                                            a href=(format!("/targets/{}", target.id)) { (target.name) }
                                        }
                                        td .mono.nowrap { (address(target)) }
                                        td { div .row { (target_badges(target)) } }
                                        td .small.muted {
                                            @if target.agent_version.is_empty() {
                                                // No agent is a supported mode,
                                                // not a missing value.
                                                "agentless"
                                            } @else {
                                                (target.agent_version)
                                            }
                                        }
                                        td .small { (labels_line(&target.labels)) }
                                        td .nowrap.small.muted { (ago(target.last_seen_at.as_ref())) }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

/// Labels as `k=v` pairs in a stable order, so the same target reads the same
/// way on every render.
pub(super) fn labels_line(labels: &HashMap<String, String>) -> String {
    if labels.is_empty() {
        return "-".to_string();
    }
    let mut pairs: Vec<String> = labels.iter().map(|(k, v)| format!("{k}={v}")).collect();
    pairs.sort();
    pairs.join(", ")
}

/// A single target: connection details, the last check result, and its services.
pub fn target_detail(
    target: &Target,
    services: &[Service],
    statuses: &HashMap<String, UnitStatus>,
    checks: Option<&CheckTargetResponse>,
) -> Markup {
    let owned: Vec<Service> = services
        .iter()
        .filter(|s| s.target_id == target.id)
        .cloned()
        .collect();

    html! {
        (topbar(&target.name, Some(&address(target)), html! {
            // A link, not a POST: the check only probes the target and changes
            // nothing, so it needs neither a CSRF token nor a confirmation.
            a .btn href=(format!("/targets/{}?check=1", target.id)) { "Run checks" }
            a .btn href=(format!("/terminal?target={}", target.id)) { "Terminal" }
            a .btn href=(format!("/targets/{}/edit", target.id)) { "Edit" }
        }))
        div .content {
            @if target.latency_critical {
                (callout("bad", "Latency-critical host", html! {
                    "Unattended mutation is refused for this target. Deploys and unit \
                     actions require an explicit override, and nothing beyond the \
                     configured services should ever run here."
                }))
            }

            div .card {
                div .row {
                    h2 { "Connection" }
                    (target_badges(target))
                }
                dl .dl style="margin-top:12px" {
                    dt { "Host" }         dd .mono { (target.host) }
                    dt { "Port" }         dd .mono { (target.port) }
                    dt { "User" }         dd .mono { (target.user) }
                    // The reference into the secret store, never the key.
                    dt { "SSH key" }      dd .mono { (or_dash(&target.ssh_key_id)) }
                    dt { "Status" }       dd { (target_badge(target.status)) }
                    dt { "Agent" }        dd {
                        @if target.agent_version.is_empty() {
                            span .muted { "agentless (plain ssh)" }
                        } @else {
                            span .mono { (target.agent_version) }
                        }
                    }
                    dt { "Labels" }       dd { (labels_line(&target.labels)) }
                    dt { "Last seen" }    dd { (ago(target.last_seen_at.as_ref())) }
                    dt { "Created" }      dd { (ago(target.created_at.as_ref())) }
                }
            }

            @if let Some(checks) = checks {
                (check_results(checks))
            }

            div .card.pad-0 {
                div .card-head {
                    h2 { "Services" }
                    div .actions {
                        a .btn.small.primary href=(format!("/services/new?target={}", target.id)) {
                            "Add service"
                        }
                    }
                }
                @if owned.is_empty() {
                    (empty_state(
                        "No services on this target",
                        "A service is one systemd unit nudo owns end to end: the binary, the unit file, and its health check.",
                        Some(("Add service", &format!("/services/new?target={}", target.id))),
                    ))
                } @else {
                    // A one-element slice; clippy's needless-borrow suggestion here is
                    // wrong, since the parameter is a slice rather than a reference.
                    (services_rows(&owned, std::slice::from_ref(target), statuses, false))
                }
            }
        }
    }
}

/// The result of the last preflight check, one row per probe.
fn check_results(checks: &CheckTargetResponse) -> Markup {
    html! {
        div .card {
            div .row {
                h2 { "Preflight checks" }
                @if checks.ok {
                    (badge("all passed", BadgeKind::Ok))
                } @else {
                    (badge("problems found", BadgeKind::Bad))
                }
            }
            @if checks.checks.is_empty() {
                p .card-note { "The check returned no probes." }
            } @else {
                dl .dl style="margin-top:12px" {
                    @for check in &checks.checks {
                        dt { (check.name) }
                        dd {
                            div .row {
                                @if check.ok {
                                    (badge("ok", BadgeKind::Ok))
                                } @else {
                                    (badge("failed", BadgeKind::Bad))
                                }
                                @if !check.detail.is_empty() {
                                    span .small.muted { (check.detail) }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

/// Create/edit form for a target.
///
/// The ssh key is a `<select>` over stored secrets and never a text field: a key
/// pasted into a form would be logged by every proxy on the way in, and the
/// proto only ever carries a `ssh_key_id`.
pub fn target_form(existing: Option<&Target>, secrets: &[Secret], csrf: &str) -> Markup {
    let editing = existing.is_some();
    let action = match existing {
        Some(t) => format!("/targets/{}", t.id),
        None => "/targets".to_string(),
    };
    let title = if editing { "Edit target" } else { "Add target" };

    html! {
        (topbar(title, Some("A machine nudo reaches over ssh"), html! {
            a .btn href="/targets" { "Cancel" }
        }))
        div .content {
            form .card method="post" action=(action) {
                (csrf_input(csrf))

                div .fields {
                    div .field {
                        label for="name" { "Name" }
                        input type="text" id="name" name="name" required
                            placeholder="hft-box"
                            value=(existing.map(|t| t.name.as_str()).unwrap_or_default());
                        span .hint { "How this machine is referred to everywhere else." }
                    }
                    div .field {
                        label for="host" { "Host" }
                        input type="text" id="host" name="host" required
                            placeholder="10.0.0.4"
                            value=(existing.map(|t| t.host.as_str()).unwrap_or_default());
                        span .hint { "Hostname or IP." }
                    }
                    div .field {
                        label for="port" { "SSH port" }
                        input type="number" id="port" name="port" min="1" max="65535"
                            value=(existing.map(|t| t.port).filter(|p| *p > 0).unwrap_or(22));
                    }
                    div .field {
                        label for="user" { "SSH user" }
                        input type="text" id="user" name="user" required
                            placeholder="deploy"
                            value=(existing.map(|t| t.user.as_str()).unwrap_or_default());
                        span .hint { "Needs sudo for systemctl and the release directory." }
                    }
                }

                div .field style="margin-top:14px" {
                    label for="ssh_key_id" { "SSH key" }
                    select id="ssh_key_id" name="ssh_key_id" required {
                        option value="" { "Select a stored key…" }
                        @for secret in secrets {
                            option value=(secret.id)
                                selected[existing.is_some_and(|t| t.ssh_key_id == secret.id)] {
                                (secret.name) " (" (scope_label(secret)) ")"
                            }
                        }
                    }
                    span .hint {
                        "Keys live in the secret store and are chosen by reference. "
                        a href="/secrets" { "Add a key" }
                        " if the one you need is not listed."
                    }
                }

                div .field style="margin-top:14px" {
                    label for="labels" { "Labels" }
                    input type="text" id="labels" name="labels"
                        placeholder="env=prod,role=indexer"
                        value=(existing.map(|t| labels_input(&t.labels)).unwrap_or_default());
                    span .hint { "Comma-separated " code { "key=value" } " pairs, used by label selectors." }
                }

                div .field style="margin-top:14px" {
                    div .check {
                        input type="checkbox" id="latency_critical" name="latency_critical" value="1"
                            checked[existing.is_some_and(|t| t.latency_critical)];
                        label for="latency_critical" {
                            "Latency-critical"
                            div .hint {
                                "Refuses unattended mutation and keeps everything \
                                 non-essential off the box. Set this for hosts where a \
                                 stray process costs money."
                            }
                        }
                    }
                }

                div .form-actions {
                    button .btn.primary type="submit" {
                        @if editing { "Save target" } @else { "Add target" }
                    }
                    a .btn href="/targets" { "Cancel" }
                }
            }

            @if editing {
                @let id = existing.map(|t| t.id.as_str()).unwrap_or_default();
                form .card method="post" action=(format!("/targets/{id}/delete")) {
                    (csrf_input(csrf))
                    div .row {
                        div {
                            h2 { "Delete target" }
                            p .card-note {
                                "Removes the target and its services from nudo. Units \
                                 already running on the machine are left alone."
                            }
                        }
                        div style="margin-left:auto" {
                            button .btn.danger type="submit"
                                onclick="return confirm('Delete this target and all of its service definitions from nudo? Units already on the machine will keep running.')" {
                                "Delete target"
                            }
                        }
                    }
                }
            }
        }
    }
}

/// Labels rendered back into the comma-separated form the input accepts.
pub(super) fn labels_input(labels: &HashMap<String, String>) -> String {
    let mut pairs: Vec<String> = labels.iter().map(|(k, v)| format!("{k}={v}")).collect();
    pairs.sort();
    pairs.join(",")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn labels_render_in_a_stable_order_for_tables_and_forms() {
        let labels = HashMap::from([
            ("z".to_string(), "1".to_string()),
            ("a".to_string(), "2".to_string()),
            ("m".to_string(), "3".to_string()),
        ]);

        assert_eq!(labels_line(&labels), "a=2, m=3, z=1");
        assert_eq!(labels_input(&labels), "a=2,m=3,z=1");
        assert_eq!(labels_line(&HashMap::new()), "-");
    }
}
