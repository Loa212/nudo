use super::*;

use nudo_proto::LOCAL_BUILD_HOST_ID;

/// The build hosts list, with the instance default selector above it.
pub fn build_hosts_list(hosts: &[BuildHost], default_id: &str, csrf: &str) -> Markup {
    html! {
        (topbar("Build hosts", Some("Machines that build, when the control plane does not"), html! {
            a .btn.primary href="/build-hosts/new" { "Add build host" }
        }))
        div .content {
            (build_default_card(hosts, default_id, csrf))

            div .card.pad-0 {
                @if hosts.is_empty() {
                    (empty_state(
                        "No build hosts",
                        "Builds run on the control plane. Add a build host to run them \
                         somewhere else — a bigger box, or one with a toolchain the control \
                         plane does not have.",
                        Some(("Add build host", "/build-hosts/new")),
                    ))
                } @else {
                    div .table-scroll {
                        table {
                            thead {
                                tr {
                                    th { "Name" }
                                    th { "Address" }
                                    th { "Status" }
                                    th { "Workspace" }
                                    th { "Last seen" }
                                }
                            }
                            tbody {
                                @for host in hosts {
                                    tr {
                                        td {
                                            a href=(format!("/build-hosts/{}", host.id)) { (host.name) }
                                            @if host.id == default_id {
                                                " "
                                                span .badge.info title="Used by services that do not name a build host" {
                                                    span .dot {}
                                                    "default"
                                                }
                                            }
                                        }
                                        td .mono.nowrap { (build_host_address(host)) }
                                        td { div .row { (build_host_badges(host)) } }
                                        td .mono.small { (or_dash(&host.workspace_root)) }
                                        td .small.muted { (ago(host.last_seen_at.as_ref())) }
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

/// Where builds run when a service does not say.
fn build_default_card(hosts: &[BuildHost], default_id: &str, csrf: &str) -> Markup {
    let on_control_plane = default_id.is_empty() || default_id == LOCAL_BUILD_HOST_ID;

    html! {
        div .card {
            div .row { h2 { "Default build location" } }
            p .card-note {
                "Where a build runs when its service does not name a build host. "
                "A service that names one always overrides this."
            }
            form method="post" action="/build-hosts/default" style="margin-top:12px" {
                (csrf_input(csrf))
                div .row {
                    select name="build_host_id" {
                        option value="" selected[on_control_plane] {
                            "The control plane"
                        }
                        @for host in hosts {
                            option value=(host.id) selected[host.id == default_id] {
                                (host.name)
                                @if host.latency_critical { " (latency-critical)" }
                            }
                        }
                    }
                    button .btn type="submit" { "Save" }
                }
            }
        }
    }
}

/// One build host, its checks, and what depends on it.
pub fn build_host_detail(
    host: &BuildHost,
    services: &[Service],
    default_id: &str,
    checks: Option<&CheckBuildHostResponse>,
    csrf: &str,
) -> Markup {
    let host_key = host.host_key.clone().unwrap_or_default();

    html! {
        (topbar(&host.name, Some(&build_host_address(host)), html! {
            form method="post" action=(format!("/build-hosts/{}/check", host.id))
                style="display:inline" {
                (csrf_input(csrf))
                button .btn type="submit" { "Run checks" }
            }
        }))
        div .content {
            // First: an unreviewed key change means nothing builds here at all.
            @if !host_key.pending_key.is_empty() {
                (build_host_key_change(host, &host_key, csrf))
            }

            @if host.latency_critical {
                (callout("bad", "Latency-critical host", html! {
                    "A build here competes with whatever else runs on this box for CPU, \
                     cache and memory bandwidth. Expect jitter on anything sensitive while \
                     a build is in flight. Mutating this host requires an explicit override."
                }))
            }

            // Said on every build host, not only shared ones: the assumption
            // that registering one buys isolation is exactly the assumption
            // worth correcting before someone relies on it.
            (callout("warn", "A build host is not a sandbox", html! {
                "Build commands run here as "
                span .mono { (or_dash(&host.user)) }
                " with no isolation between builds. If builds should not see each \
                 other, run this host as a one-shot container, an ephemeral VM, or a \
                 fresh instance per build — nudo does not do it for you."
            }))

            div .card {
                div .row {
                    h2 { "Connection" }
                    (build_host_badges(host))
                }
                dl .dl style="margin-top:12px" {
                    dt { "Host" }        dd .mono { (host.host) }
                    dt { "Port" }        dd .mono { (host.port) }
                    dt { "User" }        dd .mono { (or_dash(&host.user)) }
                    dt { "Workspace" }   dd .mono { (or_dash(&host.workspace_root)) }
                    dt { "SSH key" }     dd .mono { (or_dash(&host.ssh_key_id)) }
                    dt { "Host key" }
                    dd .mono.small {
                        @if host_key.fingerprint.is_empty() {
                            "not pinned yet — the first successful connection records one"
                        } @else {
                            (host_key.fingerprint)
                        }
                    }
                    dt { "Default" }
                    dd {
                        @if host.id == default_id {
                            "Yes — services that do not name a build host build here"
                        } @else {
                            "No"
                        }
                    }
                }
            }

            @if let Some(checks) = checks {
                (preflight_card(checks.ok, &probes(&checks.checks), &checks.warnings))
            }

            div .card.pad-0 {
                div .card-head { h2 { "Services building here" } }
                @if services.is_empty() {
                    p .card-note style="padding:0 16px 16px" {
                        "No service names this build host. Services falling back to the \
                         instance default are not listed here, since they would move with \
                         the default rather than break."
                    }
                } @else {
                    div .table-scroll {
                        table {
                            thead { tr { th { "Service" } th { "Build command" } } }
                            tbody {
                                @for service in services {
                                    tr {
                                        td { a href=(format!("/services/{}", service.id)) { (service.name) } }
                                        td .mono.small { (truncate(&build_command_of(service), 48)) }
                                    }
                                }
                            }
                        }
                    }
                    p .card-note style="padding:0 16px 16px" {
                        "Deleting this build host leaves these services pointing at it. \
                         Their next build fails with a message naming it, rather than \
                         silently moving somewhere else."
                    }
                }
            }

            form .card method="post" action=(format!("/build-hosts/{}/delete", host.id)) {
                (csrf_input(csrf))
                div .row {
                    h2 { "Delete" }
                }
                p .card-note {
                    "Removes this build host. Nothing on the machine itself is changed."
                }
                @if host.latency_critical {
                    input type="hidden" name="allow_latency_critical" value="1";
                }
                button .btn.danger type="submit"
                    onclick=(format!("return confirm('Delete build host {}?')", host.name)) {
                    "Delete build host"
                }
            }
        }
    }
}

/// The host-key review form.
fn build_host_key_change(host: &BuildHost, host_key: &HostKey, csrf: &str) -> Markup {
    html! {
        (callout("bad", "This host's SSH key has changed", html! {
            "Every connection is refused until this is resolved, so nothing builds here. \
             A rebuilt machine legitimately has a new key — but so does a different \
             machine answering for this address, and this host is handed repository \
             credentials."
        }))
        div .card {
            dl .dl {
                dt { "Pinned" }
                dd .mono.small { (or_dash(&host_key.fingerprint)) }
                dt { "Presented" }
                dd .mono.small { (host_key.pending_fingerprint) }
                dt { "First seen" }
                dd .small.muted { (ago(host_key.pending_seen_at.as_ref())) }
            }
            p .card-note style="margin-top:12px" {
                "Verify the fingerprint on the machine itself before accepting:"
            }
            pre .mono.small {
                (format!("ssh-keyscan -t ed25519 {} | ssh-keygen -lf -", host.host))
            }
            form method="post" action=(format!("/build-hosts/{}/host-key/accept", host.id)) {
                (csrf_input(csrf))
                // Round-trips so a key that changed again in between is refused
                // rather than accepted unseen.
                input type="hidden" name="fingerprint" value=(host_key.pending_fingerprint);
                @if host.latency_critical {
                    input type="hidden" name="allow_latency_critical" value="1";
                }
                button .btn.danger type="submit"
                    onclick=(format!(
                        "return confirm('Accept {} as the host key for {}?')",
                        host_key.pending_fingerprint, host.name
                    )) {
                    "Accept this key"
                }
            }
        }
    }
}

/// The check results, and any warnings alongside them.
/// Create and edit share a form, keyed on whether there is an existing host.
pub fn build_host_form(existing: Option<&BuildHost>, secrets: &[Secret], csrf: &str) -> Markup {
    let editing = existing.is_some();
    let action = match existing {
        Some(h) => format!("/build-hosts/{}", h.id),
        None => "/build-hosts".to_string(),
    };
    let title = if editing {
        "Edit build host"
    } else {
        "Add build host"
    };

    html! {
        (topbar(title, Some("A machine nudo can reach over ssh to run builds on"), html! {}))
        div .content {
            form .card method="post" action=(action) {
                (csrf_input(csrf))
                div .fields {
                    div .field {
                        label for="name" { "Name" }
                        input type="text" id="name" name="name" required
                            placeholder="builder-1"
                            value=(existing.map(|h| h.name.as_str()).unwrap_or_default());
                    }
                    div .field {
                        label for="host" { "Host" }
                        input type="text" id="host" name="host" required
                            placeholder="10.0.0.9"
                            value=(existing.map(|h| h.host.as_str()).unwrap_or_default());
                    }
                    div .field {
                        label for="port" { "SSH port" }
                        input type="number" id="port" name="port"
                            value=(existing.map(|h| h.port).filter(|p| *p > 0).unwrap_or(22));
                    }
                    div .field {
                        label for="user" { "SSH user" }
                        input type="text" id="user" name="user"
                            placeholder="root"
                            value=(existing.map(|h| h.user.as_str()).unwrap_or_default());
                    }
                    div .field {
                        label for="ssh_key_id" { "SSH key" }
                        select id="ssh_key_id" name="ssh_key_id" {
                            option value="" { "None" }
                            @for secret in secrets {
                                option value=(secret.id)
                                    selected[existing.is_some_and(|h| h.ssh_key_id == secret.id)] {
                                    (secret.name)
                                }
                            }
                        }
                        span .hint {
                            "Keys live in the secret store and are chosen by reference. "
                            a href="/secrets#ssh-key" { "Add a key" }
                            " if the one you need is not listed."
                        }
                    }
                    div .field {
                        label for="workspace_root" { "Workspace root" }
                        input type="text" id="workspace_root" name="workspace_root"
                            placeholder="/var/lib/nudo/builds"
                            value=(existing.map(|h| h.workspace_root.as_str()).unwrap_or_default());
                        span .hint {
                            "Absolute path. Each build gets a fresh directory underneath, \
                             removed when it finishes however it finishes."
                        }
                    }
                }

                div .field style="margin-top:14px" {
                    label for="labels" { "Labels" }
                    textarea id="labels" name="labels" rows="3"
                        placeholder="arch=arm64\npool=ci" {
                        (existing.map(labels_text).unwrap_or_default())
                    }
                    span .hint { "One key=value per line." }
                }

                div .field style="margin-top:14px" {
                    div .check {
                        input type="checkbox" id="latency_critical" name="latency_critical" value="1"
                            checked[existing.is_some_and(|h| h.latency_critical)];
                        label for="latency_critical" {
                            "Latency-critical"
                            div .hint {
                                "Tick this if something latency-sensitive also runs here. \
                                 Building on such a box is allowed — you may have exactly \
                                 one spare machine — but it will contend for CPU, cache and \
                                 memory bandwidth, and every surface will say so."
                            }
                        }
                    }
                }

                div .row style="margin-top:16px" {
                    button .btn.primary type="submit" {
                        @if editing { "Save" } @else { "Add build host" }
                    }
                    a .btn href="/build-hosts" { "Cancel" }
                }
            }
        }
    }
}

/// `user@host:port`, as the target pages render it.
fn build_host_address(host: &BuildHost) -> String {
    format!("{}@{}:{}", host.user, host.host, host.port)
}

/// Status, plus the contention flag when it is set.
fn build_host_badges(host: &BuildHost) -> Markup {
    html! {
        (build_host_badge(host.status))
        @if host.latency_critical { (latency_critical_badge()) }
    }
}

/// A build host's reachability.
fn build_host_badge(status: i32) -> Markup {
    match build_host::Status::try_from(status) {
        Ok(build_host::Status::Reachable) => badge("reachable", BadgeKind::Ok),
        Ok(build_host::Status::Unreachable) => badge("unreachable", BadgeKind::Bad),
        _ => badge("unknown", BadgeKind::Neutral),
    }
}

/// The labels textarea's contents, one `key=value` per line.
fn labels_text(host: &BuildHost) -> String {
    let mut pairs: Vec<_> = host.labels.iter().collect();
    // Sorted so the field does not reorder itself between renders.
    pairs.sort();
    pairs
        .into_iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// A service's build command, for the dependants table.
fn build_command_of(service: &Service) -> String {
    match service.artifact.as_ref().and_then(|a| a.kind.as_ref()) {
        Some(nudo_proto::artifact_source::Kind::Git(git)) => git.build_command.clone(),
        _ => String::new(),
    }
}
