use super::*;

// ---------------------------------------------------------------------------
// Services
// ---------------------------------------------------------------------------

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

/// A single service: live state, controls, configuration, history, releases.
pub fn service_detail(
    service: &Service,
    target: &Target,
    status: &UnitStatus,
    releases: &[Release],
    deployments: &[Deployment],
    csrf: &str,
) -> Markup {
    let unit = service.unit.clone().unwrap_or_default();
    let running = status.active_state == "active";
    let hot = target.latency_critical;

    html! {
        (topbar(&service.name, Some(&format!("{} · {}", target.name, unit.unit_name)), html! {
            div .row { (unit_badge(status)) @if hot { (latency_critical_badge()) } }

            form method="post" action=(format!("/services/{}/deploy", service.id)) {
                (csrf_input(csrf))
                button .btn.primary type="submit"
                    onclick=(deploy_confirm(hot)) { "Deploy" }
            }
            // One endpoint takes every unit action, with the verb in a hidden
            // field — so start, stop, restart, reload, enable and disable all go
            // through the same handler and the same guardrail.
            form method="post" action=(format!("/services/{}/action", service.id)) {
                (csrf_input(csrf))
                input type="hidden" name="action" value="restart";
                // Restarting drops in-flight work, so it confirms even though
                // the end state is the same as the current one.
                button .btn type="submit"
                    onclick=(format!("return confirm('Restart {}? The process will be killed and started again, dropping anything in flight.')", js_text(&unit.unit_name))) {
                    "Restart"
                }
            }
            @if running {
                form method="post" action=(format!("/services/{}/action", service.id)) {
                    (csrf_input(csrf))
                    input type="hidden" name="action" value="stop";
                    button .btn.danger type="submit"
                        onclick=(format!("return confirm('Stop {}? The service will be down until it is started again.')", js_text(&unit.unit_name))) {
                        "Stop"
                    }
                }
            } @else {
                form method="post" action=(format!("/services/{}/action", service.id)) {
                    (csrf_input(csrf))
                    input type="hidden" name="action" value="start";
                    button .btn type="submit" { "Start" }
                }
            }
        }))
        (tabs(&[
            ("Overview", &format!("/services/{}", service.id), true),
            ("Logs", &format!("/services/{}/logs", service.id), false),
            ("Unit file", &format!("/services/{}/unit", service.id), false),
            ("Edit", &format!("/services/{}/edit", service.id), false),
        ]))
        div .content {
            @if hot {
                (callout("bad", "Runs on a latency-critical host", html! {
                    "Deploys and unit actions against " (target.name) " require an \
                     explicit override and are never performed unattended."
                }))
            }
            @if status.active_state == "failed" {
                (callout("bad", "Unit is failed", html! {
                    "systemd reports " span .mono { (status.active_state) "/" (status.sub_state) }
                    ". The logs tab has the last output before it died."
                }))
            }

            // Latency knobs first when set: they are the reason a service is on
            // nudo instead of in a container, and a silently dropped affinity is
            // exactly the bug that is hardest to notice.
            @if has_latency_knobs(service) {
                div .card {
                    div .row {
                        h2 { "Latency configuration" }
                        (badge("pinned", BadgeKind::Hot))
                    }
                    dl .dl style="margin-top:12px" {
                        @if !unit.cpu_affinity.is_empty() {
                            dt { "CPUAffinity" } dd .mono { (unit.cpu_affinity) }
                        }
                        @if !unit.nice.is_empty() {
                            dt { "Nice" } dd .mono { (unit.nice) }
                        }
                        @if !unit.io_scheduling_class.is_empty() {
                            dt { "IOSchedulingClass" } dd .mono { (unit.io_scheduling_class) }
                        }
                    }
                }
            }

            div .card {
                h2 { "Summary" }
                dl .dl style="margin-top:12px" {
                    dt { "Target" }
                    dd { a href=(format!("/targets/{}", target.id)) { (target.name) }
                         span .sep { " · " } span .mono.small { (address(target)) } }
                    dt { "Unit" }             dd .mono { (or_dash(&unit.unit_name)) }
                    dt { "Source" }           dd { (artifact_detail(service)) }
                    dt { "Release root" }     dd .mono { (or_dash(&service.release_root)) }
                    dt { "Current release" }  dd .mono {
                        @if service.current_release_id.is_empty() {
                            span .muted { "never deployed" }
                        } @else {
                            (service.current_release_id)
                        }
                    }
                    dt { "Keep releases" }    dd { (service.keep_releases) }
                    dt { "Health check" }     dd { (health_check_summary(service)) }
                    dt { "Restart" }          dd .mono {
                        (or_dash(&unit.restart))
                        @if unit.restart_sec > 0 { " after " (unit.restart_sec) "s" }
                    }
                    dt { "Runs as" }          dd .mono {
                        (or_dash(&unit.user))
                        @if !unit.group.is_empty() { ":" (unit.group) }
                    }
                    dt { "Working dir" }      dd .mono { (or_dash(&unit.working_directory)) }
                    dt { "Env" }              dd {
                        @if service.env.is_empty() {
                            span .muted { "-" }
                        } @else {
                            span .mono.small { (env_line(&service.env)) }
                        }
                    }
                    // Ids only. The values are resolved on the target at deploy
                    // time and never transit the API, let alone this page.
                    dt { "Secrets" }          dd {
                        @if service.secret_ids.is_empty() {
                            span .muted { "-" }
                        } @else {
                            span .mono.small { (service.secret_ids.join(", ")) }
                            span .sep { " · " }
                            span .small.faint { "ids only; values are written on the target" }
                        }
                    }
                    dt { "PID" }              dd .mono { @if status.pid > 0 { (status.pid) } @else { "-" } }
                    dt { "Memory" }           dd { @if status.memory_bytes > 0 { (bytes(status.memory_bytes)) } @else { "-" } }
                    dt { "Restarts" }         dd { (status.restart_count) }
                    dt { "Enabled at boot" }  dd { @if status.enabled { "yes" } @else { "no" } }
                    dt { "Active since" }     dd { (ago(status.since.as_ref())) }
                }
            }

            div .card.pad-0 {
                div .card-head {
                    h2 { "Recent deployments" }
                    div .actions {
                        a .btn.small href=(format!("/deployments?service={}", service.id)) { "All" }
                    }
                }
                @if deployments.is_empty() {
                    div .card-body { p .muted { "This service has never been deployed." } }
                } @else {
                    (deployments_table(deployments, std::slice::from_ref(service)))
                }
            }

            div .card.pad-0 {
                div .card-head {
                    h2 { "Releases" }
                    p .card-note { "Kept for rollback: " (service.keep_releases) }
                }
                @if releases.is_empty() {
                    div .card-body { p .muted { "No releases yet." } }
                } @else {
                    div .table-scroll {
                        table {
                            thead {
                                tr {
                                    th { "Release" }
                                    th { "Ref" }
                                    th { "SHA" }
                                    th { "Size" }
                                    th { "Created" }
                                    th {}
                                }
                            }
                            tbody {
                                @for release in releases {
                                    @let current = release.id == service.current_release_id;
                                    tr {
                                        td .mono.small {
                                            (release.id)
                                            @if current { " " (badge("current", BadgeKind::Info)) }
                                        }
                                        td .small { (or_dash(&release.git_ref)) }
                                        td .mono.small { (short_sha(&release.git_sha)) }
                                        td .num.small { (bytes(release.artifact_bytes)) }
                                        td .nowrap.small.muted { (ago(release.created_at.as_ref())) }
                                        td {
                                            @if !current {
                                                form method="post" action=(format!("/services/{}/rollback", service.id)) {
                                                    (csrf_input(csrf))
                                                    input type="hidden" name="release_id" value=(release.id);
                                                    button .btn.small.danger type="submit"
                                                        onclick=(format!("return confirm('Roll {} back to release {}? The current symlink is swapped and the unit restarted.')", js_text(&service.name), js_text(&release.id))) {
                                                        "Rollback"
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
            }
        }
    }
}

/// The confirm text for a deploy. On a latency-critical host it names the
/// override, because that is the consequence the operator is agreeing to.
fn deploy_confirm(latency_critical: bool) -> String {
    if latency_critical {
        "return confirm('Deploy to a LATENCY-CRITICAL host? This restarts the unit on a machine where that has a cost. Continue only if you are watching it.')"
            .to_string()
    } else {
        "return confirm('Deploy the current source to this service? The unit will be restarted.')"
            .to_string()
    }
}

/// Escapes a value for interpolation into a single-quoted JavaScript string
/// inside an attribute.
///
/// Maud escapes the attribute for HTML, which neutralises `"`, `<` and `&`, but
/// not the `'` that would close the JS literal. Both layers are needed.
pub(super) fn js_text(value: &str) -> String {
    value.replace('\\', "\\\\").replace('\'', "\\'")
}

/// Where the binary comes from, in full.
fn artifact_detail(service: &Service) -> Markup {
    use nudo_proto::artifact_source::Kind;
    html! {
        @match service.artifact.as_ref().and_then(|a| a.kind.as_ref()) {
            Some(Kind::Url(url)) => span .mono.small { (url) },
            Some(Kind::Git(git)) => {
                span .mono.small { (git.repo) "@" (or_dash(&git.branch)) }
                @if !git.build_command.is_empty() {
                    div .small.muted { "build: " span .mono { (git.build_command) } }
                }
                @if !git.artifact_path.is_empty() {
                    div .small.muted { "artifact: " span .mono { (git.artifact_path) } }
                }
                @if git.auto_deploy_on_push {
                    div { (badge("auto-deploy on push", BadgeKind::Info)) }
                }
            },
            _ => span .muted { "pushed by the CLI" },
        }
    }
}

/// The health check in one line. Which check runs decides whether a bad deploy
/// rolls back, so "systemd only" is stated rather than implied.
fn health_check_summary(service: &Service) -> Markup {
    use nudo_proto::health_check::Kind;
    let Some(check) = service.health_check.as_ref() else {
        return html! { span .muted { "none — a deploy is never rolled back automatically" } };
    };

    html! {
        @match check.kind.as_ref() {
            Some(Kind::HttpUrl(url)) => { "GET " span .mono.small { (url) } },
            Some(Kind::Command(command)) => { "command " span .mono.small { (command) } },
            Some(Kind::SystemdActive(_)) => { "systemctl is-active only" },
            None => span .muted { "none" },
        }
        @if check.timeout_seconds > 0 || check.retries > 0 || check.initial_delay_seconds > 0 {
            span .sep { " · " }
            span .small.muted {
                (check.timeout_seconds) "s timeout, "
                (check.retries) " retries, "
                (check.initial_delay_seconds) "s initial delay"
            }
        }
    }
}

/// Non-secret env as `K=V` pairs in a stable order.
pub(super) fn env_line(env: &HashMap<String, String>) -> String {
    let mut pairs: Vec<String> = env.iter().map(|(k, v)| format!("{k}={v}")).collect();
    pairs.sort();
    pairs.join(" ")
}

/// A preview of the systemd unit nudo would write.
pub fn service_unit(service: &Service, unit_file: &str) -> Markup {
    html! {
        (topbar(&service.name, Some("Rendered systemd unit"), html! {
            a .btn href=(format!("/services/{}", service.id)) { "Back to service" }
        }))
        (tabs(&[
            ("Overview", &format!("/services/{}", service.id), false),
            ("Logs", &format!("/services/{}/logs", service.id), false),
            ("Unit file", &format!("/services/{}/unit", service.id), true),
            ("Edit", &format!("/services/{}/edit", service.id), false),
        ]))
        div .content {
            (callout("info", "This is a preview", html! {
                "Rendered from the current configuration, not read from the target. \
                 It is written to "
                span .mono {
                    "/etc/systemd/system/"
                    (service.unit.as_ref().map(|u| u.unit_name.clone()).unwrap_or_default())
                }
                " on the next deploy."
            }))
            div .card {
                pre .unit { (unit_file) }
            }
        }
    }
}

/// Create/edit form for a service: artifact, systemd unit, health check.
pub fn service_form(
    existing: Option<&Service>,
    targets: &[Target],
    sources: &[Source],
    secrets: &[Secret],
    csrf: &str,
) -> Markup {
    use nudo_proto::artifact_source::Kind as ArtifactKind;
    use nudo_proto::health_check::Kind as CheckKind;

    let editing = existing.is_some();
    let action = match existing {
        Some(s) => format!("/services/{}", s.id),
        None => "/services".to_string(),
    };
    let unit = existing.and_then(|s| s.unit.clone()).unwrap_or_default();
    let check = existing.and_then(|s| s.health_check.clone());
    let artifact = existing
        .and_then(|s| s.artifact.as_ref())
        .and_then(|a| a.kind.as_ref());
    let git = match artifact {
        Some(ArtifactKind::Git(git)) => Some(git.clone()),
        _ => None,
    };
    let artifact_url = match artifact {
        Some(ArtifactKind::Url(url)) => url.clone(),
        _ => String::new(),
    };
    let artifact_kind = match artifact {
        Some(ArtifactKind::Url(_)) => "url",
        Some(ArtifactKind::Git(_)) => "git",
        _ => "upload",
    };
    let check_kind = match check.as_ref().and_then(|c| c.kind.as_ref()) {
        Some(CheckKind::HttpUrl(_)) => "http",
        Some(CheckKind::Command(_)) => "command",
        Some(CheckKind::SystemdActive(_)) => "systemd",
        None => "none",
    };
    let check_http = match check.as_ref().and_then(|c| c.kind.as_ref()) {
        Some(CheckKind::HttpUrl(url)) => url.clone(),
        _ => String::new(),
    };
    let check_command = match check.as_ref().and_then(|c| c.kind.as_ref()) {
        Some(CheckKind::Command(command)) => command.clone(),
        _ => String::new(),
    };

    html! {
        (topbar(
            if editing { "Edit service" } else { "Add service" },
            Some("One systemd unit on one target"),
            html! { a .btn href="/services" { "Cancel" } },
        ))
        div .content {
            form method="post" action=(action) {
                (csrf_input(csrf))

                div .card {
                    h2 { "Identity" }
                    div .fields style="margin-top:12px" {
                        div .field {
                            label for="name" { "Name" }
                            input type="text" id="name" name="name" required
                                placeholder="hft-bot"
                                value=(existing.map(|s| s.name.as_str()).unwrap_or_default());
                        }
                        div .field {
                            label for="target_id" { "Target" }
                            select id="target_id" name="target_id" required {
                                option value="" { "Select a target…" }
                                @for target in targets {
                                    option value=(target.id)
                                        selected[existing.is_some_and(|s| s.target_id == target.id)] {
                                        (target.name)
                                        @if target.latency_critical { " (latency-critical)" }
                                    }
                                }
                            }
                        }
                        div .field {
                            label for="release_root" { "Release root" }
                            input type="text" id="release_root" name="release_root"
                                placeholder="/opt/hft-bot"
                                value=(existing.map(|s| s.release_root.as_str()).unwrap_or_default());
                            span .hint { code { "releases/" } " and the " code { "current" } " symlink live here." }
                        }
                        div .field {
                            label for="keep_releases" { "Keep releases" }
                            input type="number" id="keep_releases" name="keep_releases" min="1" max="50"
                                value=(existing.map(|s| s.keep_releases).filter(|k| *k > 0).unwrap_or(5));
                            span .hint { "How many old releases stay on disk for rollback." }
                        }
                    }
                }

                div .card {
                    h2 { "Artifact" }
                    p .card-note { "Where the binary comes from." }
                    div .field style="margin-top:12px" {
                        label for="artifact_kind" { "Source" }
                        select id="artifact_kind" name="artifact_kind" {
                            option value="upload" selected[artifact_kind == "upload"] { "Pushed by the CLI" }
                            option value="url" selected[artifact_kind == "url"] { "Prebuilt binary at a URL" }
                            option value="git" selected[artifact_kind == "git"] { "Built from a git source" }
                        }
                    }
                    div .field style="margin-top:12px" {
                        label for="artifact_url" { "Artifact URL" }
                        input type="text" id="artifact_url" name="artifact_url"
                            placeholder="https://github.com/owner/repo/releases/download/v1/bot"
                            value=(artifact_url);
                    }
                    div .fields style="margin-top:12px" {
                        div .field {
                            label for="source_id" { "Git source" }
                            select id="source_id" name="source_id" {
                                option value="" { "None" }
                                @for source in sources {
                                    option value=(source.id)
                                        selected[git.as_ref().is_some_and(|g| g.source_id == source.id)] {
                                        (source.name)
                                        " (" (source::Kind::try_from(source.kind).unwrap_or(source::Kind::Unspecified).as_str()) ")"
                                    }
                                }
                            }
                            @if sources.is_empty() {
                                span .hint { a href="/sources" { "Connect a GitHub App" } " to build from a repo." }
                            }
                        }
                        div .field {
                            label for="repo" { "Repository" }
                            input type="text" id="repo" name="repo" placeholder="owner/name"
                                value=(git.as_ref().map(|g| g.repo.clone()).unwrap_or_default());
                        }
                        div .field {
                            label for="branch" { "Branch" }
                            input type="text" id="branch" name="branch" placeholder="main"
                                value=(git.as_ref().map(|g| g.branch.clone()).unwrap_or_default());
                        }
                        div .field {
                            label for="build_command" { "Build command" }
                            input type="text" id="build_command" name="build_command"
                                placeholder="cargo build --release"
                                value=(git.as_ref().map(|g| g.build_command.clone()).unwrap_or_default());
                        }
                        div .field {
                            label for="artifact_path" { "Artifact path" }
                            input type="text" id="artifact_path" name="artifact_path"
                                placeholder="target/release/bot"
                                value=(git.as_ref().map(|g| g.artifact_path.clone()).unwrap_or_default());
                        }
                    }
                    div .check style="margin-top:12px" {
                        input type="checkbox" id="auto_deploy_on_push" name="auto_deploy_on_push" value="1"
                            checked[git.as_ref().is_some_and(|g| g.auto_deploy_on_push)];
                        label for="auto_deploy_on_push" {
                            "Deploy automatically on push"
                            div .hint {
                                "A push to the branch deploys without anyone watching. \
                                 Refused server-side for latency-critical targets."
                            }
                        }
                    }
                }

                div .card {
                    h2 { "systemd unit" }
                    div .fields style="margin-top:12px" {
                        div .field {
                            label for="unit_name" { "Unit name" }
                            input type="text" id="unit_name" name="unit_name"
                                placeholder="hft-bot.service" value=(unit.unit_name);
                        }
                        div .field {
                            label for="description" { "Description" }
                            input type="text" id="description" name="description"
                                value=(unit.description);
                        }
                        div .field {
                            label for="exec_args" { "Exec args" }
                            input type="text" id="exec_args" name="exec_args"
                                placeholder="--config /etc/bot.toml" value=(unit.exec_args);
                            span .hint { "Appended to the binary in " code { "ExecStart" } "." }
                        }
                        div .field {
                            label for="working_directory" { "Working directory" }
                            input type="text" id="working_directory" name="working_directory"
                                value=(unit.working_directory);
                        }
                        div .field {
                            label for="unit_user" { "User" }
                            input type="text" id="unit_user" name="unit_user" value=(unit.user);
                        }
                        div .field {
                            label for="unit_group" { "Group" }
                            input type="text" id="unit_group" name="unit_group" value=(unit.group);
                        }
                        div .field {
                            label for="restart" { "Restart" }
                            select id="restart" name="restart" {
                                @for option in ["always", "on-failure", "no"] {
                                    option value=(option) selected[unit.restart == option] { (option) }
                                }
                            }
                        }
                        div .field {
                            label for="restart_sec" { "RestartSec" }
                            input type="number" id="restart_sec" name="restart_sec" min="0"
                                value=(unit.restart_sec);
                        }
                        div .field {
                            label for="after" { "After" }
                            input type="text" id="after" name="after"
                                placeholder="network-online.target" value=(unit.after.join(","));
                            span .hint { "Comma-separated units this one is ordered after." }
                        }
                    }
                }

                // Its own card, not a row in the unit fields: these are the
                // knobs the whole tool exists for, and burying them invites
                // someone to leave them empty by accident.
                div .card {
                    div .row {
                        h2 { "Latency knobs" }
                        (badge("hot path", BadgeKind::Hot))
                    }
                    p .card-note {
                        "Written verbatim into the unit. Leave blank on hosts where \
                         the scheduler's defaults are fine."
                    }
                    div .fields style="margin-top:12px" {
                        div .field {
                            label for="cpu_affinity" { "CPUAffinity" }
                            input type="text" id="cpu_affinity" name="cpu_affinity"
                                placeholder="2-5" value=(unit.cpu_affinity);
                            span .hint { "Pins the process to these cores." }
                        }
                        div .field {
                            label for="nice" { "Nice" }
                            input type="text" id="nice" name="nice"
                                placeholder="-10" value=(unit.nice);
                        }
                        div .field {
                            label for="io_scheduling_class" { "IOSchedulingClass" }
                            select id="io_scheduling_class" name="io_scheduling_class" {
                                option value="" selected[unit.io_scheduling_class.is_empty()] { "default" }
                                @for option in ["realtime", "best-effort", "idle"] {
                                    option value=(option) selected[unit.io_scheduling_class == option] { (option) }
                                }
                            }
                        }
                    }
                    div .field style="margin-top:12px" {
                        label for="extra_directives" { "Extra directives" }
                        textarea id="extra_directives" name="extra_directives"
                            placeholder="LimitMEMLOCK=infinity\nLimitNOFILE=65535" {
                            (directives_text(&unit.extra_directives))
                        }
                        span .hint {
                            "One " code { "Key=Value" } " per line, copied into the "
                            code { "[Service]" } " section without validation."
                        }
                    }
                }

                div .card {
                    h2 { "Health check" }
                    p .card-note {
                        "Decides whether a deploy stands. Without one, a broken \
                         release is never rolled back automatically."
                    }
                    div .field style="margin-top:12px" {
                        label for="check_kind" { "Kind" }
                        select id="check_kind" name="check_kind" {
                            option value="none" selected[check_kind == "none"] { "None" }
                            option value="systemd" selected[check_kind == "systemd"] { "systemctl is-active" }
                            option value="http" selected[check_kind == "http"] { "HTTP GET, expect 2xx" }
                            option value="command" selected[check_kind == "command"] { "Command, expect exit 0" }
                        }
                    }
                    div .fields style="margin-top:12px" {
                        div .field {
                            label for="check_http_url" { "HTTP URL" }
                            input type="text" id="check_http_url" name="check_http_url"
                                placeholder="http://127.0.0.1:9000/healthz" value=(check_http);
                        }
                        div .field {
                            label for="check_command" { "Command" }
                            input type="text" id="check_command" name="check_command"
                                placeholder="/opt/bot/current/bot --selftest" value=(check_command);
                        }
                        div .field {
                            label for="check_timeout" { "Timeout (s)" }
                            input type="number" id="check_timeout" name="check_timeout" min="1"
                                value=(check.as_ref().map(|c| c.timeout_seconds).filter(|v| *v > 0).unwrap_or(5));
                        }
                        div .field {
                            label for="check_retries" { "Retries" }
                            input type="number" id="check_retries" name="check_retries" min="0"
                                value=(check.as_ref().map(|c| c.retries).unwrap_or(3));
                        }
                        div .field {
                            label for="check_initial_delay" { "Initial delay (s)" }
                            input type="number" id="check_initial_delay" name="check_initial_delay" min="0"
                                value=(check.as_ref().map(|c| c.initial_delay_seconds).unwrap_or(2));
                        }
                    }
                }

                div .card {
                    h2 { "Environment" }
                    div .field style="margin-top:12px" {
                        label for="env" { "Non-secret variables" }
                        textarea id="env" name="env" placeholder="RUST_LOG=info\nEXCHANGE=binance" {
                            (directives_text(&existing.map(|s| s.env.clone()).unwrap_or_default()))
                        }
                        span .hint { "One " code { "KEY=value" } " per line. Anything sensitive belongs in a secret." }
                    }
                    div .field style="margin-top:14px" {
                        label { "Secrets" }
                        // Checkboxes over ids. A value cannot be entered here,
                        // and none is displayed: secret values are write-only
                        // and are resolved into an EnvironmentFile on the target.
                        @if secrets.is_empty() {
                            p .hint { "No secrets stored yet. " a href="/secrets" { "Add one" } "." }
                        } @else {
                            @for secret in secrets {
                                div .check {
                                    input type="checkbox" id=(format!("secret_{}", secret.id))
                                        name="secret_ids" value=(secret.id)
                                        checked[existing.is_some_and(|s| s.secret_ids.contains(&secret.id))];
                                    label for=(format!("secret_{}", secret.id)) {
                                        span .mono { (secret.name) }
                                        span .sep { " · " }
                                        span .small.muted { (scope_label(secret)) }
                                    }
                                }
                            }
                        }
                        span .hint {
                            "Selected values are written to the unit's EnvironmentFile on \
                             the target at deploy time. They are never sent to this page."
                        }
                    }
                }

                div .form-actions {
                    button .btn.primary type="submit" {
                        @if editing { "Save service" } @else { "Create service" }
                    }
                    a .btn href="/services" { "Cancel" }
                }
            }

            @if editing {
                @let id = existing.map(|s| s.id.as_str()).unwrap_or_default();
                form .card method="post" action=(format!("/services/{id}/delete")) {
                    (csrf_input(csrf))
                    h2 { "Delete service" }
                    p .card-note { "Choose what happens on the target as well as in nudo." }
                    div .check style="margin-top:10px" {
                        input type="checkbox" id="stop_and_disable_unit" name="stop_and_disable_unit" value="1" checked;
                        label for="stop_and_disable_unit" { "Stop and disable the unit on the target" }
                    }
                    div .check style="margin-top:8px" {
                        input type="checkbox" id="remove_release_dir" name="remove_release_dir" value="1";
                        label for="remove_release_dir" {
                            "Delete the release directory"
                            div .hint { "Removes every release, so rollback is no longer possible." }
                        }
                    }
                    div .form-actions {
                        button .btn.danger type="submit"
                            onclick="return confirm('Delete this service? Depending on the options above this stops the unit on the target and may delete every release, which cannot be undone.')" {
                            "Delete service"
                        }
                    }
                }
            }
        }
    }
}

/// A `key=value` map as newline-separated text for a textarea, in a stable order.
pub(super) fn directives_text(map: &HashMap<String, String>) -> String {
    let mut lines: Vec<String> = map.iter().map(|(k, v)| format!("{k}={v}")).collect();
    lines.sort();
    lines.join("\n")
}
