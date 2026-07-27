use super::*;

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

            link rel="stylesheet" href=(crate::assets::url("xterm.css"));
            div .term-wrap {
                div #terminal {}
            }
            div #term-status .term-status { "connecting…" }
            p .small.faint {
                "The session is single-use and expires on its own. Closing this tab \
                 ends it; reconnecting needs a new one."
            }

            script src=(crate::assets::url("xterm.js")) {}
            script src=(crate::assets::url("xterm-addon-fit.js")) {}
            // Set before terminal.js runs, which reads it at load.
            script { (PreEscaped(format!("window.nudoTerminal = {config};"))) }
            script src=(crate::assets::url("terminal.js")) {}
        }
    }
}
