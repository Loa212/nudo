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
    csrf: &str,
) -> Markup {
    let owned: Vec<Service> = services
        .iter()
        .filter(|s| s.target_id == target.id)
        .cloned()
        .collect();
    let host_key = target.host_key.clone().unwrap_or_default();

    html! {
        (topbar(&target.name, Some(&address(target)), html! {
            // A link, not a POST: the check only probes the target and changes
            // nothing, so it needs neither a CSRF token nor a confirmation.
            a .btn href=(format!("/targets/{}?check=1", target.id)) { "Run checks" }
            a .btn href=(format!("/terminal?target={}", target.id)) { "Terminal" }
            a .btn href=(format!("/targets/{}/edit", target.id)) { "Edit" }
        }))
        div .content {
            // First on the page, above even the latency-critical warning: while
            // a key change is unreviewed, nothing else about this target can be
            // acted on, so nothing else is the first thing to read.
            @if !host_key.pending_key.is_empty() {
                (host_key_change(target, &host_key, csrf))
            }

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
                    dt { "Host key" }     dd { (host_key_summary(&host_key)) }
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

            (ingress_card(target, &owned, csrf))

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

/// A preview of the proxy config nudo would write for this target.
///
/// The `View unit` of ingress. For a managed target it is what the next reload
/// writes; for an external one it is the whole of the feature — copy it into
/// the proxy you run yourself.
pub fn ingress_config(
    target_id: &str,
    target_name: &str,
    response: &RenderIngressResponse,
) -> Markup {
    html! {
        (topbar(target_name, Some("Rendered proxy config"), html! {
            a .btn href=(format!("/targets/{target_id}")) { "Back to target" }
        }))
        div .content {
            (callout("info", "This is a preview", html! {
                "Rendered from the current configuration, not read from the target. \
                 It is written to "
                span .mono { (response.path) }
                " on the next reload or deploy."
            }))
            div .card {
                pre .unit { (response.config) }
            }
        }
    }
}

/// Ingress: whether services on this host are reachable by domain.
///
/// Always rendered, including when there is none, because "nudo can do this and
/// is not doing it here" is information an operator wants on the page rather
/// than in the docs.
fn ingress_card(target: &Target, services: &[Service], csrf: &str) -> Markup {
    // Named `state` rather than `ingress` so it does not shadow the proto
    // module the enums live in.
    let state = target.ingress.clone().unwrap_or_default();
    let mode = ingress::Mode::try_from(state.mode).unwrap_or(ingress::Mode::Unspecified);
    let status = ingress::Status::try_from(state.status).unwrap_or(ingress::Status::Unspecified);

    // Flattened to routes rather than services, because one service can hold
    // several and the table is a list of ways in, not a list of services.
    // Sorted the same way the renderer sorts them, so the page and the config
    // read in the same order.
    let routed: Vec<Route> = nudo_proto::routes_of(services);

    html! {
        div .card {
            div .row {
                h2 { "Ingress" }
                @if mode != ingress::Mode::Unspecified {
                    (ingress_badge(status))
                }
            }

            @match mode {
                ingress::Mode::Unspecified => (ingress_off(target, csrf)),
                _ => {
                    dl .dl style="margin-top:12px" {
                        dt { "Mode" }
                        dd {
                            @if mode == ingress::Mode::Managed {
                                "nudo installs and reloads Caddy here"
                            } @else {
                                "you run the proxy; nudo only renders its config"
                            }
                        }
                        dt { "Certificates" }
                        dd {
                            @if state.acme_email.is_empty() {
                                span .muted { "automatic, no contact address set" }
                            } @else {
                                span .mono { (state.acme_email) }
                            }
                        }
                        @if !state.version.is_empty() {
                            dt { "Caddy" }  dd .mono { (state.version) }
                        }
                        dt { "Reloaded" }   dd { (ago(state.last_reload_at.as_ref())) }
                    }

                    // A degraded proxy is serving its previous routes, which is
                    // easy to miss precisely because the site still works.
                    @if !state.last_error.is_empty() {
                        (callout("bad", "The last reload was rejected", html! {
                            "The proxy is still serving the routes from before it, so this \
                             may not be visible from outside. Fix the cause and reload."
                            pre .mono.small style="margin-top:8px" { (state.last_error) }
                        }))
                    }

                    @if routed.is_empty() {
                        p .card-note {
                            "No service on this target has a domain yet. Set one on a \
                             service to route traffic to it."
                        }
                    } @else {
                        table .table style="margin-top:12px" {
                            thead { tr { th { "Address" } th { "Service" } th { "Backend" } } }
                            tbody {
                                @for route in &routed {
                                    tr {
                                        td {
                                            a href=(format!("https://{}{}", route.domain, route.path))
                                              target="_blank" rel="noreferrer noopener" {
                                                (route.domain) (route.path)
                                            }
                                        }
                                        td {
                                            a href=(format!("/services/{}", route.service_id)) {
                                                (route.service_name)
                                            }
                                        }
                                        td .mono.small {
                                            ":" (route.port)
                                            // Named on the row rather than in a
                                            // legend: a gRPC route reached over
                                            // HTTP/1.1 fails in a way that looks
                                            // like the service being broken.
                                            @if route.protocol_or_default() == route::Protocol::H2c {
                                                span .sep { " · " }
                                                span .muted { "gRPC" }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }

                    div .form-actions {
                        a .btn.small href=(format!("/targets/{}/ingress/config", target.id)) {
                            "View config"
                        }
                        @if mode == ingress::Mode::Managed {
                            form method="post"
                                 action=(format!("/targets/{}/ingress/reload", target.id)) {
                                (csrf_input(csrf))
                                @if target.latency_critical {
                                    input type="hidden" name="allow_latency_critical" value="1";
                                }
                                button .btn.small type="submit" { "Reload" }
                            }
                        }
                        form method="post"
                             action=(format!("/targets/{}/ingress/disable", target.id)) {
                            (csrf_input(csrf))
                            @if target.latency_critical {
                                input type="hidden" name="allow_latency_critical" value="1";
                            }
                            button .btn.small.danger type="submit"
                                onclick=(format!(
                                    "return confirm('Turn ingress off for {}? Services here stop being reachable by domain.')",
                                    target.name
                                )) {
                                "Disable"
                            }
                        }
                    }
                }
            }
        }
    }
}

/// The enable form, shown when this target has no ingress.
fn ingress_off(target: &Target, csrf: &str) -> Markup {
    html! {
        p .card-note {
            "Services on this host are reached by IP and port. Turn ingress on and nudo \
             installs Caddy here, routes each service's domain to it, and gets a \
             certificate from Let's Encrypt automatically."
        }

        @if target.latency_critical {
            (callout("bad", "This host is latency-critical", html! {
                "A reverse proxy adds a hop to every request and a process competing for \
                 CPU and cache. That may be exactly wrong here — enabling it is allowed, \
                 but it is a deliberate choice rather than a default."
            }))
        }

        form method="post" action=(format!("/targets/{}/ingress/enable", target.id)) {
            (csrf_input(csrf))
            @if target.latency_critical {
                input type="hidden" name="allow_latency_critical" value="1";
            }

            div .form-row {
                label {
                    "Contact address"
                    input type="email" name="acme_email" placeholder="ops@example.com";
                    span .hint {
                        "Where Let's Encrypt sends expiry warnings. Optional, but without \
                         it the first notice of an expiring certificate is the outage."
                    }
                }
            }
            div .form-row {
                label {
                    "Mode"
                    select name="mode" {
                        option value="managed" selected { "Managed — nudo installs and drives Caddy" }
                        option value="external" { "External — you run the proxy, nudo renders its config" }
                    }
                }
            }
            div .form-actions {
                button .btn.primary type="submit" { "Enable ingress" }
            }
        }
    }
}

fn ingress_badge(status: ingress::Status) -> Markup {
    match status {
        ingress::Status::Active => badge("active", BadgeKind::Ok),
        ingress::Status::Degraded => badge("degraded", BadgeKind::Bad),
        ingress::Status::Pending => badge("pending", BadgeKind::Warn),
        ingress::Status::Unspecified => badge("unknown", BadgeKind::Neutral),
    }
}

/// The pinned host key, at a glance.
///
/// The fingerprint rather than the key: it is what an operator compares against
/// `ssh-keyscan`, and the full key would push every other row off the line.
fn host_key_summary(host_key: &HostKey) -> Markup {
    html! {
        @if !host_key.pending_key.is_empty() {
            div .row {
                (badge("changed", BadgeKind::Bad))
                span .small.muted { "connections refused until reviewed" }
            }
        } @else if host_key.key.is_empty() {
            span .muted { "not pinned yet — the first connection records one" }
        } @else {
            div .row {
                (badge("pinned", BadgeKind::Ok))
                span .mono.small { (host_key.fingerprint) }
            }
            div .small.muted { "trusted on first use, " (ago(host_key.pinned_at.as_ref())) }
        }
    }
}

/// The review-and-accept flow for a changed host key.
///
/// Deliberately not a one-click "accept": both fingerprints are shown, the
/// command to verify against the machine itself is spelled out, and the button
/// carries the fingerprint being accepted so what is accepted is what was
/// displayed.
fn host_key_change(target: &Target, host_key: &HostKey, csrf: &str) -> Markup {
    html! {
        div .card {
            (callout("bad", "This target's SSH host key has changed", html! {
                "Every connection to " (target.name) " is refused until this is resolved — \
                 deploys, logs, terminals and checks alike. A host that was rebuilt or \
                 reinstalled legitimately has a new key. A host that was not is something \
                 else answering for this address, and accepting the key would hand it an \
                 authentication attempt with this target's private key."
            }))

            dl .dl style="margin-top:12px" {
                dt { "Pinned" }
                dd {
                    @if host_key.fingerprint.is_empty() {
                        span .muted { "none" }
                    } @else {
                        div .mono.small { (host_key.fingerprint) }
                    }
                }
                dt { "Presented" }
                dd {
                    div .mono.small { (host_key.pending_fingerprint) }
                    div .small.muted { "first seen " (ago(host_key.pending_seen_at.as_ref())) }
                }
            }

            p .card-note {
                "Verify on the machine itself before accepting — over a console, or a \
                 channel that does not depend on this address being the right host:"
            }
            pre .mono.small { "ssh-keygen -lf /etc/ssh/ssh_host_ed25519_key.pub" }
            p .card-note {
                "That must print the presented fingerprint above. "
                code { "ssh-keyscan" }
                " asks the same address nudo is being redirected away from, so it \
                 confirms nothing on its own."
            }

            form method="post" action=(format!("/targets/{}/host-key/accept", target.id)) {
                (csrf_input(csrf))
                // The fingerprint travels with the acceptance, so a key that
                // changes again between this page rendering and the click is
                // refused rather than accepted unseen.
                input type="hidden" name="fingerprint" value=(host_key.pending_fingerprint);
                @if target.latency_critical {
                    input type="hidden" name="allow_latency_critical" value="1";
                }
                div .form-actions {
                    button .btn.danger type="submit"
                        onclick=(format!(
                            "return confirm('Accept {} as the host key for {}? Do this only if you have confirmed that fingerprint on the machine itself.')",
                            host_key.pending_fingerprint, target.name
                        )) {
                        "Accept this key"
                    }
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
                        a href="/secrets#ssh-key" { "Add a key" }
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
