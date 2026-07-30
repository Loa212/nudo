use super::*;

pub(super) async fn services(cli: &Cli, command: &ServiceCommand) -> anyhow::Result<()> {
    let mut client = cli.client()?.services();

    match command {
        ServiceCommand::List { target } => {
            let response = client
                .list(ListServicesRequest {
                    target_id: target.clone().unwrap_or_default(),
                    page_size: 200,
                    page_token: String::new(),
                })
                .await?
                .into_inner();

            let services = response.services;
            emit(cli, &JsonServices::from(&services), || {
                format::services_table(&services)
            });
        }

        ServiceCommand::Get { id } => {
            let service = client
                .get(GetServiceRequest { id: id.clone() })
                .await?
                .into_inner();
            let list = vec![service];
            emit(cli, &JsonServices::from(&list), || {
                format::services_table(&list)
            });
        }

        ServiceCommand::Unit { id } => {
            let response = client
                .render_unit(RenderUnitRequest {
                    service_id: id.clone(),
                })
                .await?
                .into_inner();
            // Printed raw so it can be piped to a file or diffed.
            print!("{}", response.unit_file);
        }

        ServiceCommand::Status { id } => {
            let status = client
                .get_unit_status(GetUnitStatusRequest {
                    service_id: id.clone(),
                })
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
            let mut deployments = cli.client()?.deployments();
            let response = deployments
                .list(ListDeploymentsRequest {
                    service_id: id.clone(),
                    page_size: *limit,
                    page_token: String::new(),
                })
                .await?
                .into_inner();

            let list = response.deployments;
            emit(cli, &JsonDeployments::from(&list), || {
                format::deployments_table(&list)
            });
        }

        ServiceCommand::Releases { id } => {
            let mut deployments = cli.client()?.deployments();
            let response = deployments
                .list_releases(ListReleasesRequest {
                    service_id: id.clone(),
                })
                .await?
                .into_inner();

            let list = response.releases;
            emit(cli, &JsonReleases::from(&list), || {
                format::releases_table(&list)
            });
        }

        ServiceCommand::Domain {
            id,
            routes,
            grpc,
            clear,
        } => {
            if !clear && routes.is_empty() {
                bail!(
                    "pass --route DOMAIN[/PATH]:PORT to route this service, or \
                     --clear to stop routing to it"
                );
            }

            let parsed = if *clear {
                Vec::new()
            } else {
                routes
                    .iter()
                    .map(|raw| parse_route(raw, *grpc))
                    .collect::<anyhow::Result<Vec<_>>>()?
            };

            let updated = client
                .update(UpdateServiceRequest {
                    mutation: Some(mutation(cli)),
                    id: id.clone(),
                    service: Some(Service {
                        routes: parsed,
                        ..Default::default()
                    }),
                    // Named explicitly: an empty mask means "apply every
                    // field", which here would blank the rest of the service.
                    update_mask: vec!["routes".to_string()],
                })
                .await?
                .into_inner();

            if updated.routes.is_empty() {
                println!(
                    "{}{} is no longer routed",
                    dry_run_prefix(cli),
                    updated.name
                );
            } else {
                println!("{}{} is reachable at:", dry_run_prefix(cli), updated.name);
                for route in &updated.routes {
                    let protocol = route.protocol_or_default();
                    println!(
                        "  https://{}{}  ->  {}:{}{}",
                        route.domain,
                        route.path,
                        if protocol == route::Protocol::H2c {
                            "h2c://127.0.0.1"
                        } else {
                            "127.0.0.1"
                        },
                        route.port,
                        if protocol == route::Protocol::H2c {
                            " (gRPC)"
                        } else {
                            ""
                        }
                    );
                }
            }
        }
    }

    Ok(())
}

async fn unit_action(
    cli: &Cli,
    client: &mut nudo_client::ServicesClient,
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
        .unit_action(UnitActionRequest {
            mutation: Some(mutation(cli)),
            service_id: service_id.to_string(),
            action: action as i32,
        })
        .await?
        .into_inner();

    println!("{}{verb} {service_id}", dry_run_prefix(cli));
    println!("{}", format::unit_status_line(&status));
    Ok(())
}

/// Parses `domain[/path]:port` into a route.
///
/// One flag rather than three, because a route's parts belong together: the
/// alternative is `--domain a --path b --port c` repeated, where nothing in the
/// syntax says which path goes with which domain.
///
/// The port is taken from the last colon so an IPv6-looking value fails on the
/// domain validator rather than being silently mis-split.
pub(crate) fn parse_route(raw: &str, grpc: bool) -> anyhow::Result<Route> {
    let raw = raw.trim();
    // A pasted URL is the obvious mistake; take the host part rather than
    // refusing something whose meaning is unambiguous.
    let raw = raw
        .strip_prefix("https://")
        .or_else(|| raw.strip_prefix("http://"))
        .unwrap_or(raw);

    let (host_and_path, port) = raw.rsplit_once(':').ok_or_else(|| {
        anyhow!(
            "{raw:?} needs a port, as DOMAIN[/PATH]:PORT — nudo has to know what \
             port the service listens on to route to it"
        )
    })?;

    let port: u32 = port
        .parse()
        .with_context(|| format!("{port:?} in {raw:?} is not a port number"))?;

    let (domain, path) = match host_and_path.split_once('/') {
        Some((domain, path)) => (domain, format!("/{path}")),
        None => (host_and_path, String::new()),
    };

    let route = Route {
        domain: domain.trim().to_string(),
        path,
        port,
        protocol: if grpc {
            route::Protocol::H2c as i32
        } else {
            route::Protocol::Unspecified as i32
        },
        ..Default::default()
    };

    // Refused here as well as server-side so the message names the argument
    // that is wrong rather than arriving as a gRPC status.
    route
        .validate()
        .map_err(|error| anyhow!("{raw:?}: {error}"))?;
    Ok(route)
}
