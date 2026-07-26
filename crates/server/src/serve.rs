//! Assembling and running the gRPC server.
//!
//! Lives in the library rather than the binary so the all-in-one build can start
//! the same server in-process alongside the dashboard, instead of maintaining a
//! second copy of the wiring.

use std::sync::Arc;

use nudo_proto::audit_server::AuditServer;
use nudo_proto::deployments_server::DeploymentsServer;
use nudo_proto::logs_server::LogsServer;
use nudo_proto::secrets_server::SecretsServer;
use nudo_proto::services_api_server::ServicesApiServer;
use nudo_proto::sources_server::SourcesServer;
use nudo_proto::targets_server::TargetsServer;
use nudo_proto::terminals_server::TerminalsServer;

use crate::Config;
use crate::api;

/// Builds the state, starts the background workers, and serves until shutdown.
pub async fn run(config: Config, addr: std::net::SocketAddr) -> anyhow::Result<()> {
    let secret_key = config.resolve_secret_key()?;
    let store = crate::store::Store::open(&config.database).await?;
    // Fail here rather than mid-deploy if the key does not match what this
    // database's ciphertexts were written under.
    store.verify_secret_key(&secret_key).await?;
    let bus = crate::events::Bus::new(config.log_buffer_lines);
    let config = Arc::new(config);

    tokio::fs::create_dir_all(config.workspace_dir()).await?;
    tokio::fs::create_dir_all(config.upload_dir()).await?;

    let context = api::Context::new(store.clone(), bus, secret_key, config.clone());

    spawn_reachability_probe(context.clone(), config.probe_interval_seconds);
    spawn_terminal_sweeper(store.clone());
    spawn_log_tailers(context.clone());
    spawn_update_check(&config, store.clone());
    reconcile_interrupted_deployments(&store).await;

    // Health reporting so a container orchestrator or a load balancer can tell
    // whether this process is ready, without a bespoke endpoint.
    let (health_reporter, health_service) = tonic_health::server::health_reporter();
    health_reporter
        .set_serving::<TargetsServer<api::TargetsService>>()
        .await;

    if config.require_api_token {
        tracing::info!("every gRPC call requires an API token");
    } else {
        tracing::warn!(
            "the gRPC API accepts unauthenticated calls. That is fine while it is \
             bound to loopback with only the dashboard in front of it; set \
             --require-api-token (NUDO_REQUIRE_API_TOKEN=true) if anything else \
             can reach it."
        );
    }

    tracing::info!(%addr, database = %config.database.display(), "nudo control plane listening");

    let mut builder = tonic::transport::Server::builder().layer(tower::util::option_layer(
        config
            .require_api_token
            .then(|| crate::auth::RequireApiToken::new(store.clone())),
    ));

    builder
        .add_service(health_service)
        .add_service(TargetsServer::new(api::TargetsService::new(
            context.clone(),
        )))
        .add_service(ServicesApiServer::new(api::ServicesApiService::new(
            context.clone(),
        )))
        .add_service(DeploymentsServer::new(api::DeploymentsService::new(
            context.clone(),
        )))
        .add_service(LogsServer::new(api::LogsService::new(context.clone())))
        .add_service(TerminalsServer::new(api::TerminalsService::new(
            context.clone(),
        )))
        .add_service(SourcesServer::new(api::SourcesService::new(
            context.clone(),
        )))
        .add_service(SecretsServer::new(api::SecretsService::new(
            context.clone(),
        )))
        .add_service(AuditServer::new(api::AuditService::new(context)))
        .serve_with_shutdown(addr, shutdown_signal())
        .await?;

    tracing::info!("shut down cleanly");
    Ok(())
}

/// Periodically probes each target so the dashboard shows reachability without
/// the user having to click Check.
fn spawn_reachability_probe(context: api::Context, interval_seconds: u64) {
    if interval_seconds == 0 {
        tracing::info!("reachability probing is disabled");
        return;
    }

    tokio::spawn(async move {
        let mut ticker =
            tokio::time::interval(std::time::Duration::from_secs(interval_seconds.max(10)));
        // The first tick fires immediately, which would probe every host at
        // startup before anything is configured.
        ticker.tick().await;

        loop {
            ticker.tick().await;

            let targets = match context.store.list_targets("", 500, 0).await {
                Ok(targets) => targets,
                Err(error) => {
                    tracing::warn!(%error, "listing targets for the reachability probe failed");
                    continue;
                }
            };

            for target in targets {
                // A target with no key cannot be probed, and saying so every
                // minute would be noise.
                if target.ssh_key_id.trim().is_empty() {
                    continue;
                }

                let Ok(ssh_target) = context.engine.ssh_target_for(&target).await else {
                    continue;
                };

                // One cheap command, not the full four-check probe: this runs on
                // a timer against hosts that are supposed to be doing nothing
                // else.
                let status = match crate::ssh::SshSession::connect(&ssh_target).await {
                    Ok(session) => {
                        let reachable = session
                            .exec_timeout("true", std::time::Duration::from_secs(10))
                            .await
                            .map(|result| result.ok())
                            .unwrap_or(false);
                        let _ = session.close().await;
                        if reachable {
                            nudo_proto::target::Status::Reachable
                        } else {
                            nudo_proto::target::Status::Unreachable
                        }
                    }
                    Err(_) => nudo_proto::target::Status::Unreachable,
                };

                if let Err(error) = context.store.set_target_status(&target.id, status).await {
                    tracing::warn!(%error, target = %target.name, "recording status failed");
                }
            }
        }
    });
}

