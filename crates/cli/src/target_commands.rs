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
    }

    Ok(())
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
