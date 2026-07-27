//! Server-rendered HTML for the nudo dashboard.
//!
//! Presentation only. Every function here takes data that has already been
//! fetched and returns `maud::Markup`; nothing in this module talks to axum, to
//! gRPC, or to the filesystem. That split is what makes the whole dashboard
//! testable without a running control plane — the tests at the bottom render
//! real proto messages and assert on the HTML.
//!
//! Three rules hold throughout:
//!
//! * **No secret value ever reaches a template.** Secrets are write-only over
//!   the API, so the only way a value could appear here is by mistake. There is
//!   deliberately no parameter, no field and no element in this module that
//!   could hold one — only names, scopes and digest prefixes.
//! * **Every POST form carries a CSRF token.** The `csrf` parameter threaded
//!   through the form functions is not optional, so a new form cannot be added
//!   without one.
//! * **Maud escapes text by default.** The only `PreEscaped` in this module is
//!   the terminal's config JSON, and that is produced by `serde_json`, which
//!   escapes `<` and `/` such that it cannot break out of a `<script>` literal.
//!
//! Class names come from `assets/app.css` and nothing else; a class invented
//! here would render unstyled.

use std::collections::HashMap;

use maud::{DOCTYPE, Markup, PreEscaped, html};
use nudo_proto::{
    AuditEntry, CheckTargetResponse, Deployment, LogLine, Release, Secret, Service, Source, Target,
    UnitStatus, deployment, source, target,
};

// ---------------------------------------------------------------------------
// Formatting helpers
//
// These mirror `nudo-cli`'s `format` module. They are reimplemented rather than
// shared because the CLI is a binary crate the web tier does not depend on, and
// the conventions ("just now" rather than "in -3s", a digest prefix rather than
// a whole digest) matter more than the code.
// ---------------------------------------------------------------------------

/// A relative time. Absolute timestamps are the wrong default when the question
/// is nearly always "how long ago".
fn ago(timestamp: Option<&nudo_proto::Timestamp>) -> String {
    let Some(when) = timestamp.and_then(nudo_proto::from_timestamp) else {
        return "-".to_string();
    };
    ago_at(when)
}

/// The same relative time for a value that is already chrono-typed.
pub fn ago_at(when: chrono::DateTime<chrono::Utc>) -> String {
    let seconds = chrono::Utc::now().signed_duration_since(when).num_seconds();

    // A negative value means the peer's clock is ahead of ours. "in 3s" would
    // read as a bug, so clamp to "just now".
    if seconds < 60 {
        return "just now".to_string();
    }
    if seconds < 3600 {
        return format!("{}m ago", seconds / 60);
    }
    if seconds < 86_400 {
        return format!("{}h ago", seconds / 3600);
    }
    format!("{}d ago", seconds / 86_400)
}

/// How long a deployment took, or "running" while it is still going.
fn duration(
    started: Option<&nudo_proto::Timestamp>,
    finished: Option<&nudo_proto::Timestamp>,
) -> String {
    let Some(started) = started.and_then(nudo_proto::from_timestamp) else {
        return "-".to_string();
    };
    let Some(finished) = finished.and_then(nudo_proto::from_timestamp) else {
        return "running".to_string();
    };

    let seconds = finished.signed_duration_since(started).num_seconds().max(0);
    if seconds < 60 {
        format!("{seconds}s")
    } else {
        format!("{}m{}s", seconds / 60, seconds % 60)
    }
}

/// A byte count in the largest unit that keeps it readable.
fn bytes(count: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    if count == 0 {
        return "0 B".to_string();
    }

    let mut value = count as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }

    if unit == 0 {
        format!("{count} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

/// Shortens a string for a table cell, collapsing newlines.
///
/// A multi-line build error pasted verbatim into a `<td>` makes the row taller
/// than the viewport, so table cells always go through this.
fn truncate(value: &str, limit: usize) -> String {
    let single_line = value.replace('\n', " ").trim().to_string();
    if single_line.is_empty() {
        return "-".to_string();
    }
    if single_line.chars().count() <= limit {
        return single_line;
    }
    let kept: String = single_line.chars().take(limit.saturating_sub(1)).collect();
    format!("{kept}…")
}

/// A one-phrase description of where a service's binary comes from.
fn artifact_summary(service: &Service) -> String {
    use nudo_proto::artifact_source::Kind;
    match service.artifact.as_ref().and_then(|a| a.kind.as_ref()) {
        Some(Kind::Url(url)) if !url.is_empty() => "url".to_string(),
        Some(Kind::Git(git)) => {
            if git.branch.is_empty() {
                format!("git:{}", git.repo)
            } else {
                format!("git:{}@{}", git.repo, git.branch)
            }
        }
        _ => "upload".to_string(),
    }
}

/// Describes a secret's scope. Metadata only — see the module note on secrets.
fn scope_label(secret: &Secret) -> String {
    match (
        secret.scope_target_id.is_empty(),
        secret.scope_service_id.is_empty(),
    ) {
        (true, true) => "global".to_string(),
        (false, true) => format!("target {}", secret.scope_target_id),
        (true, false) => format!("service {}", secret.scope_service_id),
        // The proto permits both; the narrower scope is the one that decides.
        (false, false) => format!("service {}", secret.scope_service_id),
    }
}

/// The first twelve characters of a digest — enough to tell whether two
/// environments hold the same secret, useless for recovering the value.
fn digest_prefix(digest: &str) -> String {
    if digest.is_empty() {
        return "-".to_string();
    }
    digest.chars().take(12).collect()
}

/// A short git sha, as everyone actually reads them.
fn short_sha(sha: &str) -> String {
    if sha.is_empty() {
        return "-".to_string();
    }
    sha.chars().take(8).collect()
}

/// `user@host:port`, the address as an operator would type it into ssh.
fn address(target: &Target) -> String {
    format!("{}@{}:{}", target.user, target.host, target.port)
}

/// A dash rather than an empty cell, so a missing value is visibly missing.
fn or_dash(value: &str) -> String {
    if value.trim().is_empty() {
        "-".to_string()
    } else {
        value.to_string()
    }
}

/// Looks a name up by id for cross-referencing tables, falling back to the raw
/// id when the referenced row is not in the slice we were handed.
fn name_of<'a>(id: &'a str, pairs: impl Iterator<Item = (&'a str, &'a str)>) -> &'a str {
    for (candidate, name) in pairs {
        if candidate == id {
            return name;
        }
    }
    id
}

fn target_name<'a>(id: &'a str, targets: &'a [Target]) -> &'a str {
    name_of(id, targets.iter().map(|t| (t.id.as_str(), t.name.as_str())))
}

fn service_name<'a>(id: &'a str, services: &'a [Service]) -> &'a str {
    name_of(
        id,
        services.iter().map(|s| (s.id.as_str(), s.name.as_str())),
    )
}

/// Whether a service has any latency knob set. When it does, the knobs are
/// shown at the top of the detail page instead of buried in the unit fields —
/// they are the reason this tool exists instead of a container runtime.
fn has_latency_knobs(service: &Service) -> bool {
    service.unit.as_ref().is_some_and(|u| {
        !u.cpu_affinity.is_empty() || !u.nice.is_empty() || !u.io_scheduling_class.is_empty()
    })
}

// ---------------------------------------------------------------------------
// Shell and layout
// ---------------------------------------------------------------------------

/// Which rail item is current. Passed into [`page`] so the active item is
/// decided by the handler rather than by string-matching the request path in a
/// template.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Nav {
    Dashboard,
    Targets,
    Services,
    Deployments,
    Secrets,
    Sources,
    Terminal,
    Audit,
    Settings,
}

