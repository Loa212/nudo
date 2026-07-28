use super::*;

/// Create/edit form for a service: artifact, systemd unit, health check.
pub fn service_form(
    existing: Option<&Service>,
    targets: &[Target],
    sources: &[Source],
    secrets: &[Secret],
    build_hosts: &[BuildHost],
    csrf: &str,
) -> Markup {
    use nudo_proto::LOCAL_BUILD_HOST_ID;
    use nudo_proto::artifact_source::Kind as ArtifactKind;
    use nudo_proto::health_check::Kind as CheckKind;

    let editing = existing.is_some();
    let action = match existing {
        Some(s) => format!("/services/{}", s.id),
        None => "/services".to_string(),
    };
    let unit = existing.and_then(|s| s.unit.clone()).unwrap_or_default();
    let check = existing.and_then(|s| s.health_check.clone());
    let artifact = existing
        .and_then(|s| s.artifact.as_ref())
        .and_then(|a| a.kind.as_ref());
    let git = match artifact {
        Some(ArtifactKind::Git(git)) => Some(git.clone()),
        _ => None,
    };
    let artifact_url = match artifact {
        Some(ArtifactKind::Url(url)) => url.clone(),
        _ => String::new(),
    };
    let artifact_kind = match artifact {
        Some(ArtifactKind::Url(_)) => "url",
        Some(ArtifactKind::Git(_)) => "git",
        _ => "upload",
    };
    let check_kind = match check.as_ref().and_then(|c| c.kind.as_ref()) {
        Some(CheckKind::HttpUrl(_)) => "http",
        Some(CheckKind::Command(_)) => "command",
        Some(CheckKind::SystemdActive(_)) => "systemd",
        None => "none",
    };
    let check_http = match check.as_ref().and_then(|c| c.kind.as_ref()) {
        Some(CheckKind::HttpUrl(url)) => url.clone(),
        _ => String::new(),
    };
    let check_command = match check.as_ref().and_then(|c| c.kind.as_ref()) {
        Some(CheckKind::Command(command)) => command.clone(),
        _ => String::new(),
    };

    html! {
        (topbar(
            if editing { "Edit service" } else { "Add service" },
            Some("One systemd unit on one target"),
            html! { a .btn href="/services" { "Cancel" } },
        ))
        div .content {
            form method="post" action=(action) {
                (csrf_input(csrf))

                div .card {
                    h2 { "Identity" }
                    div .fields style="margin-top:12px" {
                        div .field {
                            label for="name" { "Name" }
                            input type="text" id="name" name="name" required
                                placeholder="hft-bot"
                                value=(existing.map(|s| s.name.as_str()).unwrap_or_default());
                        }
                        div .field {
                            label for="target_id" { "Target" }
                            select id="target_id" name="target_id" required {
                                option value="" { "Select a target…" }
                                @for target in targets {
                                    option value=(target.id)
                                        selected[existing.is_some_and(|s| s.target_id == target.id)] {
                                        (target.name)
                                        @if target.latency_critical { " (latency-critical)" }
                                    }
                                }
                            }
                        }
                        div .field {
                            label for="release_root" { "Release root" }
                            input type="text" id="release_root" name="release_root"
                                placeholder="/opt/hft-bot"
                                value=(existing.map(|s| s.release_root.as_str()).unwrap_or_default());
                            span .hint { code { "releases/" } " and the " code { "current" } " symlink live here." }
                        }
                        div .field {
                            label for="keep_releases" { "Keep releases" }
                            input type="number" id="keep_releases" name="keep_releases" min="1" max="50"
                                value=(existing.map(|s| s.keep_releases).filter(|k| *k > 0).unwrap_or(5));
                            span .hint { "How many old releases stay on disk for rollback." }
                        }
                    }
                }

                div .card {
                    h2 { "Artifact" }
                    p .card-note { "Where the binary comes from." }
                    div .field style="margin-top:12px" {
                        label for="artifact_kind" { "Source" }
                        select id="artifact_kind" name="artifact_kind" {
                            option value="upload" selected[artifact_kind == "upload"] { "Pushed by the CLI" }
                            option value="url" selected[artifact_kind == "url"] { "Prebuilt binary at a URL" }
                            option value="git" selected[artifact_kind == "git"] { "Built from a git source" }
                        }
                    }
                    div .field style="margin-top:12px" {
                        label for="artifact_url" { "Artifact URL" }
                        input type="text" id="artifact_url" name="artifact_url"
                            placeholder="https://github.com/owner/repo/releases/download/v1/bot"
                            value=(artifact_url);
                    }
                    div .fields style="margin-top:12px" {
                        div .field {
                            label for="source_id" { "Git source" }
                            select id="source_id" name="git_source_id" {
                                option value="" { "None" }
                                @for source in sources {
                                    option value=(source.id)
                                        selected[git.as_ref().is_some_and(|g| g.source_id == source.id)] {
                                        (source.name)
                                        " (" (source::Kind::try_from(source.kind).unwrap_or(source::Kind::Unspecified).as_str()) ")"
                                    }
                                }
                            }
                            @if sources.is_empty() {
                                span .hint { a href="/sources" { "Connect a GitHub App" } " to build from a repo." }
                            }
                        }
                        div .field {
                            label for="repo" { "Repository" }
                            input type="text" id="repo" name="git_repo" placeholder="owner/name"
                                value=(git.as_ref().map(|g| g.repo.clone()).unwrap_or_default());
                        }
                        div .field {
                            label for="branch" { "Branch" }
                            input type="text" id="branch" name="git_branch" placeholder="main"
                                value=(git.as_ref().map(|g| g.branch.clone()).unwrap_or_default());
                        }
                        div .field {
                            label for="build_command" { "Build command" }
                            input type="text" id="build_command" name="git_build_command"
                                placeholder="cargo build --release"
                                value=(git.as_ref().map(|g| g.build_command.clone()).unwrap_or_default());
                        }
                        div .field {
                            label for="artifact_path" { "Artifact path" }
                            input type="text" id="artifact_path" name="git_artifact_path"
                                placeholder="target/release/bot"
                                value=(git.as_ref().map(|g| g.artifact_path.clone()).unwrap_or_default());
                        }
                        div .field {
                            label for="git_build_host_id" { "Build on" }
                            select id="git_build_host_id" name="git_build_host_id" {
                                @let selected = git
                                    .as_ref()
                                    .map(|g| g.build_host_id.clone())
                                    .unwrap_or_default();
                                option value="" selected[selected.is_empty()] {
                                    "The instance default"
                                }
                                option value=(LOCAL_BUILD_HOST_ID)
                                    selected[selected == LOCAL_BUILD_HOST_ID] {
                                    "The control plane"
                                }
                                @for host in build_hosts {
                                    option value=(host.id) selected[selected == host.id] {
                                        (host.name)
                                        @if host.latency_critical { " (latency-critical)" }
                                    }
                                }
                            }
                            span .hint {
                                @if build_hosts.is_empty() {
                                    a href="/build-hosts" { "Add a build host" }
                                    " to build somewhere other than the control plane."
                                } @else {
                                    "Never the deploy target — a target runs the OS, systemd \
                                     and the binary, and nothing else."
                                }
                            }
                        }
                    }
                    div .check style="margin-top:12px" {
                        input type="checkbox" id="auto_deploy_on_push" name="git_auto_deploy" value="1"
                            checked[git.as_ref().is_some_and(|g| g.auto_deploy_on_push)];
                        label for="auto_deploy_on_push" {
                            "Deploy automatically on push"
                            div .hint {
                                "A push to the branch deploys without anyone watching. \
                                 Refused server-side for latency-critical targets."
                            }
                        }
                    }
                }

                div .card {
                    h2 { "systemd unit" }
                    div .fields style="margin-top:12px" {
                        div .field {
                            label for="unit_name" { "Unit name" }
                            input type="text" id="unit_name" name="unit_name"
                                placeholder="hft-bot.service" value=(unit.unit_name);
                        }
                        div .field {
                            label for="description" { "Description" }
                            input type="text" id="description" name="description"
                                value=(unit.description);
                        }
                        div .field {
                            label for="exec_args" { "Exec args" }
                            input type="text" id="exec_args" name="exec_args"
                                placeholder="--config /etc/bot.toml" value=(unit.exec_args);
                            span .hint { "Appended to the binary in " code { "ExecStart" } "." }
                        }
                        div .field {
                            label for="working_directory" { "Working directory" }
                            input type="text" id="working_directory" name="working_directory"
                                value=(unit.working_directory);
                        }
                        div .field {
                            label for="unit_user" { "User" }
                            input type="text" id="unit_user" name="unit_user" value=(unit.user);
                        }
                        div .field {
                            label for="unit_group" { "Group" }
                            input type="text" id="unit_group" name="unit_group" value=(unit.group);
                        }
                        div .field {
                            label for="restart" { "Restart" }
                            select id="restart" name="restart" {
                                @for option in ["always", "on-failure", "no"] {
                                    option value=(option) selected[unit.restart == option] { (option) }
                                }
                            }
                        }
                        div .field {
                            label for="restart_sec" { "RestartSec" }
                            input type="number" id="restart_sec" name="restart_sec" min="0"
                                value=(unit.restart_sec);
                        }
                        div .field {
                            label for="after" { "After" }
                            input type="text" id="after" name="after"
                                placeholder="network-online.target" value=(unit.after.join(","));
                            span .hint { "Comma-separated units this one is ordered after." }
                        }
                    }
                }

                // Its own card, not a row in the unit fields: these are the
                // knobs the whole tool exists for, and burying them invites
                // someone to leave them empty by accident.
                div .card {
                    div .row {
                        h2 { "Latency knobs" }
                        (badge("hot path", BadgeKind::Hot))
                    }
                    p .card-note {
                        "Written verbatim into the unit. Leave blank on hosts where \
                         the scheduler's defaults are fine."
                    }
                    div .fields style="margin-top:12px" {
                        div .field {
                            label for="cpu_affinity" { "CPUAffinity" }
                            input type="text" id="cpu_affinity" name="cpu_affinity"
                                placeholder="2-5" value=(unit.cpu_affinity);
                            span .hint { "Pins the process to these cores." }
                        }
                        div .field {
                            label for="nice" { "Nice" }
                            input type="text" id="nice" name="nice"
                                placeholder="-10" value=(unit.nice);
                        }
                        div .field {
                            label for="io_scheduling_class" { "IOSchedulingClass" }
                            select id="io_scheduling_class" name="io_scheduling_class" {
                                option value="" selected[unit.io_scheduling_class.is_empty()] { "default" }
                                @for option in ["realtime", "best-effort", "idle"] {
                                    option value=(option) selected[unit.io_scheduling_class == option] { (option) }
                                }
                            }
                        }
                    }
                    div .field style="margin-top:12px" {
                        label for="extra_directives" { "Extra directives" }
                        textarea id="extra_directives" name="extra_directives"
                            placeholder="LimitMEMLOCK=infinity\nLimitNOFILE=65535" {
                            (directives_text(&unit.extra_directives))
                        }
                        span .hint {
                            "One " code { "Key=Value" } " per line, copied into the "
                            code { "[Service]" } " section without validation."
                        }
                    }
                }

                div .card {
                    h2 { "Health check" }
                    p .card-note {
                        "Decides whether a deploy stands. Without one, a broken \
                         release is never rolled back automatically."
                    }
                    div .field style="margin-top:12px" {
                        label for="check_kind" { "Kind" }
                        select id="check_kind" name="check_kind" {
                            option value="none" selected[check_kind == "none"] { "None" }
                            option value="systemd" selected[check_kind == "systemd"] { "systemctl is-active" }
                            option value="http" selected[check_kind == "http"] { "HTTP GET, expect 2xx" }
                            option value="command" selected[check_kind == "command"] { "Command, expect exit 0" }
                        }
                    }
                    div .fields style="margin-top:12px" {
                        div .field {
                            label for="check_http_url" { "HTTP URL" }
                            input type="text" id="check_http_url" name="check_http_url"
                                placeholder="http://127.0.0.1:9000/healthz" value=(check_http);
                        }
                        div .field {
                            label for="check_command" { "Command" }
                            input type="text" id="check_command" name="check_command"
                                placeholder="/opt/bot/current/bot --selftest" value=(check_command);
                        }
                        div .field {
                            label for="check_timeout" { "Timeout (s)" }
                            input type="number" id="check_timeout" name="check_timeout" min="1"
                                value=(check.as_ref().map(|c| c.timeout_seconds).filter(|v| *v > 0).unwrap_or(5));
                        }
                        div .field {
                            label for="check_retries" { "Retries" }
                            input type="number" id="check_retries" name="check_retries" min="0"
                                value=(check.as_ref().map(|c| c.retries).unwrap_or(3));
                        }
                        div .field {
                            label for="check_initial_delay" { "Initial delay (s)" }
                            input type="number" id="check_initial_delay" name="check_initial_delay" min="0"
                                value=(check.as_ref().map(|c| c.initial_delay_seconds).unwrap_or(2));
                        }
                    }
                }

                div .card {
                    h2 { "Environment" }
                    div .field style="margin-top:12px" {
                        label for="env" { "Non-secret variables" }
                        textarea id="env" name="env" placeholder="RUST_LOG=info\nEXCHANGE=binance" {
                            (directives_text(&existing.map(|s| s.env.clone()).unwrap_or_default()))
                        }
                        span .hint { "One " code { "KEY=value" } " per line. Anything sensitive belongs in a secret." }
                    }
                    div .field style="margin-top:14px" {
                        label { "Secrets" }
                        // Checkboxes over ids. A value cannot be entered here,
                        // and none is displayed: secret values are write-only
                        // and are resolved into an EnvironmentFile on the target.
                        @if secrets.is_empty() {
                            p .hint { "No secrets stored yet. " a href="/secrets" { "Add one" } "." }
                        } @else {
                            @for secret in secrets {
                                div .check {
                                    input type="checkbox" id=(format!("secret_{}", secret.id))
                                        name="secret_ids" value=(secret.id)
                                        checked[existing.is_some_and(|s| s.secret_ids.contains(&secret.id))];
                                    label for=(format!("secret_{}", secret.id)) {
                                        span .mono { (secret.name) }
                                        span .sep { " · " }
                                        span .small.muted { (scope_label(secret)) }
                                    }
                                }
                            }
                        }
                        span .hint {
                            "Selected values are written to the unit's EnvironmentFile on \
                             the target at deploy time. They are never sent to this page."
                        }
                    }
                }

                div .form-actions {
                    button .btn.primary type="submit" {
                        @if editing { "Save service" } @else { "Create service" }
                    }
                    a .btn href="/services" { "Cancel" }
                }
            }

            @if editing {
                @let id = existing.map(|s| s.id.as_str()).unwrap_or_default();
                form .card method="post" action=(format!("/services/{id}/delete")) {
                    (csrf_input(csrf))
                    h2 { "Delete service" }
                    p .card-note { "Choose what happens on the target as well as in nudo." }
                    div .check style="margin-top:10px" {
                        input type="checkbox" id="stop_and_disable_unit" name="stop_and_disable_unit" value="1" checked;
                        label for="stop_and_disable_unit" { "Stop and disable the unit on the target" }
                    }
                    div .check style="margin-top:8px" {
                        input type="checkbox" id="remove_release_dir" name="remove_release_dir" value="1";
                        label for="remove_release_dir" {
                            "Delete the release directory"
                            div .hint { "Removes every release, so rollback is no longer possible." }
                        }
                    }
                    div .form-actions {
                        button .btn.danger type="submit"
                            onclick="return confirm('Delete this service? Depending on the options above this stops the unit on the target and may delete every release, which cannot be undone.')" {
                            "Delete service"
                        }
                    }
                }
            }
        }
    }
}

/// A `key=value` map as newline-separated text for a textarea, in a stable order.
fn directives_text(map: &HashMap<String, String>) -> String {
    let mut lines: Vec<String> = map.iter().map(|(k, v)| format!("{k}={v}")).collect();
    lines.sort();
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn directives_render_in_a_stable_order() {
        let directives = HashMap::from([
            ("z".to_string(), "1".to_string()),
            ("a".to_string(), "2".to_string()),
            ("m".to_string(), "3".to_string()),
        ]);

        assert_eq!(directives_text(&directives), "a=2\nm=3\nz=1");
    }
}
