use super::*;

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

                        form method="post" action="/settings/self-upgrade" style="margin-top:12px" {
                            (csrf_input(csrf))
                            div .field {
                                label .check {
                                    input type="checkbox" name="enabled" value="on"
                                        checked[prefs.self_upgrade_enabled];
                                    span {
                                        "Allow this instance to upgrade itself from the "
                                        a href="/upgrade" { "upgrade page" }
                                        ". Only half the permission: the instance must \
                                         also have been started with "
                                        code { "NUDO_ALLOW_SELF_UPGRADE=true" }
                                        ", and it only applies to a binary install \
                                         running from the release layout."
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
    pub self_upgrade_enabled: bool,
    /// Humanised time of the last release check, empty when it has never run.
    pub last_checked: String,
}
