use super::*;

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
