use super::*;

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
pub(super) fn auth_shell(title: &str, body: Markup) -> Markup {
    html! {
        (DOCTYPE)
        html lang="en" {
            head {
                meta charset="utf-8";
                meta name="viewport" content="width=device-width, initial-scale=1";
                title { (title) " · nudo" }
                link rel="stylesheet" href=(crate::assets::url("app.css"));
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
