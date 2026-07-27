use super::*;

pub(super) async fn secrets(cli: &Cli, command: &SecretCommand) -> anyhow::Result<()> {
    let mut client = secrets_client::SecretsClient::new(channel(cli).await?);

    match command {
        SecretCommand::List { target, service } => {
            let response = client
                .list(authenticated(
                    cli,
                    ListSecretsRequest {
                        target_id: target.clone().unwrap_or_default(),
                        service_id: service.clone().unwrap_or_default(),
                    },
                ))
                .await?
                .into_inner();

            let list = response.secrets;
            emit(cli, &JsonSecrets::from(&list), || {
                format::secrets_table(&list)
            });
        }

        SecretCommand::Set {
            name,
            value,
            target,
            service,
        } => {
            // Reading from stdin by default keeps the value out of shell history
            // and out of the process table.
            let value = match value {
                Some(value) => value.clone(),
                None => {
                    use tokio::io::AsyncReadExt;
                    let mut buffer = String::new();
                    tokio::io::stdin()
                        .read_to_string(&mut buffer)
                        .await
                        .context("reading the secret value from stdin")?;
                    let trimmed = buffer.trim_end_matches('\n').to_string();
                    if trimmed.is_empty() {
                        bail!(
                            "no value given: pass --value or pipe one in, \
                             e.g. `printf %s \"$TOKEN\" | nudo secrets set NAME`"
                        );
                    }
                    trimmed
                }
            };

            let secret = client
                .put(authenticated(
                    cli,
                    PutSecretRequest {
                        mutation: Some(mutation(cli)),
                        name: name.clone(),
                        value,
                        scope_target_id: target.clone().unwrap_or_default(),
                        scope_service_id: service.clone().unwrap_or_default(),
                    },
                ))
                .await?
                .into_inner();

            println!(
                "{}stored {} ({}) digest {}",
                dry_run_prefix(cli),
                secret.name,
                format::scope_label(&secret),
                secret.digest.chars().take(12).collect::<String>()
            );
        }

        SecretCommand::Remove { id } => {
            client
                .delete(authenticated(
                    cli,
                    DeleteSecretRequest {
                        mutation: Some(mutation(cli)),
                        id: id.clone(),
                    },
                ))
                .await?;
            println!("{}removed secret {id}", dry_run_prefix(cli));
        }
    }

    Ok(())
}
