use super::*;

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
