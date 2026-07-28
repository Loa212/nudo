use super::*;

// ---------------------------------------------------------------------------
// Secrets
// ---------------------------------------------------------------------------

/// Storing an SSH private key.
///
/// The same store as an environment secret, asked for in the shape it actually
/// has. A key is multi-line, so it needs a textarea rather than a single-line
/// input; it is never an environment variable, so the name is not framed as one;
/// and it is used to open a connection rather than written into an
/// EnvironmentFile, so target and service scope do not apply and are not shown.
///
/// Write-only like everything else here: the textarea has no content and is
/// never populated from a stored value, because a stored value cannot be read
/// back.
fn ssh_key_form(csrf: &str) -> Markup {
    html! {
        // Linked to from the target and build-host forms, which is where an
        // operator discovers they need one.
        form .card id="ssh-key" method="post" action="/secrets/ssh-key" {
            (csrf_input(csrf))
            h2 { "Add an SSH key" }
            p .card-note {
                "The private key nudo uses to reach a target or a build host. \
                 Select it by name when adding one."
            }

            div .field style="margin-top:12px" {
                label for="key_name" { "Name" }
                input type="text" id="key_name" name="name" required
                    placeholder="DEPLOY_KEY" autocomplete="off";
                span .hint { "How you will recognise it in the key list." }
            }

            div .field style="margin-top:12px" {
                label for="key_value" { "Private key" }
                // Deliberately empty on every render, including when the form
                // comes back after an error: this page never holds a value.
                textarea id="key_value" name="value" rows="8" required
                    spellcheck="false" autocomplete="off"
                    placeholder="-----BEGIN OPENSSH PRIVATE KEY-----" {}
                span .hint {
                    "The whole file, including the BEGIN and END lines. Encrypted \
                     immediately and never shown again."
                }
            }

            div .form-actions {
                button .btn.primary type="submit" { "Store SSH key" }
            }

            p .card-note style="margin-top:12px" {
                "Prefer not to paste a key into a browser? "
                code { "nudo secrets set DEPLOY_KEY < ~/.ssh/id_ed25519" }
                " reads from stdin, so it stays out of your shell history too."
            }
        }
    }
}

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
///
/// Two forms, because the store holds one kind of thing but operators write two.
/// A secret is a name and a value either way; an SSH key is not an environment
/// variable, is multi-line, and has no meaningful target or service scope, so
/// asking for it through a single-line field labelled "Becomes the environment
/// variable name" is asking the wrong question.
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
                 target's EnvironmentFile, or used to open an ssh connection. To \
                 change one, write it again — the digest below tells you whether \
                 it actually changed."
            }))

            (ssh_key_form(csrf))

            form .card method="post" action="/secrets" {
                (csrf_input(csrf))
                h2 { "Add or replace an environment secret" }
                p .card-note {
                    "Resolved at deploy time into the unit's EnvironmentFile. \
                     Writing an existing name replaces its value."
                }
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
