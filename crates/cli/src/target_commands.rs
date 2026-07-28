use super::*;

// ---------------------------------------------------------------------------
// targets
// ---------------------------------------------------------------------------

pub(super) async fn targets(cli: &Cli, command: &TargetCommand) -> anyhow::Result<()> {
    let mut client = targets_client::TargetsClient::new(channel(cli).await?);

    match command {
        TargetCommand::List { selector } => {
            let response = client
                .list(authenticated(
                    cli,
                    ListTargetsRequest {
                        label_selector: selector.clone().unwrap_or_default(),
                        page_size: 200,
                        page_token: String::new(),
                    },
                ))
                .await?
                .into_inner();

            let targets = response.targets;
            emit(cli, &JsonTargets::from(&targets), || {
                format::targets_table(&targets)
            });
        }

        TargetCommand::Get { id } => {
            let target = client
                .get(authenticated(cli, GetTargetRequest { id: id.clone() }))
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
                .create(authenticated(
                    cli,
                    CreateTargetRequest {
                        mutation: Some(mutation(cli)),
                        name: name.clone(),
                        host: host.clone(),
                        port: *port,
                        user: user.clone(),
                        ssh_key_id: ssh_key.clone(),
                        latency_critical: *latency_critical,
                        labels: parse_labels(labels)?,
                    },
                ))
                .await?
                .into_inner();

            let list = vec![target];
            emit(cli, &JsonTargets::from(&list), || {
                format::targets_table(&list)
            });
        }

        TargetCommand::Remove { id } => {
            client
                .delete(authenticated(
                    cli,
                    DeleteTargetRequest {
                        mutation: Some(mutation(cli)),
                        id: id.clone(),
                    },
                ))
                .await?;
            println!("{}removed target {id}", dry_run_prefix(cli));
        }

        TargetCommand::Check { id } => {
            let response = client
                .check(authenticated(cli, CheckTargetRequest { id: id.clone() }))
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
                    .accept_host_key(authenticated(
                        cli,
                        AcceptHostKeyRequest {
                            mutation: Some(mutation(cli)),
                            id: id.clone(),
                            fingerprint: fingerprint.trim().to_string(),
                        },
                    ))
                    .await?
                    .into_inner(),
                (None, true) => client
                    .forget_host_key(authenticated(
                        cli,
                        ForgetHostKeyRequest {
                            mutation: Some(mutation(cli)),
                            id: id.clone(),
                        },
                    ))
                    .await?
                    .into_inner(),
                (None, false) => client
                    .get(authenticated(cli, GetTargetRequest { id: id.clone() }))
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
pub(super) fn parse_labels(
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
