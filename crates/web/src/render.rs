//! Server-rendered HTML for the nudo dashboard.
//!
//! Presentation only. Every function here takes data that has already been
//! fetched and returns `maud::Markup`; nothing in this module talks to axum, to
//! gRPC, or to the filesystem. That split is what makes the whole dashboard
//! testable without a running control plane — the feature-grouped tests render
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
    AuditEntry, CheckTargetResponse, Deployment, HostKey, LogLine, Release, Secret, Service,
    Source, Target, UnitStatus, deployment, source, target,
};

// ---------------------------------------------------------------------------
// Formatting helpers shared by the feature renderers.
// ---------------------------------------------------------------------------

pub use nudo_format::ago_at;
use nudo_format::{ago, artifact_summary, bytes, duration, scope_label, truncate};

mod dashboard;
pub use dashboard::*;
mod targets;
pub use targets::*;
mod services;
pub use services::*;
mod deployments;
pub use deployments::*;
mod logs;
pub use logs::*;
mod secrets;
pub use secrets::*;
mod sources;
pub use sources::*;
mod tokens;
pub use tokens::*;
mod audit;
pub use audit::*;
mod terminal;
pub use terminal::*;
mod settings;
pub use settings::*;
mod auth;
pub use auth::*;
mod updates;
use auth::auth_shell;
use deployments::deployments_table;
use services::js_text;
pub use updates::*;

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
                link rel="stylesheet" href=(crate::assets::url("app.css"));
                script src=(crate::assets::url("htmx.min.js")) defer {}
                script src=(crate::assets::url("sse.js")) defer {}
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
            "A machine reachable over SSH. nudo checks its host key, SSH, sudo, \
             systemd and the release directory before you trust it with anything.",
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
    let kind = match status.active_state.as_str() {
        "active" => BadgeKind::Ok,
        "activating" | "deactivating" | "unknown" => BadgeKind::Warn,
        "failed" => BadgeKind::Bad,
        _ => BadgeKind::Neutral,
    };
    // The compact clients use the canonical collapsed vocabulary. The
    // dashboard has room for systemd's useful active sub-state (`exited`,
    // `start-pre`, ...), so retain that detail rather than changing what an
    // existing screen tells the operator.
    let label = if status.active_state == "active"
        && status.sub_state != "running"
        && !status.sub_state.is_empty()
    {
        status.sub_state.as_str()
    } else {
        nudo_format::unit_state_label(status)
    };
    badge(label, kind)
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

#[cfg(test)]
mod tests;
