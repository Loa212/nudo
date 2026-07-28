use super::*;

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
