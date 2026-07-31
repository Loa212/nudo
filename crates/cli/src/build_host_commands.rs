use super::*;

use crate::target_commands::parse_labels;

pub(super) async fn build_hosts(cli: &Cli, command: &BuildHostCommand) -> anyhow::Result<()> {
    let mut client = cli.client()?.build_hosts();

    match command {
        BuildHostCommand::List { selector } => {
            let response = client
                .list(ListBuildHostsRequest {
                    label_selector: selector.clone().unwrap_or_default(),
                    page_size: 200,
                    page_token: String::new(),
                })
                .await?
                .into_inner();

            let hosts = response.build_hosts;
            emit(cli, &JsonBuildHosts::from(&hosts), || {
                format::build_hosts_table(&hosts)
            });
        }

        BuildHostCommand::Get { id } => {
            let host = client
                .get(GetBuildHostRequest { id: id.clone() })
                .await?
                .into_inner();

            let list = vec![host];
            emit(cli, &JsonBuildHosts::from(&list), || {
                format::build_hosts_table(&list)
            });
        }

        BuildHostCommand::Add {
            name,
            host,
            port,
            user,
            ssh_key,
            workspace_root,
            latency_critical,
            labels,
        } => {
            let created = client
                .create(CreateBuildHostRequest {
                    mutation: Some(mutation(cli)),
                    name: name.clone(),
                    host: host.clone(),
                    port: *port,
                    user: user.clone(),
                    ssh_key_id: ssh_key.clone(),
                    workspace_root: workspace_root.clone().unwrap_or_default(),
                    latency_critical: *latency_critical,
                    labels: parse_labels(labels)?,
                })
                .await?
                .into_inner();

            let list = vec![created];
            emit(cli, &JsonBuildHosts::from(&list), || {
                format::build_hosts_table(&list)
            });
        }

        BuildHostCommand::Remove { id } => {
            client
                .delete(DeleteBuildHostRequest {
                    mutation: Some(mutation(cli)),
                    id: id.clone(),
                })
                .await?;
            println!("{}removed build host {id}", dry_run_prefix(cli));
        }

        BuildHostCommand::Check { id } => {
            let response = client
                .check(CheckBuildHostRequest { id: id.clone() })
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
                    // After the checks, so a warning is the last thing read —
                    // and never confused with a failure, since the command
                    // still exits zero.
                    for warning in &response.warnings {
                        println!();
                        println!("warning: {warning}");
                    }
                }
            }

            // Non-zero exit so a CI step gating on readiness fails. Warnings
            // deliberately do not affect this: a latency-critical build host is
            // a choice, not a fault.
            if !response.ok {
                bail!("build host {id} is not ready");
            }
        }

        BuildHostCommand::HostKey { id, accept, forget } => {
            let host = match (accept, forget) {
                (Some(fingerprint), _) => client
                    .accept_host_key(AcceptBuildHostKeyRequest {
                        mutation: Some(mutation(cli)),
                        id: id.clone(),
                        fingerprint: fingerprint.trim().to_string(),
                    })
                    .await?
                    .into_inner(),
                (None, true) => client
                    .forget_host_key(ForgetBuildHostKeyRequest {
                        mutation: Some(mutation(cli)),
                        id: id.clone(),
                    })
                    .await?
                    .into_inner(),
                (None, false) => client
                    .get(GetBuildHostRequest { id: id.clone() })
                    .await?
                    .into_inner(),
            };

            match cli.output {
                Output::Json => println!(
                    "{}",
                    serde_json::to_string_pretty(&JsonHostKey::of_build_host(&host))?
                ),
                Output::Table => print_host_key(cli, &host, accept.is_some(), *forget),
            }

            // Non-zero exit while a change is outstanding, so a CI step that
            // builds after checking does not proceed against a host whose
            // identity is in question.
            if host
                .host_key
                .as_ref()
                .is_some_and(|k| !k.pending_key.is_empty())
            {
                bail!("build host {id} has an unreviewed host-key change");
            }
        }

        BuildHostCommand::Default { id, local, show } => {
            // `--show`, or no argument at all, reads rather than writes.
            if *show || (id.is_none() && !*local) {
                let defaults = client
                    .get_defaults(GetBuildDefaultsRequest {})
                    .await?
                    .into_inner();
                print_default(cli, &defaults.build_host_id);
                return Ok(());
            }

            let build_host_id = if *local {
                LOCAL_BUILD_HOST_ID.to_string()
            } else {
                id.clone().unwrap_or_default()
            };

            let defaults = client
                .set_defaults(SetBuildDefaultsRequest {
                    mutation: Some(mutation(cli)),
                    build_host_id,
                })
                .await?
                .into_inner();

            print!("{}", dry_run_prefix(cli));
            print_default(cli, &defaults.build_host_id);
        }
    }

    Ok(())
}

/// Says where builds run by default, in the terms the operator set it in.
fn print_default(cli: &Cli, build_host_id: &str) {
    if cli.output == Output::Json {
        let json = serde_json::json!({ "build_host_id": build_host_id });
        if let Ok(rendered) = serde_json::to_string_pretty(&json) {
            println!("{rendered}");
        }
        return;
    }

    if build_host_id.is_empty() || build_host_id == LOCAL_BUILD_HOST_ID {
        println!("builds run on the control plane by default");
    } else {
        println!("builds run on {build_host_id} by default");
    }
}

/// The human-readable host-key report for a build host.
///
/// The same report `nudo targets host-key` prints, against the other noun. Kept
/// separate rather than generalised because the remediation command it prints
/// has to name the right one — being sent to `nudo targets host-key <id>` for a
/// build host is a dead end.
fn print_host_key(cli: &Cli, host: &BuildHost, accepted: bool, forgotten: bool) {
    let prefix = dry_run_prefix(cli);
    let host_key = host.host_key.clone().unwrap_or_default();

    if forgotten {
        println!(
            "{prefix}forgot the pinned host key for {}; the next connection will pin afresh",
            host.name
        );
        return;
    }
    if accepted {
        println!(
            "{prefix}accepted {} as the host key for {}",
            host_key.fingerprint, host.name
        );
        return;
    }

    if host_key.key.is_empty() {
        println!(
            "no host key pinned for {} yet — the first successful connection will record one",
            host.name
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
            "Every connection to {} is refused until this is resolved, so nothing \n\
             builds there. Compare the fingerprint against `ssh-keyscan -t ed25519 {}` \n\
             run on the machine itself, then accept it with:\n\n    \
             nudo build-hosts host-key {} --accept {}",
            host.name, host.host, host.id, host_key.pending_fingerprint
        );
    }
}
