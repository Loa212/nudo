use super::*;

// ---------------------------------------------------------------------------
// targets
// ---------------------------------------------------------------------------

pub(super) async fn targets(cli: &Cli, command: &TargetCommand) -> anyhow::Result<()> {
    let mut client = cli.client()?.targets();

    match command {
        TargetCommand::List { selector } => {
            let response = client
                .list(ListTargetsRequest {
                    label_selector: selector.clone().unwrap_or_default(),
                    page_size: 200,
                    page_token: String::new(),
                })
                .await?
                .into_inner();

            let targets = response.targets;
            emit(cli, &JsonTargets::from(&targets), || {
                format::targets_table(&targets)
            });
        }

        TargetCommand::Get { id } => {
            let target = client
                .get(GetTargetRequest { id: id.clone() })
                .await?
                .into_inner();
            let list = vec![target];
            emit(cli, &JsonTargets::from(&list), || {
                format::targets_table(&list)
            });
        }

        TargetCommand::Add {
            name,
            host,
            port,
            user,
            ssh_key,
            latency_critical,
            labels,
        } => {
            let target = client
                .create(CreateTargetRequest {
                    mutation: Some(mutation(cli)),
                    name: name.clone(),
                    host: host.clone(),
                    port: *port,
                    user: user.clone(),
                    ssh_key_id: ssh_key.clone(),
                    latency_critical: *latency_critical,
                    labels: parse_labels(labels)?,
                })
                .await?
                .into_inner();

            let list = vec![target];
            emit(cli, &JsonTargets::from(&list), || {
                format::targets_table(&list)
            });
        }

        TargetCommand::Remove { id } => {
            client
                .delete(DeleteTargetRequest {
                    mutation: Some(mutation(cli)),
                    id: id.clone(),
                })
                .await?;
            println!("{}removed target {id}", dry_run_prefix(cli));
        }

        TargetCommand::Check { id } => {
            let response = client
                .check(CheckTargetRequest { id: id.clone() })
                .await?
                .into_inner();

            match cli.output {
                Output::Json => println!(
                    "{}",
                    serde_json::to_string_pretty(&JsonChecks::from(&response))?
                ),
                Output::Table => {
                    for check in &response.checks {
                        println!(
                            "{} {:<12} {}",
                            if check.ok { "ok  " } else { "FAIL" },
                            check.name,
                            check.detail
                        );
                    }
                }
            }

            // Non-zero exit so a CI step gating on reachability fails.
            if !response.ok {
                bail!("target {id} is not ready");
            }
        }

        TargetCommand::HostKey { id, accept, forget } => {
            let target = match (accept, forget) {
                (Some(fingerprint), _) => client
                    .accept_host_key(AcceptHostKeyRequest {
                        mutation: Some(mutation(cli)),
                        id: id.clone(),
                        fingerprint: fingerprint.trim().to_string(),
                    })
                    .await?
                    .into_inner(),
                (None, true) => client
                    .forget_host_key(ForgetHostKeyRequest {
                        mutation: Some(mutation(cli)),
                        id: id.clone(),
                    })
                    .await?
                    .into_inner(),
                (None, false) => client
                    .get(GetTargetRequest { id: id.clone() })
                    .await?
                    .into_inner(),
            };

            match cli.output {
                Output::Json => println!(
                    "{}",
                    serde_json::to_string_pretty(&JsonHostKey::of(&target))?
                ),
                Output::Table => print_host_key(cli, &target, accept.is_some(), *forget),
            }

            // Non-zero exit while a change is outstanding, so a CI step that
            // deploys after checking does not proceed against a host whose
            // identity is in question.
            if target
                .host_key
                .as_ref()
                .is_some_and(|k| !k.pending_key.is_empty())
            {
                bail!("target {id} has an unreviewed host-key change");
            }
        }

        TargetCommand::Ingress(command) => ingress(cli, &mut client, command).await?,
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// targets ingress
// ---------------------------------------------------------------------------

async fn ingress(
    cli: &Cli,
    client: &mut nudo_client::TargetsClient,
    command: &IngressCommand,
) -> anyhow::Result<()> {
    match command {
        IngressCommand::Enable {
            target,
            mode,
            acme_email,
            admin_port,
        } => {
            let parsed = ingress::Mode::parse(mode.trim());
            if parsed == ingress::Mode::Unspecified {
                bail!("unknown mode {mode:?}: expected `managed` or `external`");
            }

            let updated = client
                .enable_ingress(EnableIngressRequest {
                    mutation: Some(mutation(cli)),
                    target_id: target.clone(),
                    mode: parsed as i32,
                    admin_port: admin_port.unwrap_or(0),
                    acme_email: acme_email.clone().unwrap_or_default(),
                })
                .await?
                .into_inner();

            let state = updated.ingress.clone().unwrap_or_default();
            println!(
                "{}{} ingress on {} ({})",
                dry_run_prefix(cli),
                parsed.as_str(),
                updated.name,
                ingress::Status::try_from(state.status)
                    .unwrap_or(ingress::Status::Unspecified)
                    .as_str()
            );

            // Enabling stores the setting even when the host could not be
            // reached, so say why rather than reporting a success that did not
            // reach the machine.
            if !state.last_error.is_empty() {
                println!();
                println!("the proxy is not serving yet: {}", state.last_error);
                println!("fix that and run `nudo targets ingress reload {target}`");
            }
        }

        IngressCommand::Disable { target } => {
            let updated = client
                .disable_ingress(DisableIngressRequest {
                    mutation: Some(mutation(cli)),
                    target_id: target.clone(),
                })
                .await?
                .into_inner();
            println!(
                "{}disabled ingress on {}; its config is left on the host",
                dry_run_prefix(cli),
                updated.name
            );
        }

        IngressCommand::Show { target } => {
            let response = client
                .render_ingress(RenderIngressRequest {
                    target_id: target.clone(),
                })
                .await?
                .into_inner();

            match cli.output {
                Output::Json => println!(
                    "{}",
                    serde_json::to_string_pretty(&JsonIngressConfig::from(&response))?
                ),
                // The config alone, so it can be redirected to a file or piped
                // into a proxy the operator runs themselves.
                Output::Table => print!("{}", response.config),
            }
        }

        IngressCommand::Reload { target } => {
            let response = client
                .reload_ingress(ReloadIngressRequest {
                    mutation: Some(mutation(cli)),
                    target_id: target.clone(),
                })
                .await?
                .into_inner();

            if response.ok {
                println!(
                    "{}reloaded the proxy on {target}: {} route{}",
                    dry_run_prefix(cli),
                    response.routes.len(),
                    if response.routes.len() == 1 { "" } else { "s" }
                );
                for route in &response.routes {
                    println!("  {} -> :{}", route.domain, route.port);
                }
            } else {
                // Non-zero exit so a CI step that sets a domain and expects it
                // to be serving does not carry on as if it were.
                bail!(
                    "the proxy rejected the config and is still serving the \
                     previous one: {}",
                    response.error
                );
            }
        }

        IngressCommand::Check { target } => {
            let response = client
                .check_ingress(CheckIngressRequest {
                    target_id: target.clone(),
                })
                .await?
                .into_inner();

            match cli.output {
                Output::Json => println!(
                    "{}",
                    serde_json::to_string_pretty(&JsonChecks::from(&response))?
                ),
                Output::Table => {
                    for check in &response.checks {
                        println!(
                            "{} {:<24} {}",
                            if check.ok { "ok  " } else { "FAIL" },
                            check.name,
                            check.detail
                        );
                    }
                    // Warnings after the checks and clearly apart from them: a
                    // domain that does not resolve yet is the single most common
                    // way this feature disappoints someone, and it is not a
                    // failure — the record may be minutes away.
                    if !response.warnings.is_empty() {
                        println!();
                        for warning in &response.warnings {
                            println!("note: {warning}");
                        }
                    }
                }
            }

            if !response.ok {
                bail!("ingress on {target} is not ready");
            }
        }
    }

    Ok(())
}

/// The human-readable host-key report.
fn print_host_key(cli: &Cli, target: &Target, accepted: bool, forgotten: bool) {
    let prefix = dry_run_prefix(cli);
    let host_key = target.host_key.clone().unwrap_or_default();

    if forgotten {
        println!(
            "{prefix}forgot the pinned host key for {}; the next connection will pin afresh",
            target.name
        );
        return;
    }
    if accepted {
        println!(
            "{prefix}accepted {} as the host key for {}",
            host_key.fingerprint, target.name
        );
        return;
    }

    if host_key.key.is_empty() {
        println!(
            "no host key pinned for {} yet — the first successful connection will record one",
            target.name
        );
    } else {
        println!("pinned    {}", host_key.fingerprint);
        println!("          {}", host_key.key.trim());
    }

    if !host_key.pending_key.is_empty() {
        println!();
        println!("CHANGED   {}", host_key.pending_fingerprint);
        println!("          {}", host_key.pending_key.trim());
        println!();
        println!(
            "Every connection to {} is refused until this is resolved. Compare the \n\
             fingerprint against `ssh-keyscan -t ed25519 {}` run on the machine itself, \n\
             then accept it with:\n\n    nudo targets host-key {} --accept {}",
            target.name, target.host, target.id, host_key.pending_fingerprint
        );
    }
}

/// Parses repeated `key=value` label flags.
pub(crate) fn parse_labels(
    labels: &[String],
) -> anyhow::Result<std::collections::HashMap<String, String>> {
    let mut parsed = std::collections::HashMap::new();
    for label in labels {
        let (key, value) = label
            .split_once('=')
            .ok_or_else(|| anyhow!("label {label:?} must be in key=value form"))?;
        if key.trim().is_empty() {
            bail!("label {label:?} has an empty key");
        }
        parsed.insert(key.trim().to_string(), value.trim().to_string());
    }
    Ok(parsed)
}

// ---------------------------------------------------------------------------
// services
// ---------------------------------------------------------------------------