impl Nav {
    /// Rail entries in display order: label, href, icon.
    const fn items() -> [(Nav, &'static str, &'static str, &'static str); 9] {
        [
            (Nav::Dashboard, "Overview", "/", "◎"),
            (Nav::Targets, "Targets", "/targets", "▦"),
            (Nav::Services, "Services", "/services", "◈"),
            (Nav::Deployments, "Deployments", "/deployments", "↑"),
            (Nav::Secrets, "Secrets", "/secrets", "✦"),
            (Nav::Sources, "Sources", "/sources", "⑂"),
            (Nav::Terminal, "Terminal", "/terminal", "▶"),
            (Nav::Audit, "Audit", "/audit", "≡"),
            (Nav::Settings, "Settings", "/settings", "⚙"),
        ]
    }
}

/// The full HTML document: head, left rail, and `body` as the main content.
///
/// Assets are served from the binary itself (see `assets/README.md`), so the
/// page renders with no network egress beyond the control plane.
pub fn page(title: &str, nav: Nav, body: Markup) -> Markup {
    html! {
        (DOCTYPE)
        html lang="en" {
            head {
                meta charset="utf-8";
                meta name="viewport" content="width=device-width, initial-scale=1";
                title { (title) " · nudo" }
                link rel="stylesheet" href="/assets/app.css";
                script src="/assets/htmx.min.js" defer {}
                script src="/assets/sse.js" defer {}
            }
            body {
                div .shell {
                    (rail(nav))
                    div .main { (body) }
                }
            }
        }
    }
}

/// The first-run checklist, shown until something has been deployed.
///
/// Three steps, each either done or the next thing to do. Steps already
/// completed stay visible rather than disappearing: seeing what you have
/// already done is what makes the remaining step feel like the last one rather
/// than one of an unknown number.
///
/// It stops appearing the moment a deployment exists — a checklist that
/// outlives its usefulness becomes furniture.
fn first_run_checklist(has_target: bool, has_service: bool) -> Markup {
    // The first incomplete step is the one being asked for; the rest are shown
    // but not offered, so there is exactly one thing to click.
    let steps = [
        (
            has_target,
            "Add a target",
            "A machine reachable over SSH. nudo checks SSH, sudo, systemd and the \
             release directory before you trust it with anything.",
            "/targets/new",
            "Add a target",
        ),
        (
            has_service,
            "Define a service",
            "What to run, where its binary comes from, and how to tell whether it \
             came up. This is the unit file nudo will write.",
            "/services/new",
            "Define a service",
        ),
        (
            false,
            "Deploy it",
            "From the service page, or `nudo deploy <service>`. The release is \
             staged, swapped atomically, health-checked, and rolled back if that \
             check fails.",
            "/services",
            "Go to services",
        ),
    ];

    // The step being asked for: the first one not yet done.
    let current = steps.iter().position(|(done, ..)| !done);
    let completed = steps.iter().filter(|(done, ..)| *done).count();

    html! {
        div .card.checklist {
            div .card-head {
                h2 { "Getting to a first deploy" }
                span .small.muted { (completed) " of " (steps.len()) " done" }
            }
            div .card-body {
                ol .steps {
                    @for (index, (done, title, detail, href, action)) in steps.iter().enumerate() {
                        @let is_current = current == Some(index);
                        li .done[*done] .current[is_current] {
                            span .step-mark { @if *done { "✓" } @else { (index + 1) } }
                            div .step-body {
                                strong { (title) }
                                p .small.muted { (detail) }
                                @if is_current {
                                    a .btn.small.primary href=(href) { (action) }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

/// The left navigation rail.
fn rail(nav: Nav) -> Markup {
    html! {
        nav .rail {
            a .brand href="/" {
                "nudo"
                span .tag { "control plane" }
            }
            @for (item, label, href, icon) in Nav::items() {
                a .nav .active[item == nav] href=(href) {
                    span .nav-icon { (icon) }
                    span { (label) }
                }
            }
            div .spacer {}
            div .rail-foot {
                "systemd deploys, no containers"
            }
        }
    }
}

/// The page header: title, optional subtitle, and right-aligned actions.
pub fn topbar(title: &str, subtitle: Option<&str>, actions: Markup) -> Markup {
    html! {
        header .topbar {
            div .titles {
                h1 { (title) }
                @if let Some(subtitle) = subtitle {
                    div .subtitle { (subtitle) }
                }
            }
            div .actions { (actions) }
        }
    }
}

/// Horizontal tabs. Items are `(label, href, active)`.
pub fn tabs(items: &[(&str, &str, bool)]) -> Markup {
    html! {
        nav .tabs {
            @for (label, href, active) in items {
                a .active[*active] href=(href) { (label) }
            }
        }
    }
}

/// A vertical sub-menu, used inside `.split` on configuration screens.
pub fn submenu(items: &[(&str, &str, bool)]) -> Markup {
    html! {
        nav .submenu {
            @for (label, href, active) in items {
                a .active[*active] href=(href) { (label) }
            }
        }
    }
}

/// An empty state that carries the next action rather than only reporting
/// absence. `action` is `(label, href)`.
pub fn empty_state(heading: &str, message: &str, action: Option<(&str, &str)>) -> Markup {
    html! {
        div .empty {
            h3 { (heading) }
            p { (message) }
            @if let Some((label, href)) = action {
                a .btn .primary href=(href) { (label) }
            }
        }
    }
}

/// A hidden CSRF input. Every POST form in this module goes through this, so a
/// form cannot be added without a token.
fn csrf_input(csrf: &str) -> Markup {
    html! { input type="hidden" name="csrf" value=(csrf); }
}

/// A `.callout` with a heading and body.
fn callout(kind: &str, heading: &str, body: Markup) -> Markup {
    html! {
        div class={ "callout " (kind) } {
            strong { (heading) }
            (body)
        }
    }
}

// ---------------------------------------------------------------------------
// Badges
//
// Every status indicator in the dashboard composes from `badge`, so the mapping
// from a proto enum to a colour lives in exactly one place per type.
// ---------------------------------------------------------------------------

/// The colour families in `app.css`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BadgeKind {
    /// Grey: a resting state that is neither good nor bad (stopped, cancelled).
    Neutral,
    Ok,
    Warn,
    Bad,
    /// Blue: in progress.
    Info,
    /// Outlined red: latency-critical, and nothing else.
    Hot,
}

impl BadgeKind {
    fn class(self) -> &'static str {
        match self {
            BadgeKind::Neutral => "badge",
            BadgeKind::Ok => "badge ok",
            BadgeKind::Warn => "badge warn",
            BadgeKind::Bad => "badge bad",
            BadgeKind::Info => "badge info",
            BadgeKind::Hot => "badge hot",
        }
    }
}

/// A status pill.
pub fn badge(label: &str, kind: BadgeKind) -> Markup {
    html! {
        span class=(kind.class()) {
            span .dot {}
            (label)
        }
    }
}

/// A systemd unit's live state.
///
/// systemd's `active_state`/`sub_state` pair is more detail than an operator
/// wants at a glance, so it collapses to one of six words. The agent reports
/// `active_state == "unknown"` when it could not reach the box at all, which is
/// a warning about our own visibility rather than a claim about the unit.
pub fn unit_badge(status: &UnitStatus) -> Markup {
    match status.active_state.as_str() {
        "active" if status.sub_state == "running" => badge("running", BadgeKind::Ok),
        // Active but not running: `exited` for a oneshot, `start-pre`, etc.
        "active" => badge(
            if status.sub_state.is_empty() {
                "active"
            } else {
                status.sub_state.as_str()
            },
            BadgeKind::Ok,
        ),
        "activating" => badge("starting", BadgeKind::Warn),
        "deactivating" => badge("stopping", BadgeKind::Warn),
        "failed" => badge("failed", BadgeKind::Bad),
        "inactive" => badge("stopped", BadgeKind::Neutral),
        "unknown" => badge("unreachable", BadgeKind::Warn),
        _ => badge("unknown", BadgeKind::Neutral),
    }
}

/// A target's reachability, from the proto enum's wire value.
pub fn target_badge(status: i32) -> Markup {
    match target::Status::try_from(status) {
        Ok(target::Status::Reachable) => badge("reachable", BadgeKind::Ok),
        Ok(target::Status::Unreachable) => badge("unreachable", BadgeKind::Bad),
        _ => badge("unknown", BadgeKind::Neutral),
    }
}

/// A deployment's status. In-flight states are all `Info` so the eye is drawn
/// only to the two outcomes that need action.
pub fn deployment_badge(status: i32) -> Markup {
    use deployment::Status as S;
    match S::try_from(status) {
        Ok(S::Succeeded) => badge("succeeded", BadgeKind::Ok),
        Ok(S::Failed) => badge("failed", BadgeKind::Bad),
        Ok(S::RolledBack) => badge("rolled back", BadgeKind::Warn),
        Ok(S::Cancelled) => badge("cancelled", BadgeKind::Neutral),
        Ok(
            state @ (S::Queued | S::Building | S::Uploading | S::Activating | S::HealthChecking),
        ) => badge(state.as_str(), BadgeKind::Info),
        _ => badge("unspecified", BadgeKind::Neutral),
    }
}

/// The latency-critical marker.
///
/// This flag changes how every operation against the host behaves — unattended
/// mutation is refused server-side and nothing extra may be installed — so it
/// gets the one outlined-red badge in the design and appears next to the target
/// wherever the target appears.
pub fn latency_critical_badge() -> Markup {
    html! {
        span .badge.hot title="Mutations require an explicit override; nothing extra runs here" {
            span .dot {}
            "latency-critical"
        }
    }
}

/// The badge for a target's status plus its latency flag, which travel together.
fn target_badges(target: &Target) -> Markup {
    html! {
        (target_badge(target.status))
        @if target.latency_critical { (latency_critical_badge()) }
    }
}

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
fn labels_line(labels: &HashMap<String, String>) -> String {
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
fn labels_input(labels: &HashMap<String, String>) -> String {
    let mut pairs: Vec<String> = labels.iter().map(|(k, v)| format!("{k}={v}")).collect();
    pairs.sort();
    pairs.join(",")
}

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
fn js_text(value: &str) -> String {
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
fn env_line(env: &HashMap<String, String>) -> String {
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
fn directives_text(map: &HashMap<String, String>) -> String {
    let mut lines: Vec<String> = map.iter().map(|(k, v)| format!("{k}={v}")).collect();
    lines.sort();
    lines.join("\n")
}

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
fn deployments_table(deployments: &[Deployment], services: &[Service]) -> Markup {
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

// ---------------------------------------------------------------------------
// Logs
// ---------------------------------------------------------------------------

/// The journald log viewer for a service.
pub fn logs_view(service: &Service, lines: &[LogLine], grep: &str, follow: bool) -> Markup {
    let base = format!("/services/{}/logs", service.id);

    html! {
        (topbar(&service.name, Some("journald output"), html! {
            a .btn href=(format!("/services/{}", service.id)) { "Back to service" }
        }))
        (tabs(&[
            ("Overview", &format!("/services/{}", service.id), false),
            ("Logs", &base, true),
            ("Unit file", &format!("/services/{}/unit", service.id), false),
            ("Edit", &format!("/services/{}/edit", service.id), false),
        ]))
        div .content {
            div .card {
                // GET, so no CSRF token: this form only reads.
                form .row method="get" action=(base) {
                    div .field {
                        label for="lines" { "Lines" }
                        select id="lines" name="lines" {
                            @for option in ["100", "500", "2000"] {
                                option value=(option) { (option) }
                            }
                        }
                    }
                    div .field style="flex:1;min-width:220px" {
                        label for="grep" { "Filter" }
                        // Typed filtering goes straight to the server: journald
                        // does the matching, so the browser never holds more of
                        // the log than it is showing.
                        input type="search" id="grep" name="grep" value=(grep)
                            placeholder="substring match"
                            hx-get=(base)
                            hx-trigger="keyup changed delay:300ms, search"
                            hx-target="#log-pane"
                            hx-select="#log-pane"
                            hx-swap="outerHTML";
                    }
                    div .field {
                        label { "\u{00a0}" }
                        @if follow {
                            a .btn href=(format!("{base}?grep={}", urlencode(grep))) { "Stop following" }
                        } @else {
                            a .btn.primary href=(format!("{base}?follow=1&grep={}", urlencode(grep))) { "Follow" }
                        }
                    }
                }
            }

            @if follow {
                div hx-ext="sse" sse-connect=(format!("/services/{}/logs/stream?grep={}", service.id, urlencode(grep))) {
                    // As above: each tick carries the full window, so it replaces.
                    div #log-pane .logs.tall sse-swap="log" hx-swap="innerHTML" {
                        (log_lines(lines))
                    }
                }
            } @else {
                div #log-pane .logs.tall {
                    (log_lines(lines))
                }
            }
        }
    }
}

/// Percent-encodes a query-string value.
fn urlencode(value: &str) -> String {
    urlencoding::encode(value).into_owned()
}

/// The journald log fragment: `.line` divs only.
///
/// Separate from [`logs_view`] for the same reason as the deployment fragment —
/// the SSE stream appends exactly this into `#log-pane`.
pub fn log_lines(lines: &[LogLine]) -> Markup {
    html! {
        @if lines.is_empty() {
            div .line { span .msg .placeholder { "No matching log lines." } }
        }
        @for line in lines {
            // Log text is whatever the service printed. Maud escapes it, so a
            // line containing markup renders as text rather than as HTML.
            div class=(priority_class(&line.priority)) {
                span .at { (log_time(line)) }
                span .msg { (line.message) }
            }
        }
    }
}

/// Maps a journald priority to a line class.
///
/// Priorities are syslog severities: 0 emerg through 7 debug. 0-3 (emerg,
/// alert, crit, err) are errors, 4 is a warning, and everything else is
/// ordinary output. Anything unparseable is treated as ordinary rather than
/// alarming — an unrecognised priority is our problem, not the service's.
fn priority_class(priority: &str) -> &'static str {
    match priority.trim() {
        "0" | "1" | "2" | "3" => "line err",
        "4" => "line warn",
        _ => "line",
    }
}

/// The wall-clock time of a log line, or blank when the timestamp is missing.
fn log_time(line: &LogLine) -> String {
    line.at
        .as_ref()
        .and_then(nudo_proto::from_timestamp)
        .map(|at| at.format("%H:%M:%S").to_string())
        .unwrap_or_else(|| "--:--:--".to_string())
}

// ---------------------------------------------------------------------------
// Secrets
// ---------------------------------------------------------------------------

/// The secret store.
///
/// Values are write-only over the API, and this page keeps that property: there
/// is no parameter that could carry a value and no element that could show one.
/// The listing has a name, a scope, a digest prefix and an updated time. The
/// only value input on the page is in the add form, which writes.
///
/// The digest prefix is what makes the page useful without being dangerous —
/// two environments showing the same twelve characters hold the same secret, and
/// twelve characters of a sha256 reveal nothing about the input.
pub fn secrets_list(
    secrets: &[Secret],
    targets: &[Target],
    services: &[Service],
    csrf: &str,
) -> Markup {
    html! {
        (topbar("Secrets", Some("Write-only: values are never returned by the API"), html! {}))
        div .content {
            (callout("info", "Values cannot be read back", html! {
                "Once stored, a value is only ever decrypted on the way to a \
                 target's EnvironmentFile. To change one, write it again — the \
                 digest below tells you whether it actually changed."
            }))

            form .card method="post" action="/secrets" {
                (csrf_input(csrf))
                h2 { "Add or replace a secret" }
                p .card-note { "Writing an existing name replaces its value." }
                div .fields style="margin-top:12px" {
                    div .field {
                        label for="name" { "Name" }
                        input type="text" id="name" name="name" required
                            placeholder="EXCHANGE_API_KEY" autocomplete="off";
                        span .hint { "Becomes the environment variable name." }
                    }
                    div .field {
                        label for="value" { "Value" }
                        // Write-only, and deliberately with no `value` attribute:
                        // the field starts empty on every render, including when
                        // the form comes back after a validation error.
                        input type="password" id="value" name="value" required
                            autocomplete="new-password" spellcheck="false";
                        span .hint { "Sent once and encrypted at rest. It will not be shown again." }
                    }
                    div .field {
                        label for="scope_target_id" { "Target scope" }
                        select id="scope_target_id" name="scope_target_id" {
                            option value="" { "All targets" }
                            @for target in targets {
                                option value=(target.id) { (target.name) }
                            }
                        }
                    }
                    div .field {
                        label for="scope_service_id" { "Service scope" }
                        select id="scope_service_id" name="scope_service_id" {
                            option value="" { "All services" }
                            @for service in services {
                                option value=(service.id) { (service.name) }
                            }
                        }
                        span .hint { "Narrower scope wins when both are set." }
                    }
                }
                div .form-actions {
                    button .btn.primary type="submit" { "Store secret" }
                }
            }

            div .card.pad-0 {
                div .card-head { h2 { "Stored secrets" } }
                @if secrets.is_empty() {
                    div .card-body { p .muted { "Nothing stored yet." } }
                } @else {
                    div .table-scroll {
                        table {
                            thead {
                                tr {
                                    th { "Name" }
                                    th { "Scope" }
                                    th { "Digest" }
                                    th { "Updated" }
                                    th {}
                                }
                            }
                            tbody {
                                @for secret in secrets {
                                    tr {
                                        td .mono { (secret.name) }
                                        td .small { (scope_label(secret)) }
                                        // A prefix of the sha256, for drift
                                        // detection. Never the value.
                                        td .mono.small.faint { (digest_prefix(&secret.digest)) }
                                        td .nowrap.small.muted { (ago(secret.updated_at.as_ref())) }
                                        td {
                                            form method="post" action=(format!("/secrets/{}/delete", secret.id)) {
                                                (csrf_input(csrf))
                                                button .btn.small.danger type="submit"
                                                    onclick=(format!("return confirm('Delete {}? Any service using it will fail to start on its next deploy, and the value cannot be recovered.')", js_text(&secret.name))) {
                                                    "Delete"
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

// ---------------------------------------------------------------------------
// Sources
// ---------------------------------------------------------------------------

/// Connected git sources, plus the GitHub App creation flow.
pub fn sources_list(sources: &[Source], csrf: &str) -> Markup {
    html! {
        (topbar("Sources", Some("Where nudo clones and builds from"), html! {}))
        div .content {
            div .card.pad-0 {
                div .card-head { h2 { "Connected sources" } }
                @if sources.is_empty() {
                    (empty_state(
                        "No sources connected",
                        "Connect a GitHub App and nudo can build a service from a repository and deploy on push.",
                        Some(("Create a GitHub App", "#create-app")),
                    ))
                } @else {
                    div .table-scroll {
                        table {
                            thead {
                                tr {
                                    th { "Name" }
                                    th { "Kind" }
                                    th { "Account" }
                                    th { "Installed" }
                                    th { "Created" }
                                    th {}
                                }
                            }
                            tbody {
                                @for source in sources {
                                    tr {
                                        td {
                                            @if source.html_url.is_empty() {
                                                (source.name)
                                            } @else {
                                                a href=(source.html_url) rel="noreferrer noopener" target="_blank" {
                                                    (source.name)
                                                }
                                            }
                                            @if !source.app_slug.is_empty() {
                                                div .small.faint.mono { (source.app_slug) }
                                            }
                                        }
                                        td .small {
                                            (source::Kind::try_from(source.kind)
                                                .unwrap_or(source::Kind::Unspecified)
                                                .as_str())
                                        }
                                        td .small { (or_dash(&source.account_login)) }
                                        td {
                                            @if source.installed {
                                                (badge("installed", BadgeKind::Ok))
                                            } @else {
                                                // A created-but-uninstalled App
                                                // cannot clone anything, so this
                                                // is a warning, not neutral.
                                                (badge("not installed", BadgeKind::Warn))
                                            }
                                        }
                                        td .nowrap.small.muted { (ago(source.created_at.as_ref())) }
                                        td {
                                            form method="post" action=(format!("/sources/{}/delete", source.id)) {
                                                (csrf_input(csrf))
                                                button .btn.small.danger type="submit"
                                                    onclick=(format!("return confirm('Disconnect {}? Services that build from it will fail until another source is configured.')", js_text(&source.name))) {
                                                    "Disconnect"
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

            form #create-app .card method="post" action="/sources/github" {
                (csrf_input(csrf))
                h2 { "Create a GitHub App" }
                // Deliberately no field for App credentials: nudo publishes a
                // manifest and GitHub hands the signing material back over the
                // callback, so it is never typed into a browser.
                p .card-note {
                    "nudo generates the manifest and GitHub hands back the credentials \
                     through the callback, so nothing sensitive is pasted into a form."
                }
                div .fields style="margin-top:12px" {
                    div .field {
                        label for="app_name" { "App name" }
                        input type="text" id="app_name" name="name" required
                            placeholder="nudo-deploy";
                        span .hint { "Must be unique across GitHub." }
                    }
                    div .field {
                        label for="organization" { "Organization" }
                        input type="text" id="organization" name="organization"
                            placeholder="leave blank for your personal account";
                        span .hint { "You need owner rights on the organization." }
                    }
                }
                div .form-actions {
                    button .btn.primary type="submit" { "Continue to GitHub" }
                }
            }

            (callout("info", "Already have an App?", html! {
                "Install your existing nudo App on the account that owns the repositories, \
                 then point its webhook and callback URLs at this control plane. It shows up \
                 above as soon as the installation webhook arrives."
            }))
        }
    }
}

/// Step one of GitHub's App manifest flow.
///
/// GitHub accepts the manifest only as a form POST to its own domain, so this is
/// a self-submitting page rather than a redirect: the manifest is too long for a
/// query string and must not end up in a browser history or a proxy log.
///
/// The manifest is a JSON document nudo generated. It is placed in a textarea, so
/// maud's escaping is what keeps it inert text rather than markup.
pub fn github_handoff(post_url: &str, manifest_json: &str) -> Markup {
    auth_shell(
        "Continue on GitHub",
        html! {
            div .card {
                h2 { "Continue on GitHub" }
                p .card-note {
                    "GitHub creates the App and sends its credentials straight back to \
                     this control plane. Nothing sensitive passes through your clipboard."
                }
                // GitHub's endpoint, so no CSRF token of ours applies. The manifest
                // itself is the payload and it carries its own state parameter.
                form #handoff method="post" action=(post_url) {
                    textarea name="manifest" hidden { (manifest_json) }
                    div .form-actions {
                        button .btn.primary type="submit" { "Create the App on GitHub" }
                        a .btn href="/sources" { "Cancel" }
                    }
                }
                p .small.faint {
                    "If nothing happens, use the button above — the form submits itself \
                     only when JavaScript is enabled."
                }
            }
            script { (PreEscaped("document.getElementById('handoff').submit();")) }
        },
    )
}

/// The one and only time an API token's plaintext is shown.
///
/// Only a digest of the token is stored, so this page cannot be reproduced. It
/// says so plainly rather than leaving the reader to discover it, and it renders
/// the value as text in a `pre` rather than in an input — an input would be
/// re-sent by a browser's form restoration on a back-navigation.
pub fn token_created(name: &str, plaintext: &str) -> Markup {
    html! {
        (topbar("API token created", Some(name), html! {
            a .btn href="/settings" { "Done" }
        }))
        div .content {
            (callout("warn", "Copy this now", html! {
                "Only a digest is stored, so this value cannot be shown again. If you \
                 lose it, revoke the token and create another."
            }))
            div .card {
                pre .unit { (plaintext) }
            }
            p .small.muted {
                "Pass it to the CLI as " code { "NUDO_TOKEN" } ", or to the MCP server \
                 in its configuration."
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Audit
// ---------------------------------------------------------------------------

/// The audit log.
///
/// The point of this screen is telling a human clicking deploy apart from an
/// agent calling an MCP tool, so the actor kind is a column rather than a
/// tooltip, and a refusal is coloured like a failure — a refused mutation is the
/// guardrail working, and it is what someone reading this log is looking for.
pub fn audit_list(entries: &[AuditEntry]) -> Markup {
    html! {
        (topbar("Audit", Some("Every mutating call, and who made it"), html! {}))
        div .content {
            div .card.pad-0 {
                @if entries.is_empty() {
                    div .card-body { p .muted { "Nothing recorded yet." } }
                } @else {
                    div .table-scroll {
                        table {
                            thead {
                                tr {
                                    th { "When" }
                                    th { "Kind" }
                                    th { "Actor" }
                                    th { "Action" }
                                    th { "Subject" }
                                    th { "Mode" }
                                    th { "Summary" }
                                }
                            }
                            tbody {
                                @for entry in entries {
                                    @let refused = entry.action.contains("refused");
                                    tr {
                                        td .nowrap.small.muted { (ago(entry.at.as_ref())) }
                                        td {
                                            @match entry.actor.as_ref().map(|a| a.kind_str()) {
                                                Some("human") => (badge("human", BadgeKind::Neutral)),
                                                // An agent acting on production is
                                                // the case worth spotting quickly.
                                                Some("agent") => (badge("agent", BadgeKind::Info)),
                                                Some("webhook") => (badge("webhook", BadgeKind::Info)),
                                                Some("system") => (badge("system", BadgeKind::Neutral)),
                                                _ => (badge("unknown", BadgeKind::Neutral)),
                                            }
                                        }
                                        td .small {
                                            (entry.actor.as_ref().map(|a| or_dash(&a.label)).unwrap_or_else(|| "-".to_string()))
                                        }
                                        td .small {
                                            @if refused {
                                                span .badge.bad { (entry.action) }
                                            } @else {
                                                span .mono { (entry.action) }
                                            }
                                        }
                                        td .mono.small { (or_dash(&entry.subject_id)) }
                                        td {
                                            @if entry.dry_run {
                                                // A dry run changed nothing, so it
                                                // must not read like a real change.
                                                (badge("dry run", BadgeKind::Warn))
                                            } @else {
                                                span .faint.small { "applied" }
                                            }
                                        }
                                        td .small { (truncate(&entry.summary, 80)) }
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

// ---------------------------------------------------------------------------
// Terminal
// ---------------------------------------------------------------------------

/// The interactive PTY page.
///
/// The browser is handed a session id and a single-use token and nothing else —
/// no host, no port, no user, no command line. The server already knows which
/// target the grant is for, so the client cannot ask for a different one, and a
/// leaked page source does not disclose the fleet's addressing.
///
/// Both values go through `serde_json`, so quotes, backslashes and control
/// characters inside them cannot end the JSON string. `serde_json` does not
/// escape `/`, and an HTML parser ends a `<script>` element at the first literal
/// `</`, so the two-character sequence is rewritten to `<\/` — legal JSON that
/// decodes to the same text and cannot terminate the element. That is the only
/// `PreEscaped` in this module.
pub fn terminal_page(target: &Target, session_id: &str, token: &str) -> Markup {
    // Fall back to an empty object rather than panicking: a page with no config
    // shows "connecting…" and then a clean failure, which beats a 500.
    let config = serde_json::to_string(&serde_json::json!({
        "sessionId": session_id,
        "token": token,
    }))
    .unwrap_or_else(|_| "{}".to_string())
    .replace("</", "<\\/");

    html! {
        (topbar(&format!("Terminal · {}", target.name), Some("Interactive shell over ssh"), html! {
            @if target.latency_critical { (latency_critical_badge()) }
            a .btn href=(format!("/targets/{}", target.id)) { "Back to target" }
        }))
        div .content {
            @if target.latency_critical {
                (callout("bad", "Latency-critical host", html! {
                    "Anything you run here competes with the process this machine \
                     exists for. Every command in this session is recorded in the \
                     audit log."
                }))
            }

            link rel="stylesheet" href="/assets/xterm.css";
            div .term-wrap {
                div #terminal {}
            }
            div #term-status .term-status { "connecting…" }
            p .small.faint {
                "The session is single-use and expires on its own. Closing this tab \
                 ends it; reconnecting needs a new one."
            }

            script src="/assets/xterm.js" {}
            script src="/assets/xterm-addon-fit.js" {}
            // Set before terminal.js runs, which reads it at load.
            script { (PreEscaped(format!("window.nudoTerminal = {config};"))) }
            script src="/assets/terminal.js" {}
        }
    }
}

// ---------------------------------------------------------------------------
// Settings
// ---------------------------------------------------------------------------

/// One API token as the settings page needs it.
///
/// A view type rather than a proto message: tokens are an authentication concern
/// of the web tier and are not part of the control plane's gRPC surface. The
/// token secret itself is not a field here — it is shown once, at creation, and
/// never again.
#[derive(Debug, Clone)]
pub struct TokenView {
    pub id: String,
    pub name: String,
    pub scopes: String,
    pub last_used: Option<chrono::DateTime<chrono::Utc>>,
    pub revoked: bool,
    pub created: chrono::DateTime<chrono::Utc>,
}

/// Account settings and API tokens.
pub fn settings_page(
    api_tokens: &[TokenView],
    user_email: &str,
    prefs: &SettingsPrefs,
    csrf: &str,
) -> Markup {
    html! {
        (topbar("Settings", Some(user_email), html! {}))
        div .content {
            div .split {
                (submenu(&[
                    ("Account", "/settings", true),
                    ("API tokens", "/settings#tokens", false),
                    ("This instance", "/settings#instance", false),
                ]))
                div {
                    form .card method="post" action="/settings/password" {
                        (csrf_input(csrf))
                        h2 { "Change password" }
                        div .fields style="margin-top:12px" {
                            div .field {
                                label for="current_password" { "Current password" }
                                input type="password" id="current_password" name="current_password"
                                    required autocomplete="current-password";
                            }
                            div .field {
                                label for="new_password" { "New password" }
                                input type="password" id="new_password" name="new_password"
                                    required autocomplete="new-password" minlength="12";
                                span .hint { "At least 12 characters." }
                            }
                        }
                        div .form-actions {
                            button .btn.primary type="submit" { "Change password" }
                        }
                    }

                    form #tokens .card method="post" action="/settings/tokens" {
                        (csrf_input(csrf))
                        h2 { "New API token" }
                        p .card-note {
                            "Used by the CLI and the MCP server. Shown once when created \
                             and never stored in a form that can display it."
                        }
                        div .fields style="margin-top:12px" {
                            div .field {
                                label for="token_name" { "Name" }
                                input type="text" id="token_name" name="name" required
                                    placeholder="laptop-cli";
                            }
                            div .field {
                                label { "Scope" }
                                // The store knows two scopes. Read is always
                                // granted; write is the box, because a token
                                // that can deploy is the one worth thinking
                                // about before minting.
                                label .check {
                                    input type="checkbox" name="write" value="on";
                                    span {
                                        "Allow writes — deploy, roll back, unit "
                                        "actions and secrets. Leave unticked for a "
                                        "read-only token."
                                    }
                                }
                            }
                        }
                        div .form-actions {
                            button .btn.primary type="submit" { "Create token" }
                        }
                    }

                    div .card.pad-0 {
                        div .card-head { h2 { "Existing tokens" } }
                        @if api_tokens.is_empty() {
                            div .card-body { p .muted { "No tokens yet." } }
                        } @else {
                            div .table-scroll {
                                table {
                                    thead {
                                        tr {
                                            th { "Name" }
                                            th { "Scopes" }
                                            th { "Created" }
                                            th { "Last used" }
                                            th { "Status" }
                                            th {}
                                        }
                                    }
                                    tbody {
                                        @for token in api_tokens {
                                            tr {
                                                td { (token.name) }
                                                td .small.mono { (token.scopes) }
                                                td .nowrap.small.muted { (ago_at(token.created)) }
                                                td .nowrap.small.muted {
                                                    @match token.last_used {
                                                        // "never used" is a reason
                                                        // to revoke it, so say it.
                                                        Some(at) => (ago_at(at)),
                                                        None => "never",
                                                    }
                                                }
                                                td {
                                                    @if token.revoked {
                                                        (badge("revoked", BadgeKind::Bad))
                                                    } @else {
                                                        (badge("active", BadgeKind::Ok))
                                                    }
                                                }
                                                td {
                                                    @if !token.revoked {
                                                        form method="post" action=(format!("/settings/tokens/{}/revoke", token.id)) {
                                                            (csrf_input(csrf))
                                                            button .btn.small.danger type="submit"
                                                                onclick=(format!("return confirm('Revoke {}? Anything using it stops working immediately.')", js_text(&token.name))) {
                                                                "Revoke"
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

                    div #instance .card {
                        h2 { "This instance" }
                        p .card-note {
                            "nudo sends nothing about you anywhere. There is no usage \
                             ping, no install count and no identifier — the release \
                             check below fetches a static file and posts nothing."
                        }

                        form method="post" action="/settings/updates" style="margin-top:12px" {
                            (csrf_input(csrf))
                            div .field {
                                label .check {
                                    input type="checkbox" name="enabled" value="on"
                                        checked[prefs.update_check_enabled];
                                    span {
                                        "Check for new releases and show a banner when one \
                                         is out. Nothing is ever installed automatically."
                                    }
                                }
                            }
                            @if !prefs.last_checked.is_empty() {
                                p .small.muted { "Last checked " (prefs.last_checked) "." }
                            }
                            div .form-actions {
                                button .btn.small type="submit" { "Save" }
                            }
                        }

                        form method="post" action="/settings/support" style="margin-top:12px" {
                            (csrf_input(csrf))
                            div .field {
                                label .check {
                                    input type="checkbox" name="enabled" value="on"
                                        checked[prefs.support_prompt_enabled];
                                    span {
                                        "Show the occasional note asking for support. At \
                                         most once a month, and never before you have \
                                         deployed something."
                                    }
                                }
                            }
                            div .form-actions {
                                button .btn.small type="submit" { "Save" }
                            }
                        }
                    }
                }
            }
        }
    }
}

/// The instance-wide preferences shown on the settings page.
#[derive(Debug, Clone, Default)]
pub struct SettingsPrefs {
    pub update_check_enabled: bool,
    pub support_prompt_enabled: bool,
    /// Humanised time of the last release check, empty when it has never run.
    pub last_checked: String,
}

// ---------------------------------------------------------------------------
// Auth
// ---------------------------------------------------------------------------

/// The sign-in page. Not wrapped by [`page`]: there is no rail to show to
/// someone who is not signed in.
pub fn login_page(error: Option<&str>, csrf: &str) -> Markup {
    auth_shell(
        "Sign in",
        html! {
            form .card method="post" action="/login" {
                (csrf_input(csrf))
                h2 { "Sign in" }
                @if let Some(error) = error {
                    (callout("bad", "Could not sign in", html! { (error) }))
                }
                div .field style="margin-top:12px" {
                    label for="email" { "Email" }
                    input type="email" id="email" name="email" required
                        autocomplete="username" autofocus;
                }
                div .field style="margin-top:12px" {
                    label for="password" { "Password" }
                    input type="password" id="password" name="password" required
                        autocomplete="current-password";
                }
                div .form-actions {
                    button .btn.primary type="submit" style="width:100%;justify-content:center" {
                        "Sign in"
                    }
                }
            }
        },
    )
}

/// First-run setup: creates the only account that can create others.
pub fn setup_page(error: Option<&str>, csrf: &str) -> Markup {
    auth_shell(
        "Set up nudo",
        html! {
            form .card method="post" action="/setup" {
                (csrf_input(csrf))
                h2 { "Create the first account" }
                p .card-note {
                    "This control plane has no users yet. Whoever completes this form \
                     controls every target it manages, so do it now rather than leaving \
                     the page reachable."
                }
                @if let Some(error) = error {
                    (callout("bad", "Could not create the account", html! { (error) }))
                }
                div .field style="margin-top:12px" {
                    label for="email" { "Email" }
                    input type="email" id="email" name="email" required
                        autocomplete="username" autofocus;
                }
                div .field style="margin-top:12px" {
                    label for="password" { "Password" }
                    input type="password" id="password" name="password" required
                        autocomplete="new-password" minlength="12";
                    span .hint { "At least 12 characters." }
                }
                div .field style="margin-top:12px" {
                    label for="password_confirm" { "Confirm password" }
                    input type="password" id="password_confirm" name="password_confirm" required
                        autocomplete="new-password" minlength="12";
                }
                div .form-actions {
                    button .btn.primary type="submit" style="width:100%;justify-content:center" {
                        "Create account"
                    }
                }
            }
        },
    )
}

/// The centred single-card document the auth pages share.
fn auth_shell(title: &str, body: Markup) -> Markup {
    html! {
        (DOCTYPE)
        html lang="en" {
            head {
                meta charset="utf-8";
                meta name="viewport" content="width=device-width, initial-scale=1";
                title { (title) " · nudo" }
                link rel="stylesheet" href="/assets/app.css";
            }
            body {
                div .auth-page {
                    div .auth-card {
                        div .brand {
                            "nudo"
                            span .tag { "control plane" }
                        }
                        (body)
                    }
                }
            }
        }
    }
}

/// An error page. Deliberately says nothing about internals — an error message
/// is a place where a host name or a stack frame leaks by accident.
pub fn error_page(code: u16, message: &str) -> Markup {
    auth_shell(
        &format!("{code}"),
        html! {
            div .card {
                h2 { (code) }
                p .muted style="margin-top:6px" { (message) }
                div .form-actions {
                    a .btn href="/" { "Back to overview" }
                }
            }
        },
    )
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use nudo_proto::{
        Actor, ArtifactSource, GitSource, HealthCheck, SystemdUnit, artifact_source,
        check_target_response, health_check,
    };

    /// Renders to a string, which is what every assertion below inspects.
    fn s(markup: Markup) -> String {
        markup.into_string()
    }

    fn a_target() -> Target {
        Target {
            id: "tgt_1".to_string(),
            name: "hft-box".to_string(),
            host: "10.0.0.4".to_string(),
            port: 22,
            user: "deploy".to_string(),
            ssh_key_id: "sec_key".to_string(),
            latency_critical: false,
            status: target::Status::Reachable as i32,
            last_seen_at: Some(nudo_proto::to_timestamp(chrono::Utc::now())),
            created_at: Some(nudo_proto::to_timestamp(chrono::Utc::now())),
            ..Default::default()
        }
    }

    fn a_service() -> Service {
        Service {
            id: "svc_1".to_string(),
            target_id: "tgt_1".to_string(),
            name: "bot".to_string(),
            artifact: Some(ArtifactSource {
                kind: Some(artifact_source::Kind::Git(GitSource {
                    source_id: "src_1".to_string(),
                    repo: "owner/bot".to_string(),
                    branch: "main".to_string(),
                    build_command: "cargo build --release".to_string(),
                    artifact_path: "target/release/bot".to_string(),
                    auto_deploy_on_push: false,
                })),
            }),
            unit: Some(SystemdUnit {
                unit_name: "bot.service".to_string(),
                description: "trading bot".to_string(),
                restart: "always".to_string(),
                restart_sec: 2,
                user: "deploy".to_string(),
                ..Default::default()
            }),
            health_check: Some(HealthCheck {
                kind: Some(health_check::Kind::HttpUrl(
                    "http://127.0.0.1:9/z".to_string(),
                )),
                timeout_seconds: 5,
                retries: 3,
                initial_delay_seconds: 2,
            }),
            release_root: "/opt/bot".to_string(),
            keep_releases: 5,
            current_release_id: "rel_2".to_string(),
            ..Default::default()
        }
    }

    fn running() -> UnitStatus {
        UnitStatus {
            service_id: "svc_1".to_string(),
            active_state: "active".to_string(),
            sub_state: "running".to_string(),
            enabled: true,
            pid: 4242,
            memory_bytes: 64 * 1024 * 1024,
            since: Some(nudo_proto::to_timestamp(chrono::Utc::now())),
            restart_count: 0,
        }
    }

    // -- secrets: the property the whole module exists to preserve ----------

    #[test]
    fn a_secret_listing_shows_a_digest_and_never_the_value() {
        // There is no parameter that could carry a value, so the test asserts on
        // the whole page: nothing anywhere in it resembles a plaintext secret.
        let secret = Secret {
            id: "sec_1".to_string(),
            name: "EXCHANGE_API_KEY".to_string(),
            digest: "9f86d081884c7d659a2feaa0c55ad015".to_string(),
            updated_at: Some(nudo_proto::to_timestamp(chrono::Utc::now())),
            ..Default::default()
        };
        let rendered = s(secrets_list(
            &[secret],
            &[a_target()],
            &[a_service()],
            "tok",
        ));

        assert!(rendered.contains("EXCHANGE_API_KEY"), "the name is shown");
        assert!(
            rendered.contains("9f86d081884c"),
            "a digest prefix is shown"
        );
        // Not the whole digest either — a prefix is all that drift detection needs.
        assert!(!rendered.contains("9f86d081884c7d659a2feaa0c55ad015"));
        assert!(rendered.contains("global"), "the scope is shown");
    }

    #[test]
    fn the_secret_value_input_never_carries_a_value_attribute() {
        // The add form is the one place a value field exists. It must render
        // empty every time, including when redisplayed after a failed submit.
        let rendered = s(secrets_list(&[], &[], &[], "tok"));
        let field = rendered
            .split("id=\"value\"")
            .nth(1)
            .expect("the value input")
            .split('>')
            .next()
            .expect("the end of the tag");
        assert!(
            !field.contains("value="),
            "the write-only field must have no value attribute: {field}"
        );
        assert!(
            field.contains("type=\"password\"")
                || rendered.contains("type=\"password\" id=\"value\"")
        );
    }

    #[test]
    fn a_secret_row_has_no_element_that_could_reveal_a_value() {
        let secret = Secret {
            id: "sec_1".to_string(),
            name: "TOKEN".to_string(),
            digest: "deadbeefcafe0000".to_string(),
            ..Default::default()
        };
        let rendered = s(secrets_list(&[secret], &[], &[], "tok"));
        // No "reveal"/"show" affordance to click, and no <code> holding a value.
        assert!(!rendered.to_lowercase().contains("reveal"));
        assert!(!rendered.contains("Show value"));
    }

    #[test]
    fn a_services_secret_selection_is_by_id_and_shows_no_values() {
        let secret = Secret {
            id: "sec_1".to_string(),
            name: "API_KEY".to_string(),
            digest: "abc123abc123".to_string(),
            ..Default::default()
        };
        let rendered = s(service_form(None, &[a_target()], &[], &[secret], "tok"));
        assert!(rendered.contains("name=\"secret_ids\""), "selected by id");
        assert!(rendered.contains("value=\"sec_1\""));
        assert!(rendered.contains("API_KEY"));
        // No text input that could accept or display a value in this section.
        assert!(!rendered.contains("name=\"secret_value\""));
    }

    #[test]
    fn a_service_detail_lists_secret_ids_but_not_values() {
        let mut service = a_service();
        service.secret_ids = vec!["sec_1".to_string(), "sec_2".to_string()];
        let rendered = s(service_detail(
            &service,
            &a_target(),
            &running(),
            &[],
            &[],
            "tok",
        ));
        assert!(rendered.contains("sec_1, sec_2"));
        assert!(rendered.contains("values are written on the target"));
    }

    #[test]
    fn a_targets_ssh_key_is_chosen_from_the_store_not_typed() {
        // A key pasted into a form is logged by everything on the way in, so the
        // form must only ever offer a reference.
        let secret = Secret {
            id: "sec_key".to_string(),
            name: "deploy-key".to_string(),
            ..Default::default()
        };
        let rendered = s(target_form(None, &[secret], "tok"));
        assert!(rendered.contains("<select id=\"ssh_key_id\" name=\"ssh_key_id\""));
        assert!(rendered.contains("value=\"sec_key\""));
        // Not a textarea or text input that could hold key material.
        assert!(!rendered.contains("name=\"ssh_private_key\""));
        assert!(!rendered.contains("<textarea id=\"ssh_key_id\""));
    }

    // -- CSRF --------------------------------------------------------------

    /// Every rendered POST form has a hidden csrf input.
    fn assert_every_post_form_has_csrf(rendered: &str, token: &str, what: &str) {
        let forms: Vec<&str> = rendered.split("<form").skip(1).collect();
        let posts: Vec<&&str> = forms
            .iter()
            .filter(|f| {
                f.split("</form>")
                    .next()
                    .unwrap_or(f)
                    .contains("method=\"post\"")
            })
            .collect();
        assert!(!posts.is_empty(), "{what} renders no POST form to check");
        for form in posts {
            let body = form.split("</form>").next().unwrap_or(form);
            assert!(
                body.contains(&format!("name=\"csrf\" value=\"{token}\"")),
                "{what} has a POST form without a csrf input: {body}"
            );
        }
    }

    #[test]
    fn every_post_form_on_every_screen_carries_a_csrf_token() {
        let token = "csrf-token-abc";
        let target = a_target();
        let service = a_service();
        let secret = Secret {
            id: "sec_1".to_string(),
            name: "API_KEY".to_string(),
            digest: "abc123abc123".to_string(),
            ..Default::default()
        };
        let source = Source {
            id: "src_1".to_string(),
            name: "nudo-deploy".to_string(),
            kind: source::Kind::GithubApp as i32,
            installed: true,
            ..Default::default()
        };
        let release = Release {
            id: "rel_1".to_string(),
            service_id: "svc_1".to_string(),
            ..Default::default()
        };
        let deployment = Deployment {
            id: "dep_1".to_string(),
            service_id: "svc_1".to_string(),
            status: deployment::Status::Building as i32,
            ..Default::default()
        };
        let token_view = TokenView {
            id: "tok_1".to_string(),
            name: "laptop".to_string(),
            scopes: "deploy".to_string(),
            last_used: None,
            revoked: false,
            created: chrono::Utc::now(),
        };

        let screens: Vec<(&str, String)> = vec![
            (
                "target_form(new)",
                s(target_form(None, std::slice::from_ref(&secret), token)),
            ),
            (
                "target_form(edit)",
                s(target_form(
                    Some(&target),
                    std::slice::from_ref(&secret),
                    token,
                )),
            ),
            (
                "service_form(new)",
                s(service_form(
                    None,
                    std::slice::from_ref(&target),
                    std::slice::from_ref(&source),
                    std::slice::from_ref(&secret),
                    token,
                )),
            ),
            (
                "service_form(edit)",
                s(service_form(
                    Some(&service),
                    std::slice::from_ref(&target),
                    std::slice::from_ref(&source),
                    std::slice::from_ref(&secret),
                    token,
                )),
            ),
            (
                "service_detail",
                s(service_detail(
                    &service,
                    &target,
                    &running(),
                    &[release],
                    std::slice::from_ref(&deployment),
                    token,
                )),
            ),
            (
                "deployment_detail",
                s(deployment_detail(&deployment, &service, &[], true, token)),
            ),
            (
                "secrets_list",
                s(secrets_list(
                    &[secret],
                    std::slice::from_ref(&target),
                    std::slice::from_ref(&service),
                    token,
                )),
            ),
            ("sources_list", s(sources_list(&[source], token))),
            (
                "settings_page",
                s(settings_page(
                    &[token_view],
                    "a@example.com",
                    &SettingsPrefs::default(),
                    token,
                )),
            ),
            ("login_page", s(login_page(None, token))),
            ("setup_page", s(setup_page(None, token))),
        ];

        for (what, rendered) in screens {
            assert_every_post_form_has_csrf(&rendered, token, what);
        }
    }

    #[test]
    fn a_read_only_screen_posts_nothing_and_so_needs_no_token() {
        // Running the preflight checks probes the target and changes nothing, so
        // it is a link. A screen with no POST form has nothing to forge.
        let rendered = s(target_detail(&a_target(), &[], &HashMap::new(), None));
        assert!(!rendered.contains("method=\"post\""));
        assert!(rendered.contains("Run checks"));
        assert!(rendered.contains("href=\"/targets/tgt_1?check=1\""));

        for rendered in [
            s(deployments_list(&[], &[])),
            s(audit_list(&[])),
            s(targets_list(&[])),
            s(services_list(&[], &[], &HashMap::new())),
            s(service_unit(&a_service(), "[Unit]")),
        ] {
            assert!(!rendered.contains("method=\"post\""));
        }
    }

    #[test]
    fn the_log_filter_form_is_a_get_and_so_needs_no_token() {
        // A read-only form with a token would suggest the token is decorative.
        let rendered = s(logs_view(&a_service(), &[], "", false));
        assert!(rendered.contains("method=\"get\""));
        assert!(!rendered.contains("name=\"csrf\""));
    }

    // -- latency-critical --------------------------------------------------

    #[test]
    fn the_latency_critical_badge_is_loud_and_says_what_it_is() {
        let rendered = s(latency_critical_badge());
        assert!(rendered.contains("class=\"badge hot\""));
        assert!(rendered.contains("latency-critical"));
    }

    #[test]
    fn a_latency_critical_target_is_marked_everywhere_it_appears() {
        let mut hot = a_target();
        hot.latency_critical = true;
        let cold = Target {
            id: "tgt_2".to_string(),
            name: "spare".to_string(),
            ..a_target()
        };

        let listing = s(targets_list(&[hot.clone()]));
        assert!(listing.contains("latency-critical"));
        assert!(listing.contains("badge hot"));

        let detail = s(target_detail(&hot, &[], &HashMap::new(), None));
        assert!(detail.contains("badge hot"));
        assert!(
            detail.contains("Latency-critical host"),
            "and an explanation"
        );

        let overview = s(dashboard(&[hot.clone()], &[], &HashMap::new(), &[]));
        assert!(overview.contains("badge hot"));

        // And absent for an ordinary host, on every one of those screens.
        assert!(!s(targets_list(std::slice::from_ref(&cold))).contains("badge hot"));
        assert!(!s(target_detail(&cold, &[], &HashMap::new(), None)).contains("badge hot"));
        assert!(!s(dashboard(&[cold], &[], &HashMap::new(), &[])).contains("badge hot"));
    }

    #[test]
    fn deploying_to_a_latency_critical_host_confirms_with_that_named() {
        let mut hot = a_target();
        hot.latency_critical = true;
        let rendered = s(service_detail(
            &a_service(),
            &hot,
            &running(),
            &[],
            &[],
            "tok",
        ));
        assert!(
            rendered.contains("LATENCY-CRITICAL"),
            "the confirm names it"
        );

        let ordinary = s(service_detail(
            &a_service(),
            &a_target(),
            &running(),
            &[],
            &[],
            "tok",
        ));
        assert!(!ordinary.contains("LATENCY-CRITICAL"));
    }

    #[test]
    fn latency_knobs_are_shown_prominently_only_when_set() {
        let mut pinned = a_service();
        pinned.unit = Some(SystemdUnit {
            unit_name: "bot.service".to_string(),
            cpu_affinity: "2-5".to_string(),
            nice: "-10".to_string(),
            io_scheduling_class: "realtime".to_string(),
            ..Default::default()
        });

        let rendered = s(service_detail(
            &pinned,
            &a_target(),
            &running(),
            &[],
            &[],
            "t",
        ));
        assert!(rendered.contains("Latency configuration"));
        assert!(rendered.contains("CPUAffinity"));
        assert!(rendered.contains("2-5"));
        assert!(rendered.contains("IOSchedulingClass"));

        let plain = s(service_detail(
            &a_service(),
            &a_target(),
            &running(),
            &[],
            &[],
            "t",
        ));
        assert!(!plain.contains("Latency configuration"));
    }

    // -- badges ------------------------------------------------------------

    #[test]
    fn every_badge_kind_maps_to_its_class() {
        assert!(s(badge("x", BadgeKind::Neutral)).contains("class=\"badge\""));
        assert!(s(badge("x", BadgeKind::Ok)).contains("class=\"badge ok\""));
        assert!(s(badge("x", BadgeKind::Warn)).contains("class=\"badge warn\""));
        assert!(s(badge("x", BadgeKind::Bad)).contains("class=\"badge bad\""));
        assert!(s(badge("x", BadgeKind::Info)).contains("class=\"badge info\""));
        assert!(s(badge("x", BadgeKind::Hot)).contains("class=\"badge hot\""));
    }

    #[test]
    fn a_badge_label_is_escaped() {
        // Status text can come from systemd, which is not our code.
        let rendered = s(badge("<b>x</b>", BadgeKind::Ok));
        assert!(rendered.contains("&lt;b&gt;"));
        assert!(!rendered.contains("<b>x</b>"));
    }

    #[test]
    fn unit_states_collapse_to_the_right_word_and_colour() {
        let with = |active: &str, sub: &str| {
            s(unit_badge(&UnitStatus {
                active_state: active.to_string(),
                sub_state: sub.to_string(),
                ..Default::default()
            }))
        };

        let running = with("active", "running");
        assert!(running.contains("badge ok") && running.contains("running"));

        let starting = with("activating", "start-pre");
        assert!(starting.contains("badge warn") && starting.contains("starting"));

        let stopping = with("deactivating", "stop");
        assert!(stopping.contains("badge warn") && stopping.contains("stopping"));

        let failed = with("failed", "failed");
        assert!(failed.contains("badge bad") && failed.contains("failed"));

        let stopped = with("inactive", "dead");
        assert!(stopped.contains("class=\"badge\"") && stopped.contains("stopped"));

        // Our own blindness, not a claim about the unit.
        let unreachable = with("unknown", "");
        assert!(unreachable.contains("badge warn") && unreachable.contains("unreachable"));

        let nonsense = with("reloading-sideways", "");
        assert!(nonsense.contains("class=\"badge\"") && nonsense.contains("unknown"));
    }

    #[test]
    fn an_active_but_not_running_unit_shows_its_sub_state() {
        // A oneshot that completed is `active/exited`, which is fine, but
        // calling it "running" would be a lie.
        let rendered = s(unit_badge(&UnitStatus {
            active_state: "active".to_string(),
            sub_state: "exited".to_string(),
            ..Default::default()
        }));
        assert!(rendered.contains("badge ok"));
        assert!(rendered.contains("exited"));
    }

    #[test]
    fn every_target_status_variant_maps_to_a_badge() {
        let reachable = s(target_badge(target::Status::Reachable as i32));
        assert!(reachable.contains("badge ok") && reachable.contains("reachable"));

        let unreachable = s(target_badge(target::Status::Unreachable as i32));
        assert!(unreachable.contains("badge bad") && unreachable.contains("unreachable"));

        for status in [
            target::Status::Unknown as i32,
            target::Status::Unspecified as i32,
            // An enum value from a newer server than this build.
            9_999,
        ] {
            let rendered = s(target_badge(status));
            assert!(
                rendered.contains("class=\"badge\""),
                "{status} should be neutral"
            );
            assert!(rendered.contains("unknown"));
        }
    }

    #[test]
    fn every_deployment_status_variant_maps_to_a_badge() {
        use deployment::Status as S;

        let cases: [(S, &str, &str); 9] = [
            (S::Queued, "badge info", "queued"),
            (S::Building, "badge info", "building"),
            (S::Uploading, "badge info", "uploading"),
            (S::Activating, "badge info", "activating"),
            (S::HealthChecking, "badge info", "health_checking"),
            (S::Succeeded, "badge ok", "succeeded"),
            (S::Failed, "badge bad", "failed"),
            (S::RolledBack, "badge warn", "rolled back"),
            (S::Cancelled, "class=\"badge\"", "cancelled"),
        ];

        for (status, class, label) in cases {
            let rendered = s(deployment_badge(status as i32));
            assert!(
                rendered.contains(class),
                "{label} wanted {class}: {rendered}"
            );
            assert!(rendered.contains(label), "{label} missing: {rendered}");
        }

        // Unspecified and unknown wire values fall back rather than panicking.
        assert!(s(deployment_badge(0)).contains("unspecified"));
        assert!(s(deployment_badge(9_999)).contains("unspecified"));
    }

    // -- empty states ------------------------------------------------------

    #[test]
    fn an_empty_state_carries_its_next_action() {
        let rendered = s(empty_state(
            "Nothing",
            "Add one.",
            Some(("Add target", "/targets/new")),
        ));
        assert!(rendered.contains("class=\"empty\""));
        assert!(rendered.contains("href=\"/targets/new\""));
        assert!(rendered.contains("Add target"));
    }

    #[test]
    fn an_empty_state_without_an_action_renders_no_link() {
        let rendered = s(empty_state("Nothing", "Nothing to do.", None));
        assert!(!rendered.contains("<a "));
    }

    #[test]
    fn every_empty_listing_offers_the_action_that_fills_it() {
        let cases = [
            ("targets", s(targets_list(&[])), "/targets/new"),
            (
                "services",
                s(services_list(&[], &[], &HashMap::new())),
                "/services/new",
            ),
            ("deployments", s(deployments_list(&[], &[])), "/services"),
            (
                "dashboard",
                s(dashboard(&[], &[], &HashMap::new(), &[])),
                "/targets/new",
            ),
        ];
        for (what, rendered, href) in cases {
            // What matters is that an empty page offers the next action, not
            // which component carries it — the dashboard's comes from the
            // first-run checklist rather than an `.empty` card.
            assert!(
                rendered.contains("class=\"empty\"") || rendered.contains("class=\"steps\""),
                "{what} says it is empty without offering a way to fill it"
            );
            assert!(
                rendered.contains(&format!("href=\"{href}\"")),
                "{what} does not link to {href}"
            );
        }
    }

    #[test]
    fn a_target_with_no_services_is_offered_one_scoped_to_it() {
        let rendered = s(target_detail(&a_target(), &[], &HashMap::new(), None));
        assert!(rendered.contains("/services/new?target=tgt_1"));
    }

    // -- SSE fragments -----------------------------------------------------

    #[test]
    fn the_deployment_fragment_is_lines_only_with_no_wrapper() {
        // Appended into #deploy-log on every event: a wrapper would nest a new
        // log box each time.
        let now = chrono::Utc::now();
        let rendered = s(deployment_log_lines(&[(
            now,
            false,
            "compiling".to_string(),
        )]));
        assert!(rendered.starts_with("<div class=\"line\">"), "{rendered}");
        assert!(!rendered.contains("class=\"logs"));
        assert!(!rendered.contains("id=\"deploy-log\""));
    }

    #[test]
    fn deployment_output_marks_stderr_and_step_markers() {
        let now = chrono::Utc::now();
        let rendered = s(deployment_log_lines(&[
            (now, false, "compiling bot v0.1.0".to_string()),
            (now, true, "warning: unused import".to_string()),
            (now, false, "--- uploading artifact".to_string()),
            // A step marker on stderr is still a step marker.
            (now, true, "--- restarting unit".to_string()),
        ]));

        let lines: Vec<&str> = rendered.matches("<div class=\"line").collect();
        assert_eq!(lines.len(), 4);
        assert!(rendered.contains("<div class=\"line\"><span class=\"at\""));
        assert!(rendered.contains("class=\"line err\""));
        assert_eq!(rendered.matches("class=\"line cmd\"").count(), 2);
    }

    #[test]
    fn deployment_output_containing_markup_is_escaped() {
        // Build output is arbitrary text from a compiler or a remote shell.
        let now = chrono::Utc::now();
        let rendered = s(deployment_log_lines(&[(
            now,
            true,
            "error: <script>alert('x')</script> & <img onerror=y>".to_string(),
        )]));

        assert!(!rendered.contains("<script>"));
        assert!(!rendered.contains("<img"));
        assert!(rendered.contains("&lt;script&gt;"));
        assert!(rendered.contains("&amp;"));
    }

    #[test]
    fn an_empty_deployment_fragment_says_it_is_waiting() {
        let rendered = s(deployment_log_lines(&[]));
        assert!(rendered.contains("placeholder"));
        assert!(rendered.contains("Waiting for output"));
    }

    #[test]
    fn a_live_deployment_subscribes_and_a_finished_one_does_not() {
        let service = a_service();
        let mut deployment = Deployment {
            id: "dep_1".to_string(),
            service_id: "svc_1".to_string(),
            status: deployment::Status::Building as i32,
            started_at: Some(nudo_proto::to_timestamp(chrono::Utc::now())),
            ..Default::default()
        };

        let live = s(deployment_detail(&deployment, &service, &[], true, "tok"));
        assert!(live.contains("hx-ext=\"sse\""));
        assert!(live.contains("sse-connect=\"/deployments/dep_1/stream\""));
        assert!(live.contains("id=\"deploy-log\""));
        assert!(live.contains("sse-swap=\"log\""));
        // Cancellable while running, and it names the consequence.
        assert!(live.contains("Cancel"));
        assert!(live.contains("confirm('Cancel this deployment?"));

        deployment.status = deployment::Status::Succeeded as i32;
        deployment.finished_at = Some(nudo_proto::to_timestamp(chrono::Utc::now()));
        let done = s(deployment_detail(&deployment, &service, &[], false, "tok"));
        assert!(!done.contains("sse-connect"), "nothing to subscribe to");
        assert!(!done.contains(">Cancel<"), "and nothing to cancel");
        assert!(done.contains("id=\"deploy-log\""), "the pane still exists");
    }

    #[test]
    fn a_failed_deployment_shows_its_whole_error() {
        let deployment = Deployment {
            id: "dep_1".to_string(),
            service_id: "svc_1".to_string(),
            status: deployment::Status::Failed as i32,
            error: "health check failed after 3 retries\nlast body: 503".to_string(),
            ..Default::default()
        };
        let rendered = s(deployment_detail(
            &deployment,
            &a_service(),
            &[],
            false,
            "t",
        ));
        assert!(rendered.contains("health check failed after 3 retries"));
        assert!(rendered.contains("last body: 503"), "not truncated here");
    }

    #[test]
    fn the_log_fragment_is_lines_only_with_no_wrapper() {
        let rendered = s(log_lines(&[LogLine {
            at: Some(nudo_proto::to_timestamp(chrono::Utc::now())),
            message: "started".to_string(),
            priority: "6".to_string(),
            ..Default::default()
        }]));
        assert!(rendered.starts_with("<div class=\"line\">"), "{rendered}");
        assert!(!rendered.contains("class=\"logs"));
        assert!(!rendered.contains("id=\"log-pane\""));
    }

    #[test]
    fn journald_priorities_map_to_line_classes() {
        let with = |priority: &str| {
            s(log_lines(&[LogLine {
                message: "m".to_string(),
                priority: priority.to_string(),
                ..Default::default()
            }]))
        };

        // 0 emerg, 1 alert, 2 crit, 3 err.
        for priority in ["0", "1", "2", "3"] {
            assert!(
                with(priority).contains("class=\"line err\""),
                "priority {priority} should be an error"
            );
        }
        assert!(with("4").contains("class=\"line warn\""));
        // 5 notice, 6 info, 7 debug and anything unparseable are ordinary.
        for priority in ["5", "6", "7", "", "not-a-number"] {
            assert!(
                with(priority).contains("class=\"line\""),
                "priority {priority:?} should be ordinary"
            );
        }
    }

    #[test]
    fn log_text_containing_markup_is_escaped() {
        // A service that logs a request body can log anything at all.
        let rendered = s(log_lines(&[LogLine {
            message: "GET /?q=<script>alert(1)</script> \"quoted\" & more".to_string(),
            priority: "3".to_string(),
            ..Default::default()
        }]));

        assert!(!rendered.contains("<script>"));
        assert!(rendered.contains("&lt;script&gt;alert(1)&lt;/script&gt;"));
        assert!(rendered.contains("&amp;"));
        assert!(rendered.contains("class=\"line err\""));
    }

    #[test]
    fn a_log_line_with_no_timestamp_still_renders() {
        let rendered = s(log_lines(&[LogLine {
            message: "no clock".to_string(),
            ..Default::default()
        }]));
        assert!(rendered.contains("--:--:--"));
        assert!(rendered.contains("no clock"));
    }

    #[test]
    fn an_empty_log_says_no_matches_rather_than_nothing() {
        let rendered = s(log_lines(&[]));
        assert!(rendered.contains("No matching log lines"));
    }

    #[test]
    fn following_logs_subscribes_and_carries_the_filter_into_the_stream() {
        let following = s(logs_view(&a_service(), &[], "panic at", true));
        assert!(following.contains("hx-ext=\"sse\""));
        assert!(following.contains("/services/svc_1/logs/stream?grep=panic%20at"));
        assert!(following.contains("id=\"log-pane\""));
        assert!(following.contains("sse-swap=\"log\""));
        assert!(following.contains("Stop following"));

        let static_view = s(logs_view(&a_service(), &[], "", false));
        assert!(!static_view.contains("sse-connect"));
        assert!(static_view.contains("Follow"));
        // The grep box drives the server, not a client-side filter.
        assert!(static_view.contains("hx-get=\"/services/svc_1/logs\""));
        assert!(static_view.contains("hx-target=\"#log-pane\""));
    }

    #[test]
    fn a_grep_value_is_escaped_back_into_its_input() {
        let rendered = s(logs_view(&a_service(), &[], "\"><script>x</script>", false));
        assert!(!rendered.contains("<script>x</script>"));
        assert!(rendered.contains("&lt;script&gt;"));
    }

    // -- terminal ----------------------------------------------------------

    #[test]
    fn the_terminal_page_embeds_the_grant_as_json_and_names_no_host() {
        // The browser gets a session id and a token. It must not learn the
        // address of the machine — the server already knows which target the
        // grant is for.
        let target = a_target();
        let rendered = s(terminal_page(&target, "sess_1", "tok_secret"));

        assert!(
            rendered
                .contains(r#"window.nudoTerminal = {"sessionId":"sess_1","token":"tok_secret"};"#)
        );
        assert!(!rendered.contains("10.0.0.4"), "no host");
        assert!(!rendered.contains(":22"), "no port");
        assert!(!rendered.contains("deploy@"), "no ssh user@host");

        // And the pieces terminal.js needs.
        assert!(rendered.contains("class=\"term-wrap\""));
        assert!(rendered.contains("id=\"terminal\""));
        assert!(rendered.contains("id=\"term-status\""));
        for asset in [
            "/assets/xterm.css",
            "/assets/xterm.js",
            "/assets/xterm-addon-fit.js",
            "/assets/terminal.js",
        ] {
            assert!(rendered.contains(asset), "missing {asset}");
        }
    }

    #[test]
    fn a_token_containing_script_syntax_cannot_break_out_of_the_script_element() {
        // An HTML parser ends a script element at the first literal `</`, which
        // serde_json does not escape, so `terminal_page` rewrites the sequence.
        let rendered = s(terminal_page(
            &a_target(),
            "s",
            "</script><script>alert(1)</script>",
        ));

        assert!(!rendered.contains("</script><script>alert(1)"));
        assert!(rendered.contains(r"<\/script><script>alert(1)<\/script>"));
        // Exactly the four closing tags we wrote: xterm, fit, config,
        // terminal.js. A fifth would mean the token closed one early. A literal
        // `<script` inside the JSON string is harmless — only `</` ends a script
        // element — so only the closing count is asserted.
        assert_eq!(rendered.matches("</script>").count(), 4);
    }

    #[test]
    fn a_terminal_on_a_latency_critical_host_says_what_it_costs() {
        let mut hot = a_target();
        hot.latency_critical = true;
        let rendered = s(terminal_page(&hot, "s", "t"));
        assert!(rendered.contains("badge hot"));
        assert!(rendered.contains("audit log"));
    }

    // -- shell -------------------------------------------------------------

    #[test]
    fn the_page_shell_has_a_head_and_the_three_assets() {
        let rendered = s(page("Overview", Nav::Dashboard, html! { div { "body" } }));
        assert!(rendered.starts_with("<!DOCTYPE html>"));
        assert!(rendered.contains("<meta charset=\"utf-8\">"));
        assert!(rendered.contains("name=\"viewport\""));
        assert!(rendered.contains("<title>Overview · nudo</title>"));
        assert!(rendered.contains("href=\"/assets/app.css\""));
        assert!(rendered.contains("src=\"/assets/htmx.min.js\""));
        assert!(rendered.contains("src=\"/assets/sse.js\""));
        assert!(rendered.contains("class=\"shell\""));
        assert!(rendered.contains("class=\"rail\""));
        assert!(rendered.contains("class=\"main\""));
        assert!(rendered.contains("<div>body</div>"));
    }

    #[test]
    fn a_page_title_is_escaped() {
        let rendered = s(page("</title><script>x</script>", Nav::Dashboard, html! {}));
        assert!(!rendered.contains("<script>x</script>"));
        assert!(rendered.contains("&lt;/title&gt;"));
    }

    #[test]
    fn exactly_one_rail_item_is_active_and_it_is_the_requested_one() {
        for (nav, _, href, _) in Nav::items() {
            let rendered = s(page("t", nav, html! {}));
            assert_eq!(
                rendered.matches("class=\"nav active\"").count(),
                1,
                "{nav:?} should mark exactly one item"
            );
            let active = rendered
                .split("class=\"nav active\"")
                .nth(1)
                .expect("the active item");
            assert!(
                active.starts_with(&format!(" href=\"{href}\"")),
                "{nav:?} marked the wrong item: {}",
                &active[..active.len().min(60)]
            );
        }
    }

    #[test]
    fn tabs_and_submenus_mark_the_active_item() {
        let rendered = s(tabs(&[("One", "/one", false), ("Two", "/two", true)]));
        assert!(
            rendered.contains("class=\"\" href=\"/one\">One</a>"),
            "{rendered}"
        );
        assert!(rendered.contains("class=\"active\" href=\"/two\""));

        let rendered = s(submenu(&[("A", "/a", true), ("B", "/b", false)]));
        assert!(rendered.contains("class=\"submenu\""));
        assert!(rendered.contains("class=\"active\" href=\"/a\""));
    }

    #[test]
    fn a_topbar_omits_the_subtitle_when_there_is_none() {
        let with = s(topbar("T", Some("sub"), html! {}));
        assert!(with.contains("class=\"subtitle\">sub<"));

        let without = s(topbar("T", None, html! {}));
        assert!(!without.contains("subtitle"));
        assert!(without.contains("<h1>T</h1>"));
    }

    // -- destructive actions -----------------------------------------------

    #[test]
    fn every_destructive_button_confirms_and_names_the_consequence() {
        let target = a_target();
        let service = a_service();
        let secret = Secret {
            id: "sec_1".to_string(),
            name: "API_KEY".to_string(),
            ..Default::default()
        };
        let source = Source {
            id: "src_1".to_string(),
            name: "app".to_string(),
            ..Default::default()
        };
        let release = Release {
            id: "rel_1".to_string(),
            service_id: "svc_1".to_string(),
            ..Default::default()
        };
        let token_view = TokenView {
            id: "tok_1".to_string(),
            name: "laptop".to_string(),
            scopes: "admin".to_string(),
            last_used: None,
            revoked: false,
            created: chrono::Utc::now(),
        };

        let screens = [
            (
                "target_form(edit)",
                s(target_form(
                    Some(&target),
                    std::slice::from_ref(&secret),
                    "t",
                )),
            ),
            (
                "service_form(edit)",
                s(service_form(
                    Some(&service),
                    std::slice::from_ref(&target),
                    &[],
                    &[],
                    "t",
                )),
            ),
            (
                "service_detail",
                s(service_detail(
                    &service,
                    &target,
                    &running(),
                    &[release],
                    &[],
                    "t",
                )),
            ),
            ("secrets_list", s(secrets_list(&[secret], &[], &[], "t"))),
            ("sources_list", s(sources_list(&[source], "t"))),
            (
                "settings_page",
                s(settings_page(
                    &[token_view],
                    "a@b.c",
                    &SettingsPrefs::default(),
                    "t",
                )),
            ),
        ];

        for (what, rendered) in screens {
            for chunk in rendered.split("btn small danger").skip(1) {
                let tag = chunk.split('>').next().unwrap_or(chunk);
                assert!(
                    tag.contains("onclick"),
                    "{what}: danger button without confirm"
                );
            }
            for chunk in rendered.split("class=\"btn danger\"").skip(1) {
                let tag = chunk.split('>').next().unwrap_or(chunk);
                assert!(
                    tag.contains("onclick"),
                    "{what}: danger button without confirm"
                );
            }
            assert!(
                rendered.contains("return confirm("),
                "{what} has a destructive form and no confirm at all"
            );
        }
    }

    #[test]
    fn a_confirm_message_with_a_quote_in_the_name_stays_one_javascript_string() {
        // A service named `bo't` would otherwise close the JS literal.
        let mut service = a_service();
        service.name = "bo't".to_string();
        let release = Release {
            id: "rel_1".to_string(),
            service_id: "svc_1".to_string(),
            ..Default::default()
        };
        let rendered = s(service_detail(
            &service,
            &a_target(),
            &running(),
            &[release],
            &[],
            "t",
        ));
        // maud escapes the attribute for HTML but leaves `'` alone, since the
        // attribute is double-quoted. `js_text` is what keeps the apostrophe
        // from closing the JavaScript literal inside it.
        assert!(
            rendered.contains("confirm('Roll bo\\'t back to release rel_1?"),
            "the apostrophe must be backslash-escaped: {rendered}"
        );
    }

    #[test]
    fn a_running_service_offers_stop_and_a_stopped_one_offers_start() {
        let running_page = s(service_detail(
            &a_service(),
            &a_target(),
            &running(),
            &[],
            &[],
            "t",
        ));
        assert!(running_page.contains(">Stop<"));
        assert!(!running_page.contains(">Start<"));

        let stopped = UnitStatus {
            active_state: "inactive".to_string(),
            sub_state: "dead".to_string(),
            ..Default::default()
        };
        let stopped_page = s(service_detail(
            &a_service(),
            &a_target(),
            &stopped,
            &[],
            &[],
            "t",
        ));
        assert!(stopped_page.contains(">Start<"));
        // Starting a stopped unit is what the operator came for; no confirm.
        assert!(!stopped_page.contains(">Stop<"));
    }

    #[test]
    fn the_current_release_has_no_rollback_button() {
        let releases = [
            Release {
                id: "rel_2".to_string(),
                service_id: "svc_1".to_string(),
                ..Default::default()
            },
            Release {
                id: "rel_1".to_string(),
                service_id: "svc_1".to_string(),
                ..Default::default()
            },
        ];
        // a_service()'s current release is rel_2.
        let rendered = s(service_detail(
            &a_service(),
            &a_target(),
            &running(),
            &releases,
            &[],
            "t",
        ));
        assert_eq!(rendered.matches(">Rollback<").count(), 1);
        assert!(
            rendered.contains("release rel_1"),
            "the confirm names the target release"
        );
        assert!(rendered.contains(">current<"));
    }

    // -- listings and detail -----------------------------------------------

    #[test]
    fn the_dashboard_counts_running_and_failed_units() {
        let services = [
            a_service(),
            Service {
                id: "svc_2".to_string(),
                target_id: "tgt_1".to_string(),
                name: "sidecar".to_string(),
                ..Default::default()
            },
            Service {
                id: "svc_3".to_string(),
                target_id: "tgt_1".to_string(),
                name: "idle".to_string(),
                ..Default::default()
            },
        ];
        let mut statuses = HashMap::new();
        statuses.insert("svc_1".to_string(), running());
        statuses.insert(
            "svc_2".to_string(),
            UnitStatus {
                active_state: "failed".to_string(),
                sub_state: "failed".to_string(),
                ..Default::default()
            },
        );
        // svc_3 has no status: not counted either way.

        let rendered = s(dashboard(&[a_target()], &services, &statuses, &[]));
        assert!(rendered.contains("class=\"stats\""));
        // The failed count is the only one coloured, because it is the only one
        // that means someone has to do something.
        assert!(rendered.contains("class=\"stat is-bad\""));
        assert!(rendered.contains("<div class=\"stat-value\">1</div>"));
        assert!(
            rendered.contains("<div class=\"stat-value\">3</div>"),
            "3 services"
        );
        // A target with a failed unit is flagged on its tile.
        assert!(rendered.contains("class=\"tile alert\""));
        assert!(rendered.contains("1 failed"));
    }

    #[test]
    fn an_unreachable_target_tile_is_flagged() {
        let mut target = a_target();
        target.status = target::Status::Unreachable as i32;
        let rendered = s(dashboard(&[target], &[], &HashMap::new(), &[]));
        assert!(rendered.contains("class=\"tile alert\""));
        assert!(rendered.contains("unreachable"));
    }

    #[test]
    fn a_reachable_target_tile_is_not_flagged() {
        let rendered = s(dashboard(&[a_target()], &[], &HashMap::new(), &[]));
        assert!(rendered.contains("class=\"tile\""));
        assert!(!rendered.contains("tile alert"));
    }

    #[test]
    fn the_targets_listing_shows_the_address_and_calls_agentless_out() {
        let mut with_agent = a_target();
        with_agent.agent_version = "0.3.1".to_string();
        with_agent
            .labels
            .insert("env".to_string(), "prod".to_string());
        with_agent
            .labels
            .insert("role".to_string(), "bot".to_string());

        let rendered = s(targets_list(&[a_target(), with_agent]));
        assert!(rendered.contains("deploy@10.0.0.4:22"));
        // Agentless is a supported mode, not a missing field.
        assert!(rendered.contains("agentless"));
        assert!(rendered.contains("0.3.1"));
        // Labels render in a stable order regardless of map iteration.
        assert!(rendered.contains("env=prod, role=bot"));
    }

    #[test]
    fn target_detail_shows_the_key_reference_and_the_check_results() {
        let checks = CheckTargetResponse {
            ok: false,
            checks: vec![
                check_target_response::Check {
                    name: "ssh".to_string(),
                    ok: true,
                    detail: "connected in 42ms".to_string(),
                },
                check_target_response::Check {
                    name: "sudo".to_string(),
                    ok: false,
                    detail: "deploy is not in sudoers".to_string(),
                },
            ],
        };
        let rendered = s(target_detail(
            &a_target(),
            &[],
            &HashMap::new(),
            Some(&checks),
        ));

        assert!(rendered.contains("Preflight checks"));
        assert!(rendered.contains("problems found"));
        assert!(rendered.contains("deploy is not in sudoers"));
        // The key is a reference into the store.
        assert!(rendered.contains("sec_key"));
        assert!(rendered.contains("SSH key"));
    }

    #[test]
    fn target_detail_omits_the_check_card_when_no_check_has_run() {
        let rendered = s(target_detail(&a_target(), &[], &HashMap::new(), None));
        assert!(!rendered.contains("Preflight checks"));
    }

    #[test]
    fn a_passing_check_set_reads_as_passing() {
        let checks = CheckTargetResponse {
            ok: true,
            checks: vec![check_target_response::Check {
                name: "systemd".to_string(),
                ok: true,
                detail: String::new(),
            }],
        };
        let rendered = s(target_detail(
            &a_target(),
            &[],
            &HashMap::new(),
            Some(&checks),
        ));
        assert!(rendered.contains("all passed"));
    }

    #[test]
    fn the_services_listing_names_the_target_and_the_source() {
        let mut statuses = HashMap::new();
        statuses.insert("svc_1".to_string(), running());
        let rendered = s(services_list(&[a_service()], &[a_target()], &statuses));

        assert!(
            rendered.contains("hft-box"),
            "the target's name, not its id"
        );
        assert!(rendered.contains("git:owner/bot@main"));
        assert!(rendered.contains("bot.service"));
        assert!(rendered.contains("64.0 MiB"));
        assert!(rendered.contains("badge ok"));
    }

    #[test]
    fn a_service_with_no_reported_status_says_so_rather_than_guessing() {
        let rendered = s(services_list(
            &[a_service()],
            &[a_target()],
            &HashMap::new(),
        ));
        assert!(rendered.contains("no data"));
    }

    #[test]
    fn a_never_deployed_service_says_so_rather_than_showing_an_empty_cell() {
        let mut service = a_service();
        service.current_release_id = String::new();
        let rendered = s(services_list(&[service], &[a_target()], &HashMap::new()));
        assert!(rendered.contains("never deployed"));
    }

    #[test]
    fn a_service_with_no_health_check_says_it_will_not_roll_back() {
        let mut service = a_service();
        service.health_check = None;
        let rendered = s(service_detail(
            &service,
            &a_target(),
            &running(),
            &[],
            &[],
            "t",
        ));
        assert!(rendered.contains("never rolled back automatically"));
    }

    #[test]
    fn each_health_check_kind_is_described() {
        let with = |kind: Option<health_check::Kind>| {
            let mut service = a_service();
            service.health_check = Some(HealthCheck {
                kind,
                timeout_seconds: 5,
                retries: 2,
                initial_delay_seconds: 1,
            });
            s(service_detail(
                &service,
                &a_target(),
                &running(),
                &[],
                &[],
                "t",
            ))
        };

        assert!(with(Some(health_check::Kind::HttpUrl("http://x/z".to_string()))).contains("GET "));
        assert!(
            with(Some(health_check::Kind::Command("/bin/check".to_string()))).contains("command ")
        );
        assert!(with(Some(health_check::Kind::SystemdActive(true))).contains("is-active only"));
    }

    #[test]
    fn each_artifact_kind_is_described_on_the_detail_page() {
        let with = |kind: artifact_source::Kind| {
            let mut service = a_service();
            service.artifact = Some(ArtifactSource { kind: Some(kind) });
            s(service_detail(
                &service,
                &a_target(),
                &running(),
                &[],
                &[],
                "t",
            ))
        };

        assert!(
            with(artifact_source::Kind::Url("https://x/bot".to_string())).contains("https://x/bot")
        );
        assert!(with(artifact_source::Kind::DirectUpload(true)).contains("pushed by the CLI"));

        let git = with(artifact_source::Kind::Git(GitSource {
            repo: "owner/bot".to_string(),
            branch: "main".to_string(),
            build_command: "cargo build".to_string(),
            auto_deploy_on_push: true,
            ..Default::default()
        }));
        assert!(git.contains("owner/bot@main"));
        assert!(git.contains("cargo build"));
        assert!(git.contains("auto-deploy on push"));
    }

    #[test]
    fn a_failed_unit_gets_a_callout_not_just_a_badge() {
        let failed = UnitStatus {
            active_state: "failed".to_string(),
            sub_state: "failed".to_string(),
            restart_count: 7,
            ..Default::default()
        };
        let rendered = s(service_detail(
            &a_service(),
            &a_target(),
            &failed,
            &[],
            &[],
            "t",
        ));
        assert!(rendered.contains("callout bad"));
        assert!(rendered.contains("Unit is failed"));
    }

    #[test]
    fn the_unit_preview_says_it_is_a_preview() {
        let unit_file = "[Unit]\nDescription=bot\n\n[Service]\nExecStart=/opt/bot/current/bot\n";
        let rendered = s(service_unit(&a_service(), unit_file));
        assert!(rendered.contains("class=\"unit\""));
        assert!(rendered.contains("ExecStart=/opt/bot/current/bot"));
        assert!(rendered.contains("This is a preview"));
        assert!(rendered.contains("/etc/systemd/system/bot.service"));
    }

    #[test]
    fn a_unit_file_containing_markup_is_escaped() {
        let rendered = s(service_unit(&a_service(), "ExecStart=/bin/x --html '<b>'"));
        assert!(rendered.contains("&lt;b&gt;"));
        assert!(!rendered.contains("<b>'"));
    }

    #[test]
    fn the_service_form_carries_every_systemd_and_latency_field() {
        let rendered = s(service_form(None, &[a_target()], &[], &[], "t"));
        for field in [
            "name=\"name\"",
            "name=\"target_id\"",
            "name=\"release_root\"",
            "name=\"keep_releases\"",
            "name=\"unit_name\"",
            "name=\"description\"",
            "name=\"exec_args\"",
            "name=\"working_directory\"",
            "name=\"unit_user\"",
            "name=\"unit_group\"",
            "name=\"restart\"",
            "name=\"restart_sec\"",
            "name=\"after\"",
            "name=\"cpu_affinity\"",
            "name=\"nice\"",
            "name=\"io_scheduling_class\"",
            "name=\"extra_directives\"",
            "name=\"env\"",
            "name=\"check_kind\"",
            "name=\"check_http_url\"",
            "name=\"check_command\"",
            "name=\"check_timeout\"",
            "name=\"check_retries\"",
            "name=\"check_initial_delay\"",
            "name=\"artifact_kind\"",
            "name=\"artifact_url\"",
            "name=\"source_id\"",
            "name=\"repo\"",
            "name=\"branch\"",
            "name=\"build_command\"",
            "name=\"artifact_path\"",
            "name=\"auto_deploy_on_push\"",
        ] {
            assert!(
                rendered.contains(field),
                "the service form is missing {field}"
            );
        }
    }

    #[test]
    fn editing_a_service_preselects_its_existing_configuration() {
        let mut service = a_service();
        service.unit = Some(SystemdUnit {
            unit_name: "bot.service".to_string(),
            cpu_affinity: "2-5".to_string(),
            io_scheduling_class: "realtime".to_string(),
            restart: "on-failure".to_string(),
            after: vec![
                "network-online.target".to_string(),
                "redis.service".to_string(),
            ],
            extra_directives: HashMap::from([
                ("LimitNOFILE".to_string(), "65535".to_string()),
                ("LimitMEMLOCK".to_string(), "infinity".to_string()),
            ]),
            ..Default::default()
        });
        service.env = HashMap::from([("RUST_LOG".to_string(), "info".to_string())]);

        let rendered = s(service_form(Some(&service), &[a_target()], &[], &[], "t"));
        assert!(rendered.contains("value=\"2-5\""));
        assert!(rendered.contains("value=\"realtime\" selected"));
        assert!(rendered.contains("value=\"on-failure\" selected"));
        assert!(rendered.contains("value=\"network-online.target,redis.service\""));
        // Extra directives in a stable order, one per line.
        assert!(rendered.contains("LimitMEMLOCK=infinity\nLimitNOFILE=65535"));
        assert!(rendered.contains("RUST_LOG=info"));
        // The existing target is preselected.
        assert!(rendered.contains("value=\"tgt_1\" selected"));
    }

    #[test]
    fn editing_a_target_preselects_its_key_and_flag() {
        let mut target = a_target();
        target.latency_critical = true;
        target.labels.insert("env".to_string(), "prod".to_string());
        let secret = Secret {
            id: "sec_key".to_string(),
            name: "deploy-key".to_string(),
            ..Default::default()
        };

        let rendered = s(target_form(Some(&target), &[secret], "t"));
        assert!(rendered.contains("value=\"sec_key\" selected"));
        assert!(rendered.contains("name=\"latency_critical\" value=\"1\" checked"));
        assert!(rendered.contains("value=\"env=prod\""));
        assert!(rendered.contains("Save target"));
        // Only the edit form offers deletion.
        assert!(rendered.contains("Delete target"));
        assert!(!s(target_form(None, &[], "t")).contains("Delete target"));
    }

    #[test]
    fn the_new_target_form_defaults_the_ssh_port_to_22() {
        let rendered = s(target_form(None, &[], "t"));
        assert!(rendered.contains("name=\"port\" min=\"1\" max=\"65535\" value=\"22\""));
    }

    #[test]
    fn the_deployment_listing_names_the_service_and_the_actor_kind() {
        let deployments = [
            Deployment {
                id: "dep_1".to_string(),
                service_id: "svc_1".to_string(),
                status: deployment::Status::Succeeded as i32,
                actor: Some(Actor::agent("sess_9", "claude")),
                started_at: Some(nudo_proto::to_timestamp(
                    chrono::Utc::now() - chrono::Duration::minutes(5),
                )),
                finished_at: Some(nudo_proto::to_timestamp(
                    chrono::Utc::now() - chrono::Duration::minutes(4),
                )),
                ..Default::default()
            },
            Deployment {
                id: "dep_2".to_string(),
                service_id: "svc_1".to_string(),
                status: deployment::Status::Failed as i32,
                error: "compile error:\n".to_string() + &"x".repeat(200),
                ..Default::default()
            },
        ];
        let rendered = s(deployments_list(&deployments, &[a_service()]));

        assert!(rendered.contains("bot"), "the service name, not its id");
        assert!(rendered.contains("claude"));
        assert!(rendered.contains("agent"));
        assert!(rendered.contains("succeeded"));
        // The long error is truncated to keep the row one line tall.
        assert!(rendered.contains('…'));
        assert!(!rendered.contains(&"x".repeat(200)));
    }

    #[test]
    fn a_deployment_for_an_unknown_service_falls_back_to_the_id() {
        // The service list handed in may not contain every referenced service.
        let deployments = [Deployment {
            id: "dep_1".to_string(),
            service_id: "svc_gone".to_string(),
            status: deployment::Status::Succeeded as i32,
            ..Default::default()
        }];
        let rendered = s(deployments_list(&deployments, &[]));
        assert!(rendered.contains("svc_gone"));
    }

    // -- upgrading ---------------------------------------------------------

    fn an_upgrade(install: UpgradeInstall) -> UpgradeView {
        UpgradeView {
            current: "0.1.0".to_string(),
            latest: "0.2.0".to_string(),
            available: true,
            breaking: false,
            install,
        }
    }

    #[test]
    fn the_banner_points_at_the_instructions() {
        let rendered = s(update_banner(&an_update(true, false)));
        assert!(rendered.contains(r#"href="/upgrade""#));
    }

    #[test]
    fn a_containerised_instance_is_told_to_pull_an_image() {
        let rendered = s(upgrade_page(&an_upgrade(UpgradeInstall::Container {
            image: "ghcr.io/loa212/nudo",
        })));
        assert!(rendered.contains("docker pull ghcr.io/loa212/nudo:0.2.0"));
        assert!(rendered.contains("docker compose pull"));
        // Instructions for the other kind would be actively misleading here.
        assert!(
            !rendered.contains("systemctl stop nudo"),
            "a container was told to restart a systemd unit"
        );
    }

    #[test]
    fn a_binary_instance_is_told_to_verify_what_it_downloaded() {
        let rendered = s(upgrade_page(&an_upgrade(UpgradeInstall::Binary)));
        assert!(rendered.contains("sha256sum -c"), "no checksum step");
        assert!(rendered.contains("systemctl stop nudo"));
        assert!(rendered.contains("systemctl start nudo"));
        assert!(
            !rendered.contains("docker pull"),
            "a host install was told to pull an image"
        );
    }

    #[test]
    fn the_page_answers_whether_upgrading_loses_anything() {
        // The first question anyone has. Both install kinds must answer it.
        for install in [
            UpgradeInstall::Container {
                image: "ghcr.io/loa212/nudo",
            },
            UpgradeInstall::Binary,
        ] {
            let rendered = s(upgrade_page(&an_upgrade(install)));
            assert!(
                rendered.contains("Your data is not touched"),
                "the page does not say whether data survives"
            );
            // The generated-key trap, which is the one way someone can actually
            // lose something.
            assert!(rendered.contains("secret key"));
            // And what to do when it goes wrong.
            assert!(rendered.contains("If it goes wrong"));
        }
    }

    #[test]
    fn a_breaking_release_is_called_out_on_the_upgrade_page_too() {
        let mut view = an_upgrade(UpgradeInstall::Binary);
        view.breaking = true;
        let rendered = s(upgrade_page(&view));
        assert!(rendered.contains("needs manual steps"));
    }

    #[test]
    fn an_up_to_date_instance_is_not_told_to_pull_the_version_it_runs() {
        // `docker pull ...:0.1.0` while running 0.1.0 is a no-op dressed up as
        // an instruction.
        let mut view = an_upgrade(UpgradeInstall::Container {
            image: "ghcr.io/loa212/nudo",
        });
        view.available = false;
        let rendered = s(upgrade_page(&view));
        assert!(rendered.contains("docker pull ghcr.io/loa212/nudo:latest"));
        assert!(!rendered.contains(":0.1.0"));

        // The binary snippet becomes an illustration rather than something to
        // paste, and says which version to substitute.
        let mut binary = an_upgrade(UpgradeInstall::Binary);
        binary.available = false;
        let rendered = s(upgrade_page(&binary));
        assert!(rendered.contains("version=X.Y.Z"));
        assert!(!rendered.contains("version=latest"));
    }

    #[test]
    fn an_up_to_date_instance_still_gets_the_instructions() {
        // Reached from a bookmark or the nav rather than the banner. Saying
        // "nothing to do" and hiding the steps would be a dead end.
        let mut view = an_upgrade(UpgradeInstall::Binary);
        view.available = false;
        let rendered = s(upgrade_page(&view));
        assert!(rendered.contains("You are up to date"));
        assert!(rendered.contains("sha256sum -c"), "the steps are hidden");
    }

    #[test]
    fn the_upgrade_page_never_offers_to_do_it_for_you() {
        // nudo holds the SSH keys for every machine it manages. A button here
        // that ran an upgrade would be a much larger thing to trust than a page
        // that tells you what to type, and this test is what keeps it a page.
        for install in [
            UpgradeInstall::Container {
                image: "ghcr.io/loa212/nudo",
            },
            UpgradeInstall::Binary,
        ] {
            let rendered = s(upgrade_page(&an_upgrade(install)));
            assert!(
                !rendered.contains("<form"),
                "the upgrade page has a form, so something is being submitted"
            );
            assert!(
                !rendered.contains("curl -fsSL") && !rendered.contains("| sh"),
                "the page pipes a downloaded script into a shell"
            );
        }
    }

    // -- the first-run checklist ------------------------------------------

    fn a_finished_deployment() -> Deployment {
        Deployment {
            id: "dep_1".to_string(),
            service_id: "svc_1".to_string(),
            status: deployment::Status::Succeeded as i32,
            ..Default::default()
        }
    }

    #[test]
    fn a_brand_new_instance_is_told_what_the_first_step_is() {
        // Four zeroes and an empty table say nothing about what to do. The
        // checklist names the sequence, so the order is visible rather than
        // inferred.
        let rendered = s(dashboard(&[], &[], &HashMap::new(), &[]));
        assert!(rendered.contains("Getting to a first deploy"));
        assert!(rendered.contains("0 of 3 done"));
        // Exactly one thing to click: the step being asked for.
        assert!(rendered.contains(r#"href="/targets/new""#));
    }

    #[test]
    fn the_checklist_advances_as_each_step_is_finished() {
        let with_target = s(dashboard(&[a_target()], &[], &HashMap::new(), &[]));
        assert!(with_target.contains("1 of 3 done"));
        assert!(
            with_target.contains(r#"href="/services/new""#),
            "having a target, the next ask is a service"
        );

        let with_both = s(dashboard(
            &[a_target()],
            &[a_service()],
            &HashMap::new(),
            &[],
        ));
        assert!(with_both.contains("2 of 3 done"));
    }

    #[test]
    fn the_checklist_stops_appearing_once_something_has_been_deployed() {
        // A checklist that outlives its usefulness becomes furniture.
        let rendered = s(dashboard(
            &[a_target()],
            &[a_service()],
            &HashMap::new(),
            &[a_finished_deployment()],
        ));
        assert!(!rendered.contains("Getting to a first deploy"));
    }

    #[test]
    fn only_the_current_step_offers_a_button() {
        // Three buttons at once is three decisions; the point of the list is
        // that there is one thing to do next.
        let rendered = s(dashboard(&[], &[], &HashMap::new(), &[]));
        let checklist = rendered
            .split("Getting to a first deploy")
            .nth(1)
            .expect("the checklist is rendered")
            .split("</div></div>")
            .next()
            .unwrap_or_default();
        assert_eq!(
            checklist.matches("btn small primary").count(),
            1,
            "more than one step is asking to be clicked"
        );
    }

    // -- sources, audit, settings, auth -----------------------------------

    #[test]
    fn the_sources_page_offers_the_manifest_flow_and_the_existing_app_hint() {
        let rendered = s(sources_list(&[], "t"));
        assert!(rendered.contains("Create a GitHub App"));
        assert!(rendered.contains("name=\"name\""));
        assert!(rendered.contains("name=\"organization\""));
        assert!(rendered.contains("action=\"/sources/github\""));
        assert!(rendered.contains("Already have an App?"));
        // No place to paste a private key: GitHub hands the credentials back.
        assert!(!rendered.to_lowercase().contains("private key"));
    }

    #[test]
    fn an_uninstalled_source_is_a_warning_because_it_cannot_clone() {
        let source = Source {
            id: "src_1".to_string(),
            name: "nudo-deploy".to_string(),
            kind: source::Kind::GithubApp as i32,
            app_slug: "nudo-deploy".to_string(),
            account_login: "acme".to_string(),
            installed: false,
            ..Default::default()
        };
        let rendered = s(sources_list(std::slice::from_ref(&source), "t"));
        assert!(rendered.contains("badge warn"));
        assert!(rendered.contains("not installed"));
        assert!(rendered.contains("github_app"));
        assert!(rendered.contains("acme"));

        let installed = Source {
            installed: true,
            ..source
        };
        let rendered = s(sources_list(&[installed], "t"));
        assert!(rendered.contains("badge ok"));
    }

    #[test]
    fn the_github_handoff_posts_the_manifest_to_github_and_escapes_it() {
        let manifest = r#"{"name":"nudo","url":"https://x/</textarea>"}"#;
        let rendered = s(github_handoff(
            "https://github.com/settings/apps/new?state=abc",
            manifest,
        ));

        assert!(rendered.contains("action=\"https://github.com/settings/apps/new?state=abc\""));
        assert!(rendered.contains("name=\"manifest\""));
        // The manifest is data, not markup: a `</textarea>` inside it must not
        // close the element.
        assert!(!rendered.contains("</textarea>\"}"));
        assert!(rendered.contains("&lt;/textarea&gt;"));
        // It posts to GitHub, so it carries no token of ours; there is nothing
        // of ours to forge.
        assert!(!rendered.contains("name=\"csrf\""));
        assert!(
            rendered.contains("Create the App on GitHub"),
            "and a manual fallback"
        );
    }

    #[test]
    fn a_created_token_is_shown_once_with_that_said_and_not_in_an_input() {
        let rendered = s(token_created("laptop-cli", "nudo_pat_abc123"));

        assert!(rendered.contains("nudo_pat_abc123"));
        assert!(rendered.contains("Copy this now"));
        assert!(rendered.contains("cannot be shown again"));
        // Not an input: a browser restoring the form on a back-navigation would
        // re-send the value.
        assert!(!rendered.contains("<input"));
        assert!(rendered.contains("class=\"unit\""));
    }

    #[test]
    fn a_token_value_containing_markup_is_escaped() {
        let rendered = s(token_created("x", "<script>alert(1)</script>"));
        assert!(!rendered.contains("<script>alert(1)"));
        assert!(rendered.contains("&lt;script&gt;"));
    }

    #[test]
    fn the_audit_log_distinguishes_actor_kinds_and_marks_dry_runs() {
        let entries = [
            AuditEntry {
                id: "aud_1".to_string(),
                at: Some(nudo_proto::to_timestamp(chrono::Utc::now())),
                actor: Some(Actor::human("usr_1", "alice")),
                action: "Deployments.Deploy".to_string(),
                subject_id: "svc_1".to_string(),
                dry_run: false,
                summary: "deployed bot to hft-box".to_string(),
            },
            AuditEntry {
                id: "aud_2".to_string(),
                at: Some(nudo_proto::to_timestamp(chrono::Utc::now())),
                actor: Some(Actor::agent("sess_1", "claude")),
                action: "Deployments.Deploy".to_string(),
                subject_id: "svc_1".to_string(),
                dry_run: true,
                summary: "would deploy bot".to_string(),
            },
        ];
        let rendered = s(audit_list(&entries));

        assert!(rendered.contains("alice"));
        assert!(rendered.contains("claude"));
        assert!(rendered.contains("Deployments.Deploy"));
        // A dry run changed nothing and must not read like a real change.
        assert!(rendered.contains("dry run"));
        assert!(rendered.contains("applied"));
    }

    #[test]
    fn a_refused_action_is_coloured_like_a_failure() {
        // A refusal is the latency-critical guardrail working, and it is what
        // someone reading the audit log is looking for.
        let entries = [AuditEntry {
            id: "aud_1".to_string(),
            at: Some(nudo_proto::to_timestamp(chrono::Utc::now())),
            actor: Some(Actor::agent("sess_1", "claude")),
            action: "Deployments.Deploy refused: latency_critical".to_string(),
            subject_id: "svc_1".to_string(),
            dry_run: false,
            summary: "allow_latency_critical was not set".to_string(),
        }];
        let rendered = s(audit_list(&entries));
        assert!(rendered.contains("class=\"badge bad\""));
        assert!(rendered.contains("refused"));
    }

    #[test]
    fn an_audit_entry_with_no_actor_still_renders() {
        let entries = [AuditEntry {
            id: "aud_1".to_string(),
            action: "Secrets.Put".to_string(),
            ..Default::default()
        }];
        let rendered = s(audit_list(&entries));
        assert!(rendered.contains("Secrets.Put"));
        assert!(rendered.contains("unknown"));
    }

    #[test]
    fn an_audit_summary_is_truncated_and_escaped() {
        let entries = [AuditEntry {
            id: "aud_1".to_string(),
            action: "Targets.Update".to_string(),
            summary: format!("<b>{}</b>", "y".repeat(200)),
            ..Default::default()
        }];
        let rendered = s(audit_list(&entries));
        assert!(!rendered.contains("<b>"));
        assert!(rendered.contains("&lt;b&gt;"));
        assert!(!rendered.contains(&"y".repeat(200)));
    }

    #[test]
    fn settings_shows_token_state_and_never_a_token_secret() {
        let tokens = [
            TokenView {
                id: "tok_1".to_string(),
                name: "laptop".to_string(),
                scopes: "deploy".to_string(),
                last_used: Some(chrono::Utc::now() - chrono::Duration::hours(3)),
                revoked: false,
                created: chrono::Utc::now() - chrono::Duration::days(9),
            },
            TokenView {
                id: "tok_2".to_string(),
                name: "old-ci".to_string(),
                scopes: "admin".to_string(),
                last_used: None,
                revoked: true,
                created: chrono::Utc::now() - chrono::Duration::days(400),
            },
        ];
        let rendered = s(settings_page(
            &tokens,
            "alice@example.com",
            &SettingsPrefs::default(),
            "t",
        ));

        assert!(rendered.contains("alice@example.com"));
        assert!(rendered.contains("3h ago"));
        // Never used is a reason to revoke, so it is stated.
        assert!(rendered.contains("never"));
        assert!(rendered.contains("badge ok"));
        assert!(rendered.contains("badge bad"));
        // A revoked token has nothing left to revoke.
        assert_eq!(rendered.matches(">Revoke<").count(), 1);
        // The TokenView type has no secret field, so nothing can leak one.
        assert!(!rendered.to_lowercase().contains("nudo_pat_"));
    }

    #[test]
    fn the_auth_pages_are_standalone_documents_with_no_rail() {
        for rendered in [s(login_page(None, "t")), s(setup_page(None, "t"))] {
            assert!(rendered.starts_with("<!DOCTYPE html>"));
            assert!(rendered.contains("class=\"auth-page\""));
            assert!(rendered.contains("class=\"auth-card\""));
            // No navigation for someone who is not signed in.
            assert!(!rendered.contains("class=\"rail\""));
            assert!(!rendered.contains("class=\"nav"));
        }
    }

    #[test]
    fn an_auth_error_is_shown_as_a_callout_and_escaped() {
        let rendered = s(login_page(Some("Invalid <email> or password"), "t"));
        assert!(rendered.contains("callout bad"));
        assert!(rendered.contains("&lt;email&gt;"));
        assert!(!rendered.contains("<email>"));

        assert!(!s(login_page(None, "t")).contains("callout bad"));
    }

    #[test]
    fn the_setup_page_says_what_the_first_account_controls() {
        let rendered = s(setup_page(None, "t"));
        assert!(rendered.contains("controls every target"));
        assert!(rendered.contains("name=\"password_confirm\""));
    }

    #[test]
    fn an_error_page_states_the_code_without_leaking_internals() {
        let rendered = s(error_page(502, "The control plane is not responding."));
        assert!(rendered.contains("502"));
        assert!(rendered.contains("The control plane is not responding."));
        assert!(rendered.contains("href=\"/\""));
        assert!(rendered.contains("class=\"auth-page\""));
    }

    #[test]
    fn an_error_message_is_escaped() {
        let rendered = s(error_page(500, "<script>alert(1)</script>"));
        assert!(!rendered.contains("<script>alert(1)"));
        assert!(rendered.contains("&lt;script&gt;"));
    }

    // -- formatting helpers ------------------------------------------------

    #[test]
    fn relative_times_read_naturally_at_each_scale() {
        let now = chrono::Utc::now();
        let at = |seconds: i64| nudo_proto::to_timestamp(now - chrono::Duration::seconds(seconds));

        assert_eq!(ago(Some(&at(5))), "just now");
        assert_eq!(ago(Some(&at(59))), "just now");
        assert_eq!(ago(Some(&at(120))), "2m ago");
        assert_eq!(ago(Some(&at(7_200))), "2h ago");
        assert_eq!(ago(Some(&at(172_800))), "2d ago");
        assert_eq!(ago(None), "-");
        // Clock skew must not print "in -3s".
        assert_eq!(
            ago(Some(&nudo_proto::to_timestamp(
                now + chrono::Duration::hours(1)
            ))),
            "just now"
        );
    }

    #[test]
    fn durations_distinguish_running_from_finished() {
        let start = chrono::Utc::now();
        let started = nudo_proto::to_timestamp(start);
        let finished = nudo_proto::to_timestamp(start + chrono::Duration::seconds(95));

        assert_eq!(duration(Some(&started), Some(&finished)), "1m35s");
        assert_eq!(duration(Some(&started), None), "running");
        assert_eq!(duration(None, None), "-");
    }

    #[test]
    fn byte_counts_use_the_largest_readable_unit() {
        assert_eq!(bytes(0), "0 B");
        assert_eq!(bytes(512), "512 B");
        assert_eq!(bytes(2048), "2.0 KiB");
        assert_eq!(bytes(5 * 1024 * 1024), "5.0 MiB");
        assert_eq!(bytes(3 * 1024 * 1024 * 1024), "3.0 GiB");
    }

    #[test]
    fn long_and_multi_line_text_is_reduced_to_one_cell() {
        let rendered = truncate("line one\nline two\nline three", 20);
        assert!(!rendered.contains('\n'));
        assert!(rendered.chars().count() <= 20);
        assert!(rendered.ends_with('…'));

        assert_eq!(truncate("short", 20), "short");
        assert_eq!(truncate("", 20), "-");
        assert_eq!(truncate("   ", 20), "-");
    }

    #[test]
    fn scope_labels_cover_every_combination() {
        let scoped = |target: &str, service: &str| {
            scope_label(&Secret {
                scope_target_id: target.to_string(),
                scope_service_id: service.to_string(),
                ..Default::default()
            })
        };

        assert_eq!(scoped("", ""), "global");
        assert_eq!(scoped("tgt_1", ""), "target tgt_1");
        assert_eq!(scoped("", "svc_1"), "service svc_1");
        // Both set: the narrower scope is the one that decides.
        assert_eq!(scoped("tgt_1", "svc_1"), "service svc_1");
    }

    #[test]
    fn a_digest_is_only_ever_shown_as_a_prefix() {
        assert_eq!(digest_prefix("0123456789abcdef0123"), "0123456789ab");
        assert_eq!(digest_prefix(""), "-");
    }

    #[test]
    fn artifact_summaries_cover_every_kind() {
        let with = |kind: artifact_source::Kind| {
            artifact_summary(&Service {
                artifact: Some(ArtifactSource { kind: Some(kind) }),
                ..Default::default()
            })
        };

        assert_eq!(
            with(artifact_source::Kind::Url("https://x/bot".to_string())),
            "url"
        );
        assert_eq!(
            with(artifact_source::Kind::Git(GitSource {
                repo: "owner/bot".to_string(),
                branch: "main".to_string(),
                ..Default::default()
            })),
            "git:owner/bot@main"
        );
        assert_eq!(
            with(artifact_source::Kind::Git(GitSource {
                repo: "owner/bot".to_string(),
                ..Default::default()
            })),
            "git:owner/bot"
        );
        assert_eq!(with(artifact_source::Kind::DirectUpload(true)), "upload");
        assert_eq!(artifact_summary(&Service::default()), "upload");
        // An empty url is not a configured url.
        assert_eq!(with(artifact_source::Kind::Url(String::new())), "upload");
    }

    #[test]
    fn a_short_sha_is_eight_characters_and_a_missing_one_is_a_dash() {
        assert_eq!(short_sha("0123456789abcdef"), "01234567");
        assert_eq!(short_sha(""), "-");
    }

    #[test]
    fn a_javascript_string_is_escaped_for_both_layers() {
        assert_eq!(js_text("it's"), "it\\'s");
        assert_eq!(js_text("back\\slash"), "back\\\\slash");
        // Double quotes are left to maud's attribute escaping.
        assert_eq!(js_text("say \"hi\""), "say \"hi\"");
    }

    #[test]
    fn maps_render_in_a_stable_order_so_a_page_does_not_churn() {
        let map = HashMap::from([
            ("z".to_string(), "1".to_string()),
            ("a".to_string(), "2".to_string()),
            ("m".to_string(), "3".to_string()),
        ]);
        assert_eq!(labels_line(&map), "a=2, m=3, z=1");
        assert_eq!(labels_input(&map), "a=2,m=3,z=1");
        assert_eq!(env_line(&map), "a=2 m=3 z=1");
        assert_eq!(directives_text(&map), "a=2\nm=3\nz=1");
        assert_eq!(labels_line(&HashMap::new()), "-");
    }

    #[test]
    fn a_missing_string_field_renders_as_a_dash() {
        assert_eq!(or_dash(""), "-");
        assert_eq!(or_dash("   "), "-");
        assert_eq!(or_dash("value"), "value");
    }

    #[test]
    fn latency_knobs_are_detected_from_any_one_of_them() {
        let with = |unit: SystemdUnit| {
            has_latency_knobs(&Service {
                unit: Some(unit),
                ..Default::default()
            })
        };

        assert!(with(SystemdUnit {
            cpu_affinity: "0-3".to_string(),
            ..Default::default()
        }));
        assert!(with(SystemdUnit {
            nice: "-5".to_string(),
            ..Default::default()
        }));
        assert!(with(SystemdUnit {
            io_scheduling_class: "realtime".to_string(),
            ..Default::default()
        }));
        assert!(!with(SystemdUnit::default()));
        assert!(!has_latency_knobs(&Service::default()));
    }

    #[test]
    fn wide_tables_are_wrapped_so_the_page_does_not_scroll_sideways() {
        let screens = [
            s(targets_list(&[a_target()])),
            s(services_list(
                &[a_service()],
                &[a_target()],
                &HashMap::new(),
            )),
            s(deployments_list(
                &[Deployment {
                    id: "d".to_string(),
                    ..Default::default()
                }],
                &[a_service()],
            )),
            s(audit_list(&[AuditEntry {
                id: "a".to_string(),
                ..Default::default()
            }])),
            s(secrets_list(
                &[Secret {
                    id: "s".to_string(),
                    ..Default::default()
                }],
                &[],
                &[],
                "t",
            )),
        ];
        for rendered in screens {
            assert!(rendered.contains("class=\"table-scroll\""));
        }
    }

    // -----------------------------------------------------------------------
    // Updates, the changelog and the support banner
    // -----------------------------------------------------------------------

    fn an_update(available: bool, breaking: bool) -> UpdateBanner {
        UpdateBanner {
            current: "0.1.0".to_string(),
            latest: "0.2.0".to_string(),
            available,
            breaking,
            url: "https://github.com/loa212/nudo/releases/tag/v0.2.0".to_string(),
        }
    }

    #[test]
    fn a_current_instance_is_shown_no_update_banner_at_all() {
        // Not a hidden element or an empty box — nothing, so the dashboard of
        // someone who is up to date looks exactly as it did before.
        assert_eq!(s(update_banner(&an_update(false, false))), "");
    }

    #[test]
    fn the_update_banner_names_both_versions_and_links_to_the_notes() {
        let rendered = s(update_banner(&an_update(true, false)));
        assert!(rendered.contains("0.2.0"), "the new version is not named");
        assert!(
            rendered.contains("0.1.0"),
            "the running version is not named"
        );
        assert!(rendered.contains("/changelog"));
        assert!(rendered.contains("releases/tag/v0.2.0"));
    }

    #[test]
    fn the_update_banner_never_offers_to_perform_the_upgrade() {
        // Coolify's equivalent runs a downloaded script as root. nudo's banner
        // only ever links: `/upgrade` is a page of instructions, and the test
        // below asserts that page has no form and pipes nothing into a shell.
        // What is forbidden here is anything that would *act* — a POST, or a
        // command embedded in the banner itself.
        let rendered = s(update_banner(&an_update(true, true)));
        assert!(
            !rendered.contains("<form"),
            "the banner submits something, so it does more than link"
        );
        for forbidden in ["curl", "install.sh", "| sh"] {
            assert!(
                !rendered.contains(forbidden),
                "the banner contains {forbidden}, which would run code on the host"
            );
        }
    }

    #[test]
    fn a_breaking_release_says_so_before_anyone_upgrades() {
        let rendered = s(update_banner(&an_update(true, true)));
        assert!(rendered.contains("manual steps"));
        assert!(
            rendered.contains("callout bad"),
            "it is not styled as a warning"
        );
    }

    #[test]
    fn release_notes_from_the_manifest_cannot_inject_markup() {
        // The manifest is fetched over the network, so its notes are untrusted.
        let entry = ChangelogEntry {
            version: "9.9.9".to_string(),
            notes: "- <script>alert(1)</script>\n- <img src=x onerror=alert(1)>".to_string(),
            ..ChangelogEntry::default()
        };
        let rendered = s(changelog_page(&[entry], "0.1.0"));
        // Both are escaped, so they render as visible text rather than as
        // elements. `onerror=` still appears in the output — as the characters
        // of an escaped `<img ...>`, inside a `<li>`, where it is inert.
        assert!(!rendered.contains("<script>"), "a script tag survived");
        assert!(!rendered.contains("<img"), "an img tag survived");
        assert!(rendered.contains("&lt;script&gt;"));
        assert!(rendered.contains("&lt;img"));
    }

    #[test]
    fn release_notes_render_bullets_as_a_list_and_prose_as_paragraphs() {
        let entry = ChangelogEntry {
            version: "0.2.0".to_string(),
            notes: "## Fixed\n\n- One thing\n- Another thing\n\nA closing note.".to_string(),
            ..ChangelogEntry::default()
        };
        let rendered = s(changelog_page(&[entry], "0.1.0"));
        assert!(rendered.contains("<li>One thing</li>"));
        assert!(rendered.contains("<li>Another thing</li>"));
        assert!(rendered.contains("<p>A closing note.</p>"));
        // The heading keeps its text but loses its hashes.
        assert!(rendered.contains("Fixed"));
        assert!(!rendered.contains("## Fixed"));
    }

    #[test]
    fn the_changelog_marks_the_version_this_instance_is_running() {
        let entries = [
            ChangelogEntry {
                version: "0.2.0".to_string(),
                ..ChangelogEntry::default()
            },
            ChangelogEntry {
                version: "0.1.0".to_string(),
                current: true,
                ..ChangelogEntry::default()
            },
        ];
        let rendered = s(changelog_page(&entries, "0.1.0"));
        assert!(rendered.contains("running"));
    }

    #[test]
    fn an_instance_that_has_never_checked_gets_an_explanation_not_an_error() {
        let rendered = s(changelog_page(&[], "0.1.0"));
        assert!(rendered.contains("No release notes yet"));
        // Nothing is wrong with the instance, and the page should say so.
        assert!(rendered.contains("Nothing is wrong"));
    }

    fn support_test_links() -> SupportLinkView<'static> {
        SupportLinkView {
            sponsor: "https://github.com/sponsors/loa212",
            repository: "https://github.com/loa212/nudo",
            issues: "https://github.com/loa212/nudo/issues/new/choose",
            discussions: "https://github.com/loa212/nudo/discussions",
        }
    }

    #[test]
    fn the_support_banner_offers_a_way_out_that_is_not_a_purchase() {
        // Someone who cannot or will not sponsor should still find something
        // useful to do, and a dismissal that is one click away.
        let rendered = s(support_banner("tok", support_test_links()));
        assert!(rendered.contains("Sponsor"));
        assert!(rendered.contains("Star on GitHub"));
        assert!(rendered.contains("Report a bug"));
        assert!(rendered.contains("Maybe next time"));
        assert!(rendered.contains("/support/dismiss"));
    }

    #[test]
    fn dismissing_the_support_banner_is_a_csrf_protected_post() {
        // A GET would let any page on the internet dismiss it for the user.
        let rendered = s(support_banner("tok_abc", support_test_links()));
        assert!(rendered.contains(r#"method="post""#));
        assert!(rendered.contains(r#"value="tok_abc""#));
    }

    #[test]
    fn the_settings_page_carries_both_switches_and_says_nothing_is_sent() {
        let prefs = SettingsPrefs {
            update_check_enabled: true,
            support_prompt_enabled: false,
            last_checked: "2 hours ago".to_string(),
        };
        let rendered = s(settings_page(&[], "a@b.c", &prefs, "t"));
        assert!(rendered.contains("/settings/updates"));
        assert!(rendered.contains("/settings/support"));
        assert!(rendered.contains("2 hours ago"));
        // The claim that matters most on that page.
        assert!(rendered.contains("no usage"));
    }

    #[test]
    fn an_unticked_switch_renders_unticked() {
        // Rendering a stored `false` as a ticked box would silently turn the
        // setting back on the next time anyone saved the form.
        let off = SettingsPrefs::default();
        let rendered = s(settings_page(&[], "a@b.c", &off, "t"));
        let updates_form = rendered
            .split(r#"action="/settings/updates""#)
            .nth(1)
            .expect("the updates form is on the page");
        let form_body = updates_form.split("</form>").next().expect("a closed form");
        assert!(
            !form_body.contains("checked"),
            "a disabled release check renders as ticked"
        );
    }
}

// ---------------------------------------------------------------------------
// Updates and the changelog
//
// The banner and the "What's new" page. Both render data the control plane has
// already fetched — this module never makes a request of its own.
// ---------------------------------------------------------------------------

/// The banner shown when a newer release exists.
///
/// Renders nothing when the instance is current, so the caller can place it
/// unconditionally.
///
/// Deliberately not an "Update now" button. Coolify's equivalent downloads a
/// shell script over the network and runs it as root; for a tool holding every
/// target's SSH keys, that is a lot of trust in a URL, so upgrading here stays a
/// deliberate act on the host.
pub fn update_banner(status: &UpdateBanner) -> Markup {
    if !status.available {
        return html! {};
    }

    html! {
        div class={ "callout " @if status.breaking { "bad" } @else { "info" } } .update-banner {
            strong {
                "nudo " (status.latest) " is out"
                @if status.breaking { " — it needs manual steps" }
            }
            p {
                "You are running " (status.current) ". "
                @if status.breaking {
                    "Read the notes before upgrading: this release changes something \
                     that will not migrate itself."
                } @else {
                    "Upgrading is a manual step on the host; nudo does not update itself."
                }
            }
            div .form-actions {
                a .btn.small.primary href="/upgrade" { "How to upgrade" }
                a .btn.small href="/changelog" { "What's new" }
                @if !status.url.is_empty() {
                    a .btn.small href=(status.url) target="_blank" rel="noreferrer noopener" {
                        "Release notes"
                    }
                }
            }
        }
    }
}

/// The upgrade instructions, for the way this instance is actually installed.
///
/// A page rather than a button. nudo does not upgrade itself: it holds the SSH
/// keys for every machine it manages, and a process that can rewrite its own
/// binary — or fetch and run a script as root, which is how the tool this was
/// modelled on does it — is a much larger thing to trust than one that tells
/// you what to type.
///
/// The commands are exact and the reasoning is stated, because the questions
/// someone actually has at this point are "will this lose my data" and "what if
/// it goes wrong".
pub fn upgrade_page(view: &UpgradeView) -> Markup {
    html! {
        (topbar(
            "Upgrading nudo",
            Some(&format!("running {}", view.current)),
            html! { a .btn href="/changelog" { "What's new" } },
        ))
        div .content {
            @if view.available {
                (callout("info", &format!("nudo {} is available", view.latest), html! {
                    p { "You are running " (view.current) "." }
                }))
            } @else {
                (callout("info", "You are up to date", html! {
                    p {
                        "Nothing to do — these are the steps for when there is. "
                        "The version here is " (view.current) "."
                    }
                }))
            }

            @if view.breaking {
                (callout("bad", "This release needs manual steps", html! {
                    p {
                        "Read the release notes before starting. Something in this \
                         release does not migrate itself."
                    }
                }))
            }

            div .card {
                h2 { "Your data is not touched by any of this" }
                div .card-body {
                    p {
                        "Upgrading replaces executables. Everything nudo remembers \
                         lives outside them and is left exactly as it is:"
                    }
                    ul {
                        li { "the database — targets, services, deployment history, sessions" }
                        li { "the data directory — build workspaces and uploaded artifacts" }
                        li { "your configuration — environment variables or the systemd unit" }
                    }
                    p {
                        "Schema changes are applied automatically the first time the \
                         new version opens the database, so there is no migration \
                         step to run by hand."
                    }
                    (callout("warn", "The one thing worth checking first", html! {
                        p {
                            "If you never set a secret key, nudo generated one into the \
                             data directory and warned you at startup. It is still there \
                             and still works — but every stored secret is unreadable \
                             without it, so back it up before doing anything that could \
                             remove that directory."
                        }
                    }))
                }
            }

            // The tag to pull: the new version when there is one, otherwise
            // `latest` — pulling the version already running is a no-op, and
            // printing it as an instruction is just confusing.
            @let tag = if view.available { view.latest.as_str() } else { "latest" };

            @match view.install {
                UpgradeInstall::Container { image } => (container_upgrade(image, tag)),
                UpgradeInstall::Binary => (binary_upgrade(tag)),
            }

            div .card {
                h2 { "If it goes wrong" }
                div .card-body {
                    p {
                        "Run the previous version again — it is the same command with \
                         the older tag, or the binaries you moved aside. The database \
                         is compatible in the direction you have already come from, so \
                         going back works as long as the older version has seen that \
                         schema before."
                    }
                    p .small.muted {
                        "Downgrading across a release marked as needing manual steps is \
                         the exception: check its notes, which say what changed."
                    }
                }
            }
        }
    }
}

/// Upgrade steps for a containerised install.
fn container_upgrade(image: &str, latest: &str) -> Markup {
    let pull = format!("docker pull {image}:{latest}");
    html! {
        div .card {
            h2 { "This instance is running in a container" }
            div .card-body {
                p {
                    "Upgrading means pulling the new image and recreating the \
                     container. The state volume is not part of the image, so \
                     recreating the container keeps everything."
                }
                pre .code {
                    (pull) "\n"
                    "docker stop nudo\n"
                    "docker rm nudo\n"
                    "# then run it again with your usual flags, using the new tag"
                }
                p .small.muted {
                    "Using compose instead: " code { "docker compose pull" } " then "
                    code { "docker compose up -d" } " — which does the same thing and \
                     keeps your flags where you already wrote them down."
                }
                (callout("warn", "Check for the volume before you remove anything", html! {
                    p {
                        "Recreating the container is only safe because the database \
                         lives on a volume. If you started nudo without one, its state \
                         is inside the container and removing it destroys that state. "
                        code { "docker inspect -f '{{ .Mounts }}' nudo" }
                        " says which you have."
                    }
                }))
            }
        }
    }
}

/// Upgrade steps for a binary install.
///
/// `version` is the release to fetch, or `latest` when the instance is already
/// current — in which case the snippet is an illustration rather than something
/// to paste, and says so.
fn binary_upgrade(version: &str) -> Markup {
    let is_placeholder = version == "latest";
    html! {
        div .card {
            h2 { "This instance is running as a binary on the host" }
            div .card-body {
                p {
                    "Download the release archive, verify it, and replace the \
                     binaries. Nothing under the data directory is touched."
                }
                pre .code {
                    @if is_placeholder {
                        "version=X.Y.Z   # the release you are upgrading to\n"
                    } @else {
                        "version=" (version) "\n"
                    }
                    r#"target=x86_64-unknown-linux-musl   # or -gnu"# "\n"
                    r#"base=https://github.com/loa212/nudo/releases/download/v$version"# "\n"
                    "\n"
                    r#"curl -fLO "$base/nudo-v$version-$target.tar.gz""# "\n"
                    r#"curl -fLO "$base/nudo-v$version-$target.tar.gz.sha256""# "\n"
                    r#"sha256sum -c "nudo-v$version-$target.tar.gz.sha256""# "\n"
                    "\n"
                    r#"tar -xzf "nudo-v$version-$target.tar.gz""# "\n"
                    r#"sudo systemctl stop nudo"# "\n"
                    r#"sudo install "nudo-v$version-$target"/nudo* /usr/local/bin/"# "\n"
                    r#"sudo systemctl start nudo"# "\n"
                }
                p {
                    "The checksum step is not optional decoration: it is what \
                     distinguishes the release you meant to install from whatever \
                     the network handed you."
                }
                p .small.muted {
                    "Keep the old binaries until the new version has started and you \
                     have loaded a page — " code { "sudo cp /usr/local/bin/nudo-all-in-one /tmp/nudo.previous" }
                    " before installing makes going back a single command."
                }
            }
        }
    }
}

/// What the upgrade page needs.
#[derive(Debug, Clone)]
pub struct UpgradeView {
    pub current: String,
    pub latest: String,
    pub available: bool,
    pub breaking: bool,
    pub install: UpgradeInstall,
}

/// How this instance is installed, and anything the instructions need with it.
#[derive(Debug, Clone)]
pub enum UpgradeInstall {
    Container { image: &'static str },
    Binary,
}

/// What the banner needs to render, flattened out of the control plane's
/// `UpdateStatus` so this module does not depend on the server crate.
#[derive(Debug, Clone, Default)]
pub struct UpdateBanner {
    pub current: String,
    pub latest: String,
    pub available: bool,
    pub breaking: bool,
    pub url: String,
}

/// One entry on the changelog page.
#[derive(Debug, Clone, Default)]
pub struct ChangelogEntry {
    pub version: String,
    pub published_at: String,
    pub notes: String,
    pub url: String,
    pub breaking: bool,
    /// Whether this is the version currently running.
    pub current: bool,
}

/// The "What's new" page: every release the manifest knows about, newest first.
pub fn changelog_page(entries: &[ChangelogEntry], current_version: &str) -> Markup {
    html! {
        (topbar("What's new", Some(&format!("running {current_version}")), html! {}))
        div .content {
            @if entries.is_empty() {
                (empty_state(
                    "No release notes yet",
                    "The release check has not run, could not reach the manifest, or is \
                     turned off. Nothing is wrong with this instance — it just does not \
                     know what else has been published.",
                    Some(("Settings", "/settings")),
                ))
            } @else {
                @for entry in entries {
                    div .card {
                        div .card-head {
                            h2 {
                                (entry.version)
                                @if entry.current {
                                    " " (badge("running", BadgeKind::Ok))
                                }
                                @if entry.breaking {
                                    " " (badge("manual steps", BadgeKind::Bad))
                                }
                            }
                            @if !entry.published_at.is_empty() {
                                span .small.muted { (entry.published_at) }
                            }
                        }
                        div .card-body {
                            @if entry.notes.is_empty() {
                                p .muted { "No notes for this release." }
                            } @else {
                                (release_notes(&entry.notes))
                            }
                            @if !entry.url.is_empty() {
                                p {
                                    a href=(entry.url) target="_blank" rel="noreferrer noopener" {
                                        "Full notes"
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

/// Renders release notes.
///
/// Notes come from a manifest fetched over the network, so they are untrusted
/// input. Rather than run them through a Markdown library and then have to trust
/// its HTML sanitiser, this handles the two things release notes actually use —
/// bullets and paragraphs — and renders everything else as escaped text. Maud
/// escapes each line, so no markup in the manifest can reach the page.
fn release_notes(notes: &str) -> Markup {
    let mut blocks: Vec<NoteBlock> = Vec::new();

    for line in notes.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }

        if let Some(item) = line
            .strip_prefix("- ")
            .or_else(|| line.strip_prefix("* "))
            .or_else(|| line.strip_prefix("+ "))
        {
            match blocks.last_mut() {
                Some(NoteBlock::List(items)) => items.push(item.to_string()),
                _ => blocks.push(NoteBlock::List(vec![item.to_string()])),
            }
            continue;
        }

        // A heading keeps its text but not its level: the page already has a
        // hierarchy and notes should not introduce a competing one.
        let text = line.trim_start_matches('#').trim();
        blocks.push(NoteBlock::Paragraph(text.to_string()));
    }

    html! {
        @for block in &blocks {
            @match block {
                NoteBlock::Paragraph(text) => p { (text) },
                NoteBlock::List(items) => ul {
                    @for item in items { li { (item) } }
                },
            }
        }
    }
}

enum NoteBlock {
    Paragraph(String),
    List(Vec<String>),
}

// ---------------------------------------------------------------------------
// Supporting the project
// ---------------------------------------------------------------------------

/// The "support this project" banner.
///
/// Shown at most once a calendar month, and only to someone who has actually
/// deployed with it — see `support::should_prompt`. The dismiss button says
/// "Maybe next time" because that is what it does; the permanent off-switch is
/// in settings, where someone looking for it will find it.
pub fn support_banner(csrf: &str, links: SupportLinkView<'_>) -> Markup {
    html! {
        div .callout.info.support-banner {
            strong { "nudo is free, and built by one person" }
            p {
                "If it is saving you the cost of a platform, sponsoring keeps it \
                 maintained. If money is not on the table, a star or a good bug \
                 report genuinely helps too."
            }
            div .form-actions {
                a .btn.small.primary href=(links.sponsor) target="_blank" rel="noreferrer noopener" {
                    "Sponsor"
                }
                a .btn.small href=(links.repository) target="_blank" rel="noreferrer noopener" {
                    "Star on GitHub"
                }
                a .btn.small href=(links.issues) target="_blank" rel="noreferrer noopener" {
                    "Report a bug"
                }
                form method="post" action="/support/dismiss" style="display:inline" {
                    (csrf_input(csrf))
                    button .btn.small.quiet type="submit" { "Maybe next time" }
                }
            }
        }
    }
}

/// The links the support banner points at, passed in so this module does not
/// hard-code URLs in two places.
#[derive(Debug, Clone, Copy)]
pub struct SupportLinkView<'a> {
    pub sponsor: &'a str,
    pub repository: &'a str,
    pub issues: &'a str,
    pub discussions: &'a str,
}
