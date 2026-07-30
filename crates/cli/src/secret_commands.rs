use super::*;

pub(super) async fn secrets(cli: &Cli, command: &SecretCommand) -> anyhow::Result<()> {
    let mut client = cli.client()?.secrets();

    match command {
        SecretCommand::List { target, service } => {
            let response = client
                .list(ListSecretsRequest {
                    target_id: target.clone().unwrap_or_default(),
                    service_id: service.clone().unwrap_or_default(),
                })
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
        }
        | SecretCommand::Rotate {
            name,
            value,
            target,
            service,
        } => {
            let rotating = matches!(command, SecretCommand::Rotate { .. });
            let value = read_value(value, if rotating { "rotate" } else { "set" }).await?;

            let secret = client
                .put(PutSecretRequest {
                    mutation: Some(mutation(cli)),
                    name: name.clone(),
                    value,
                    scope_target_id: target.clone().unwrap_or_default(),
                    scope_service_id: service.clone().unwrap_or_default(),
                    replace: rotating,
                })
                .await?
                .into_inner();

            println!(
                "{}{} {} ({}) digest {}",
                dry_run_prefix(cli),
                if rotating { "rotated" } else { "stored" },
                secret.name,
                format::scope_label(&secret),
                secret.digest.chars().take(12).collect::<String>()
            );
        }

        SecretCommand::Remove { id } => {
            client
                .delete(DeleteSecretRequest {
                    mutation: Some(mutation(cli)),
                    id: id.clone(),
                })
                .await?;
            println!("{}removed secret {id}", dry_run_prefix(cli));
        }
    }

    Ok(())
}

/// The value to store, from `--value` or stdin.
///
/// Reading from stdin by default keeps the value out of shell history and out
/// of the process table, which is why it is the documented path for a key.
async fn read_value(value: &Option<String>, verb: &str) -> anyhow::Result<String> {
    if let Some(value) = value {
        return Ok(value.clone());
    }

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
             e.g. `printf %s \"$TOKEN\" | nudo secrets {verb} NAME`"
        );
    }
    Ok(trimmed)
}