/// Keeps each service's log ring buffer warm, so a freshly opened log view shows
/// history immediately rather than waiting for the next line.
///
/// A tail is started only while something is watching, and stops when the last
/// viewer goes away — an idle control plane holds no SSH connection to a
/// latency-critical box just to buffer logs it nobody is reading.
fn spawn_log_tailers(context: api::Context) {
    tokio::spawn(async move {
        // Which services already have a tail running, so a tick does not start a
        // second one.
        let mut running: std::collections::HashSet<String> = std::collections::HashSet::new();
        let mut ticker = tokio::time::interval(std::time::Duration::from_secs(15));

        loop {
            ticker.tick().await;

            let services = match context.store.list_services("", 500, 0).await {
                Ok(services) => services,
                Err(error) => {
                    tracing::warn!(%error, "listing services for the log tailers failed");
                    continue;
                }
            };

            // Drop finished tails so a service whose viewer returns is tailed
            // again.
            running.retain(|id| context.bus.has_log_watchers(id));

            for service in services {
                if running.contains(&service.id) || !context.bus.has_log_watchers(&service.id) {
                    continue;
                }

                let Ok(target) = context.require_target(&service.target_id).await else {
                    continue;
                };
                let Ok(session) = context.connect(&target).await else {
                    continue;
                };

                running.insert(service.id.clone());
                let unit_name = crate::systemd::unit_file_name(&service);
                let bus = context.bus.clone();
                let service_id = service.id.clone();

                tokio::spawn(async move {
                    if let Err(error) =
                        crate::logs::tail_into_buffer(&session, &bus, &service_id, &unit_name).await
                    {
                        tracing::debug!(%error, service = %service_id, "the log tail ended");
                    }
                    let _ = session.close().await;
                });
            }
        }
    });
}

/// Removes consumed and expired terminal grants so the table does not grow
/// without bound.
/// Starts the periodic release check, if it is enabled.
///
/// Deliberately not fatal when disabled or when the network is unavailable: an
/// instance that cannot reach the manifest is a working instance that does not
/// know about new releases, which is a far smaller problem than one that will
/// not start.
fn spawn_update_check(config: &Config, store: crate::store::Store) {
    let checker = crate::updates::UpdateChecker::new(
        store,
        config.update_manifest_url.clone(),
        config.check_for_updates,
    );
    // `spawn` itself is a no-op when the check is off, so there is no second
    // guard here.
    crate::updates::spawn(
        checker,
        std::time::Duration::from_secs(config.update_interval_hours.max(1) * 3600),
    );
}

fn spawn_terminal_sweeper(store: crate::store::Store) {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(std::time::Duration::from_secs(300));
        loop {
            ticker.tick().await;
            match store.sweep_terminal_sessions().await {
                Ok(0) => {}
                Ok(count) => tracing::debug!(count, "swept terminal grants"),
                Err(error) => tracing::warn!(%error, "sweeping terminal grants failed"),
            }
        }
    });
}

/// Marks deployments that were running when the process died as failed.
///
/// Their engine task is gone, so leaving them "building" forever would make the
/// dashboard show work that will never finish and block a new deploy behind an
/// apparently in-flight one.
async fn reconcile_interrupted_deployments(store: &crate::store::Store) {
    let active = match store.active_deployments().await {
        Ok(active) => active,
        Err(error) => {
            tracing::warn!(%error, "listing interrupted deployments failed");
            return;
        }
    };

    for deployment in active {
        tracing::warn!(
            deployment = %deployment.id,
            "marking a deployment interrupted by a restart as failed"
        );
        let _ = store
            .set_deployment_error(
                &deployment.id,
                "the control plane restarted while this deployment was running; \
                 its outcome on the target is unknown — check the unit's status",
            )
            .await;
        let _ = store
            .set_deployment_status(&deployment.id, nudo_proto::deployment::Status::Failed)
            .await;
    }
}

/// Resolves on SIGINT or SIGTERM.
async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };

    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut signal) => {
                signal.recv().await;
            }
            Err(error) => {
                tracing::warn!(%error, "could not install the SIGTERM handler");
                std::future::pending::<()>().await;
            }
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => tracing::info!("received SIGINT, shutting down"),
        _ = terminate => tracing::info!("received SIGTERM, shutting down"),
    }
}
