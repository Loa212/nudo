use super::*;

// ---------------------------------------------------------------------------
// Secrets
// ---------------------------------------------------------------------------

/// Something to say at the top of the page after a redirect.
///
/// Resolved from a key rather than carrying text, so a crafted link cannot put
/// arbitrary words in a banner on somebody's own dashboard. An unrecognised key
/// renders nothing at all.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SecretNotice {
    None,
    /// The name is already taken, and this page does not overwrite.
    Taken(String),
    /// A rotation succeeded.
    Rotated,
    /// The pasted key was empty, a public key, the wrong shape, or truncated.
    KeyEmpty,
    KeyPublic,
    KeyShape,
    KeyTruncated,
    /// A rotation arrived with nothing in the field.
    EmptyValue(String),
}

impl SecretNotice {
    pub fn from_key(key: &str, name: &str) -> Self {
        match key {
            "taken" => Self::Taken(name.to_string()),
            "rotated" => Self::Rotated,
            "empty" => Self::EmptyValue(name.to_string()),
            "key-empty" => Self::KeyEmpty,
            "key-public" => Self::KeyPublic,
            "key-shape" => Self::KeyShape,
            "key-truncated" => Self::KeyTruncated,
            _ => Self::None,
        }
    }

    fn render(&self) -> Markup {
        match self {
            Self::None => html! {},
            Self::Taken(name) => callout(
                "bad",
                "That name is already taken",
                html! {
                    "A secret named " span .mono { (name) } " already exists. Its value "
                    "cannot be read back, so writing over it would destroy something "
                    "unrecoverable — nudo will not do that by accident. To replace the "
                    "value, use " strong { "Rotate" } " on its row below. To get rid of "
                    "it entirely, delete it first."
                },
            ),
            Self::Rotated => callout(
                "info",
                "Value replaced",
                html! {
                    "The old value is gone. Any service using this secret picks up the "
                    "new one on its next deploy — until then it is still running with "
                    "the old value."
                },
            ),
            Self::EmptyValue(name) => callout(
                "bad",
                "Nothing to store",
                html! {
                    "The new value for " span .mono { (name) } " was empty. The existing "
                    "value has been left alone."
                },
            ),
            Self::KeyEmpty => callout(
                "bad",
                "No key pasted",
                html! {
                    "Paste the private key into the field."
                },
            ),
            Self::KeyPublic => callout(
                "bad",
                "That is a public key",
                html! {
                    "nudo needs the private half — the file "
                    em { "without" }
                    " the "
                    span .mono { ".pub" }
                    " extension. A public key here would be stored happily and then "
                    "fail every connection that used it."
                },
            ),
            Self::KeyShape => callout(
                "bad",
                "That does not look like a private key",
                html! {
                    "It should start with "
                    span .mono { "-----BEGIN OPENSSH PRIVATE KEY-----" }
                    " or a PEM header."
                },
            ),
            Self::KeyTruncated => callout(
                "bad",
                "The key looks truncated",
                html! {
                    "It should end with an "
                    span .mono { "-----END ...-----" }
                    " line. Copy the whole file, including the first and last lines."
                },
            ),
        }
    }
}

/// The rotate action on a secret's row.
///
/// A `<details>` rather than a modal: it needs no JavaScript, degrades to a
/// visible form with scripting off, and keeps the new value in a field on the
/// page the operator is already looking at. The summary is the button; opening
/// it reveals what rotating actually costs before there is anything to submit.
///
/// The scope is carried in hidden fields rather than the id, because that is
/// what identifies a secret for a write — two secrets can share a name under
/// different scopes, and rotating the wrong one would be the exact accident this
/// whole change exists to prevent.
fn rotate_action(secret: &Secret, csrf: &str) -> Markup {
    html! {
        details .rotate {
            summary .btn.small { "Rotate" }
            form method="post" action="/secrets/rotate" {
                (csrf_input(csrf))
                input type="hidden" name="name" value=(secret.name);
                input type="hidden" name="scope_target_id" value=(secret.scope_target_id);
                input type="hidden" name="scope_service_id" value=(secret.scope_service_id);

                p .card-note {
                    "Replaces the value of "
                    span .mono { (secret.name) }
                    ". The current value cannot be read back and is gone once this "
                    "is saved. Services keep running on the old value until their "
                    "next deploy."
                }

                div .field {
                    label for=(format!("rotate_{}", secret.id)) { "New value" }
                    textarea id=(format!("rotate_{}", secret.id)) name="value" rows="4"
                        required spellcheck="false" autocomplete="off" {}
                }

                div .form-actions {
                    button .btn.small.danger type="submit"
                        onclick=(format!(
                            "return confirm('Replace the value of {}? The current value cannot be recovered.')",
                            js_text(&secret.name)
                        )) {
                        "Replace value"
                    }
                }
            }
        }
    }
}

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
    notice: SecretNotice,
    csrf: &str,
) -> Markup {
    html! {
        (topbar("Secrets", Some("Write-only: values are never returned by the API"), html! {}))
        div .content {
            (notice.render())

            (callout("info", "Values cannot be read back", html! {
                "Once stored, a value is only ever decrypted on the way to a \
                 target's EnvironmentFile, or used to open an ssh connection. \
                 Nothing here overwrites a name that already exists — replacing \
                 a value is a deliberate act, and the digest below is how you \
                 tell whether one actually changed."
            }))

            (ssh_key_form(csrf))

            form .card method="post" action="/secrets" {
                (csrf_input(csrf))
                h2 { "Add an environment secret" }
                p .card-note {
                    "Resolved at deploy time into the unit's EnvironmentFile. \
                     A name that is already taken is refused rather than \
                     overwritten."
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
                                            div .row {
                                                (rotate_action(secret, csrf))
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
}
