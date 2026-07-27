use super::*;

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
