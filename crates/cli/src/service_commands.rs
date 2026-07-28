use super::*;

pub(super) async fn services(cli: &Cli, command: &ServiceCommand) -> anyhow::Result<()> {
    let mut client = services_api_client::ServicesApiClient::new(channel(cli).await?);

    match command {
        ServiceCommand::List { target } => {
            let response = client
                .list(authenticated(
                    cli,
                    ListServicesRequest {
                        target_id: target.clone().unwrap_or_default(),
                        page_size: 200,
                        page_token: String::new(),
                    },
                ))
                .await?
                .into_inner();

            let services = response.services;
            emit(cli, &JsonServices::from(&services), || {
                format::services_table(&services)
            });
        }

        ServiceCommand::Get { id } => {
            let service = client
                .get(authenticated(cli, GetServiceRequest { id: id.clone() }))
                .await?
                .into_inner();
            let list = vec![service];
            emit(cli, &JsonServices::from(&list), || {
                format::services_table(&list)
            });
        }

        ServiceCommand::Unit { id } => {
            let response = client
                .render_unit(authenticated(
                    cli,
                    RenderUnitRequest {
                        service_id: id.clone(),
                    },
                ))
                .await?
                .into_inner();
            // Printed raw so it can be piped to a file or diffed.
            print!("{}", response.unit_file);
        }

        ServiceCommand::Status { id } => {
            let status = client
                .get_unit_status(authenticated(
                    cli,
                    GetUnitStatusRequest {
                        service_id: id.clone(),
                    },
                ))
                .await?
                .into_inner();

            match cli.output {
                Output::Json => println!(
                    "{}",
                    serde_json::to_string_pretty(&JsonUnitStatus::from(&status))?
                ),
                Output::Table => println!("{}", format::unit_status_line(&status)),
            }
        }

        ServiceCommand::Start { id } => unit_action(cli, &mut client, id, "start").await?,
        ServiceCommand::Stop { id } => unit_action(cli, &mut client, id, "stop").await?,
        ServiceCommand::Restart { id } => unit_action(cli, &mut client, id, "restart").await?,
        ServiceCommand::Enable { id } => unit_action(cli, &mut client, id, "enable").await?,
        ServiceCommand::Disable { id } => unit_action(cli, &mut client, id, "disable").await?,

        ServiceCommand::Deployments { id, limit } => {
            let mut deployments = deployments_client::DeploymentsClient::new(channel(cli).await?);
            let response = deployments
                .list(authenticated(
                    cli,
                    ListDeploymentsRequest {
                        service_id: id.clone(),
                        page_size: *limit,
                        page_token: String::new(),
                    },
                ))
                .await?
                .into_inner();

            let list = response.deployments;
            emit(cli, &JsonDeployments::from(&list), || {
                format::deployments_table(&list)
            });
        }

        ServiceCommand::Releases { id } => {
            let mut deployments = deployments_client::DeploymentsClient::new(channel(cli).await?);
            let response = deployments
                .list_releases(authenticated(
                    cli,
                    ListReleasesRequest {
                        service_id: id.clone(),
                    },
                ))
                .await?
                .into_inner();

            let list = response.releases;
            emit(cli, &JsonReleases::from(&list), || {
                format::releases_table(&list)
            });
        }
    }

    Ok(())
}

async fn unit_action(
    cli: &Cli,
    client: &mut services_api_client::ServicesApiClient<Channel>,
    service_id: &str,
    verb: &str,
) -> anyhow::Result<()> {
    let action = match verb {
        "start" => unit_action_request::Action::Start,
        "stop" => unit_action_request::Action::Stop,
        "restart" => unit_action_request::Action::Restart,
        "enable" => unit_action_request::Action::Enable,
        "disable" => unit_action_request::Action::Disable,
        other => bail!("unknown action {other}"),
    };

    let status = client
        .unit_action(authenticated(
            cli,
            UnitActionRequest {
                mutation: Some(mutation(cli)),
                service_id: service_id.to_string(),
                action: action as i32,
            },
        ))
        .await?
        .into_inner();

    println!("{}{verb} {service_id}", dry_run_prefix(cli));
    println!("{}", format::unit_status_line(&status));
    Ok(())
}
