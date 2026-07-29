use super::*;

/// A single service: live state, controls, configuration, history, releases.
pub fn service_detail(
    service: &Service,
    target: &Target,
    status: &UnitStatus,
    releases: &[Release],
    deployments: &[Deployment],
    csrf: &str,
) -> Markup {
    let unit = service.unit.clone().unwrap_or_default();
    let running = status.active_state == "active";
    let hot = target.latency_critical;

    html! {
        (topbar(&service.name, Some(&format!("{} · {}", target.name, unit.unit_name)), html! {
            div .row { (unit_badge(status)) @if hot { (latency_critical_badge()) } }

            form method="post" action=(format!("/services/{}/deploy", service.id)) {
                (csrf_input(csrf))
                button .btn.primary type="submit"
                    onclick=(deploy_confirm(hot)) { "Deploy" }
            }
            // One endpoint takes every unit action, with the verb in a hidden
            // field — so start, stop, restart, reload, enable and disable all go
            // through the same handler and the same guardrail.
            form method="post" action=(format!("/services/{}/action", service.id)) {
                (csrf_input(csrf))
                input type="hidden" name="action" value="restart";
                // Restarting drops in-flight work, so it confirms even though
                // the end state is the same as the current one.
                button .btn type="submit"
                    onclick=(format!("return confirm('Restart {}? The process will be killed and started again, dropping anything in flight.')", js_text(&unit.unit_name))) {
                    "Restart"
                }
            }
            @if running {
                form method="post" action=(format!("/services/{}/action", service.id)) {
                    (csrf_input(csrf))
                    input type="hidden" name="action" value="stop";
                    button .btn.danger type="submit"
                        onclick=(format!("return confirm('Stop {}? The service will be down until it is started again.')", js_text(&unit.unit_name))) {
                        "Stop"
                    }
                }
            } @else {
                form method="post" action=(format!("/services/{}/action", service.id)) {
                    (csrf_input(csrf))
                    input type="hidden" name="action" value="start";
                    button .btn type="submit" { "Start" }
                }
            }
        }))
        (tabs(&[
            ("Overview", &format!("/services/{}", service.id), true),
            ("Logs", &format!("/services/{}/logs", service.id), false),
            ("Unit file", &format!("/services/{}/unit", service.id), false),
            ("Edit", &format!("/services/{}/edit", service.id), false),
        ]))
        div .content {
            @if hot {
                (callout("bad", "Runs on a latency-critical host", html! {
                    "Deploys and unit actions against " (target.name) " require an \
                     explicit override and are never performed unattended."
                }))
            }
            @if status.active_state == "failed" {
                (callout("bad", "Unit is failed", html! {
                    "systemd reports " span .mono { (status.active_state) "/" (status.sub_state) }
                    ". The logs tab has the last output before it died."
                }))
            }

            // Latency knobs first when set: they are the reason a service is on
            // nudo instead of in a container, and a silently dropped affinity is
            // exactly the bug that is hardest to notice.
            @if has_latency_knobs(service) {
                div .card {
                    div .row {
                        h2 { "Latency configuration" }
                        (badge("pinned", BadgeKind::Hot))
                    }
                    dl .dl style="margin-top:12px" {
                        @if !unit.cpu_affinity.is_empty() {
                            dt { "CPUAffinity" } dd .mono { (unit.cpu_affinity) }
                        }
                        @if !unit.nice.is_empty() {
                            dt { "Nice" } dd .mono { (unit.nice) }
                        }
                        @if !unit.io_scheduling_class.is_empty() {
                            dt { "IOSchedulingClass" } dd .mono { (unit.io_scheduling_class) }
                        }
                    }
                }
            }

            div .card {
                h2 { "Summary" }
                dl .dl style="margin-top:12px" {
                    dt { "Target" }
                    dd { a href=(format!("/targets/{}", target.id)) { (target.name) }
                         span .sep { " · " } span .mono.small { (address(target)) } }
                    dt { "Unit" }             dd .mono { (or_dash(&unit.unit_name)) }
                    // Only when routed: a "Domain -" row on every service that
                    // is not would be noise on the pages where it never will be.
                    @if !service.domain.is_empty() {
                        dt { "Domain" }
                        dd {
                            a href=(format!("https://{}", service.domain))
                              target="_blank" rel="noreferrer noopener" {
                                (service.domain)
                            }
                            span .sep { " · " }
                            span .mono.small { "→ :" (service.port) }
                        }
                    }
                    dt { "Source" }           dd { (artifact_detail(service)) }
                    dt { "Release root" }     dd .mono { (or_dash(&service.release_root)) }
                    dt { "Current release" }  dd .mono {
                        @if service.current_release_id.is_empty() {
                            span .muted { "never deployed" }
                        } @else {
                            (service.current_release_id)
                        }
                    }
                    dt { "Keep releases" }    dd { (service.keep_releases) }
                    dt { "Health check" }     dd { (health_check_summary(service)) }
                    dt { "Restart" }          dd .mono {
                        (or_dash(&unit.restart))
                        @if unit.restart_sec > 0 { " after " (unit.restart_sec) "s" }
                    }
                    dt { "Runs as" }          dd .mono {
                        (or_dash(&unit.user))
                        @if !unit.group.is_empty() { ":" (unit.group) }
                    }
                    dt { "Working dir" }      dd .mono { (or_dash(&unit.working_directory)) }
                    dt { "Env" }              dd {
                        @if service.env.is_empty() {
                            span .muted { "-" }
                        } @else {
                            span .mono.small { (env_line(&service.env)) }
                        }
                    }
                    // Ids only. The values are resolved on the target at deploy
                    // time and never transit the API, let alone this page.
                    dt { "Secrets" }          dd {
                        @if service.secret_ids.is_empty() {
                            span .muted { "-" }
                        } @else {
                            span .mono.small { (service.secret_ids.join(", ")) }
                            span .sep { " · " }
                            span .small.faint { "ids only; values are written on the target" }
                        }
                    }
                    dt { "PID" }              dd .mono { @if status.pid > 0 { (status.pid) } @else { "-" } }
                    dt { "Memory" }           dd { @if status.memory_bytes > 0 { (bytes(status.memory_bytes)) } @else { "-" } }
                    dt { "Restarts" }         dd { (status.restart_count) }
                    dt { "Enabled at boot" }  dd { @if status.enabled { "yes" } @else { "no" } }
                    dt { "Active since" }     dd { (ago(status.since.as_ref())) }
                }
            }

            div .card.pad-0 {
                div .card-head {
                    h2 { "Recent deployments" }
                    div .actions {
                        a .btn.small href=(format!("/deployments?service={}", service.id)) { "All" }
                    }
                }
                @if deployments.is_empty() {
                    div .card-body { p .muted { "This service has never been deployed." } }
                } @else {
                    (deployments_table(deployments, std::slice::from_ref(service)))
                }
            }

            div .card.pad-0 {
                div .card-head {
                    h2 { "Releases" }
                    p .card-note { "Kept for rollback: " (service.keep_releases) }
                }
                @if releases.is_empty() {
                    div .card-body { p .muted { "No releases yet." } }
                } @else {
                    div .table-scroll {
                        table {
                            thead {
                                tr {
                                    th { "Release" }
                                    th { "Ref" }
                                    th { "SHA" }
                                    th { "Size" }
                                    th { "Created" }
                                    th {}
                                }
                            }
                            tbody {
                                @for release in releases {
                                    @let current = release.id == service.current_release_id;
                                    tr {
                                        td .mono.small {
                                            (release.id)
                                            @if current { " " (badge("current", BadgeKind::Info)) }
                                        }
                                        td .small { (or_dash(&release.git_ref)) }
                                        td .mono.small { (short_sha(&release.git_sha)) }
                                        td .num.small { (bytes(release.artifact_bytes)) }
                                        td .nowrap.small.muted { (ago(release.created_at.as_ref())) }
                                        td {
                                            @if !current {
                                                form method="post" action=(format!("/services/{}/rollback", service.id)) {
                                                    (csrf_input(csrf))
                                                    input type="hidden" name="release_id" value=(release.id);
                                                    button .btn.small.danger type="submit"
                                                        onclick=(format!("return confirm('Roll {} back to release {}? The current symlink is swapped and the unit restarted.')", js_text(&service.name), js_text(&release.id))) {
                                                        "Rollback"
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

/// The confirm text for a deploy. On a latency-critical host it names the
/// override, because that is the consequence the operator is agreeing to.
fn deploy_confirm(latency_critical: bool) -> String {
    if latency_critical {
        "return confirm('Deploy to a LATENCY-CRITICAL host? This restarts the unit on a machine where that has a cost. Continue only if you are watching it.')"
            .to_string()
    } else {
        "return confirm('Deploy the current source to this service? The unit will be restarted.')"
            .to_string()
    }
}

/// Where the binary comes from, in full.
fn artifact_detail(service: &Service) -> Markup {
    use nudo_proto::artifact_source::Kind;
    html! {
        @match service.artifact.as_ref().and_then(|a| a.kind.as_ref()) {
            Some(Kind::Url(url)) => span .mono.small { (url) },
            Some(Kind::Git(git)) => {
                span .mono.small { (git.repo) "@" (or_dash(&git.branch)) }
                @if !git.build_command.is_empty() {
                    div .small.muted { "build: " span .mono { (git.build_command) } }
                }
                @if !git.artifact_path.is_empty() {
                    div .small.muted { "artifact: " span .mono { (git.artifact_path) } }
                }
                @if git.auto_deploy_on_push {
                    div { (badge("auto-deploy on push", BadgeKind::Info)) }
                }
            },
            _ => span .muted { "pushed by the CLI" },
        }
    }
}

/// The health check in one line. Which check runs decides whether a bad deploy
/// rolls back, so "systemd only" is stated rather than implied.
fn health_check_summary(service: &Service) -> Markup {
    use nudo_proto::health_check::Kind;
    let Some(check) = service.health_check.as_ref() else {
        return html! { span .muted { "none — a deploy is never rolled back automatically" } };
    };

    html! {
        @match check.kind.as_ref() {
            Some(Kind::HttpUrl(url)) => { "GET " span .mono.small { (url) } },
            Some(Kind::Command(command)) => { "command " span .mono.small { (command) } },
            Some(Kind::SystemdActive(_)) => { "systemctl is-active only" },
            None => span .muted { "none" },
        }
        @if check.timeout_seconds > 0 || check.retries > 0 || check.initial_delay_seconds > 0 {
            span .sep { " · " }
            span .small.muted {
                (check.timeout_seconds) "s timeout, "
                (check.retries) " retries, "
                (check.initial_delay_seconds) "s initial delay"
            }
        }
    }
}

/// Non-secret env as `K=V` pairs in a stable order.
fn env_line(env: &HashMap<String, String>) -> String {
    let mut pairs: Vec<String> = env.iter().map(|(k, v)| format!("{k}={v}")).collect();
    pairs.sort();
    pairs.join(" ")
}

/// A preview of the systemd unit nudo would write.
pub fn service_unit(service: &Service, unit_file: &str) -> Markup {
    html! {
        (topbar(&service.name, Some("Rendered systemd unit"), html! {
            a .btn href=(format!("/services/{}", service.id)) { "Back to service" }
        }))
        (tabs(&[
            ("Overview", &format!("/services/{}", service.id), false),
            ("Logs", &format!("/services/{}/logs", service.id), false),
            ("Unit file", &format!("/services/{}/unit", service.id), true),
            ("Edit", &format!("/services/{}/edit", service.id), false),
        ]))
        div .content {
            (callout("info", "This is a preview", html! {
                "Rendered from the current configuration, not read from the target. \
                 It is written to "
                span .mono {
                    "/etc/systemd/system/"
                    (service.unit.as_ref().map(|u| u.unit_name.clone()).unwrap_or_default())
                }
                " on the next deploy."
            }))
            div .card {
                pre .unit { (unit_file) }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn environment_pairs_render_in_a_stable_order() {
        let env = HashMap::from([
            ("z".to_string(), "1".to_string()),
            ("a".to_string(), "2".to_string()),
            ("m".to_string(), "3".to_string()),
        ]);

        assert_eq!(env_line(&env), "a=2 m=3 z=1");
    }
}
