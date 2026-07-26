//! Integration tests for the gRPC surface.
//!
//! A real tonic server over a real socket, backed by a temporary SQLite
//! database, driven by the generated client. The service-level tests inside the
//! crate call handlers directly; these go through the wire, so they also cover
//! the codec, the streaming plumbing, and the status codes a client actually
//! sees.

use std::sync::Arc;

use nudo_proto::*;
use nudo_server::api;
use nudo_server::crypto::SecretKey;
use nudo_server::events::Bus;
use nudo_server::store::Store;
use tokio_stream::StreamExt;
use tonic::transport::Channel;

/// A running server, torn down when dropped.
struct Harness {
    endpoint: String,
    context: api::Context,
    _shutdown: tokio::sync::oneshot::Sender<()>,
    _dir: tempfile::TempDir,
}

impl Harness {
    /// Starts a server on an ephemeral port.
    async fn start() -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = Store::open(&dir.path().join("nudo.db"))
            .await
            .expect("store");

        let config = Arc::new(nudo_server::Config {
            data_dir: dir.path().to_path_buf(),
            ..nudo_server::Config::default()
        });
        let context = api::Context::new(store, Bus::default(), SecretKey::generate(), config);

        // Port 0, then read back what the OS assigned — so tests can run
        // concurrently without colliding on a fixed port.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let addr = listener.local_addr().expect("addr");
        let endpoint = format!("http://{addr}");

        let (shutdown, shutdown_rx) = tokio::sync::oneshot::channel();
        let serving = context.clone();

        tokio::spawn(async move {
            let incoming = tokio_stream::wrappers::TcpListenerStream::new(listener);
            let _ = tonic::transport::Server::builder()
                .add_service(targets_server::TargetsServer::new(
                    api::TargetsService::new(serving.clone()),
                ))
                .add_service(services_api_server::ServicesApiServer::new(
                    api::ServicesApiService::new(serving.clone()),
                ))
                .add_service(deployments_server::DeploymentsServer::new(
                    api::DeploymentsService::new(serving.clone()),
                ))
                .add_service(logs_server::LogsServer::new(api::LogsService::new(
                    serving.clone(),
                )))
                .add_service(terminals_server::TerminalsServer::new(
                    api::TerminalsService::new(serving.clone()),
                ))
                .add_service(sources_server::SourcesServer::new(
                    api::SourcesService::new(serving.clone()),
                ))
                .add_service(secrets_server::SecretsServer::new(
                    api::SecretsService::new(serving.clone()),
                ))
                .add_service(audit_server::AuditServer::new(api::AuditService::new(
                    serving,
                )))
                .serve_with_incoming_shutdown(incoming, async {
                    let _ = shutdown_rx.await;
                })
                .await;
        });

        // Wait for the listener to be accepting rather than sleeping a fixed
        // amount.
        for _ in 0..100 {
            if Channel::from_shared(endpoint.clone())
                .expect("endpoint")
                .connect()
                .await
                .is_ok()
            {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }

        Self {
            endpoint,
            context,
            _shutdown: shutdown,
            _dir: dir,
        }
    }

    async fn channel(&self) -> Channel {
        Channel::from_shared(self.endpoint.clone())
            .expect("endpoint")
            .connect()
            .await
            .expect("connect")
    }

    async fn targets(&self) -> targets_client::TargetsClient<Channel> {
        targets_client::TargetsClient::new(self.channel().await)
    }

    async fn services(&self) -> services_api_client::ServicesApiClient<Channel> {
        services_api_client::ServicesApiClient::new(self.channel().await)
    }

    async fn deployments(&self) -> deployments_client::DeploymentsClient<Channel> {
        deployments_client::DeploymentsClient::new(self.channel().await)
    }

    async fn secrets(&self) -> secrets_client::SecretsClient<Channel> {
        secrets_client::SecretsClient::new(self.channel().await)
    }

    async fn logs(&self) -> logs_client::LogsClient<Channel> {
        logs_client::LogsClient::new(self.channel().await)
    }

    async fn audit(&self) -> audit_client::AuditClient<Channel> {
        audit_client::AuditClient::new(self.channel().await)
    }

    async fn terminals(&self) -> terminals_client::TerminalsClient<Channel> {
        terminals_client::TerminalsClient::new(self.channel().await)
    }
}

/// A human's mutation envelope.
fn human() -> Mutation {
    Mutation::by(Actor::human("usr_1", "alice"))
}

/// An agent's, which is what the guardrail exists for.
fn agent() -> Mutation {
    Mutation::by(Actor::agent("sess_1", "claude"))
}

/// Creates a target over the wire.
async fn create_target(harness: &Harness, name: &str, latency_critical: bool) -> Target {
    let mut mutation = human();
    // Creating a latency-critical target is itself the acknowledgement.
    mutation.allow_latency_critical = latency_critical;

    harness
        .targets()
        .await
        .create(CreateTargetRequest {
            mutation: Some(mutation),
            name: name.to_string(),
            host: "10.0.0.5".to_string(),
            port: 22,
            user: "root".to_string(),
            ssh_key_id: String::new(),
            latency_critical,
            labels: Default::default(),
        })
        .await
        .expect("create the target")
        .into_inner()
}

/// Creates a service over the wire.
///
/// Carries the latency-critical override, because a fixture existing to set up
/// other assertions should not be the thing that trips the guardrail — the tests
/// that care about it exercise it directly.
async fn create_service(harness: &Harness, target_id: &str, name: &str) -> Service {
    harness
        .services()
        .await
        .create(CreateServiceRequest {
            mutation: Some(Mutation {
                actor: Some(Actor::human("usr_1", "alice")),
                allow_latency_critical: true,
                ..Default::default()
            }),
            service: Some(Service {
                target_id: target_id.to_string(),
                name: name.to_string(),
                unit: Some(SystemdUnit {
                    unit_name: format!("{name}.service"),
                    cpu_affinity: "2-5".to_string(),
                    nice: "-10".to_string(),
                    ..Default::default()
                }),
                ..Default::default()
            }),
        })
        .await
        .expect("create the service")
        .into_inner()
}

#[tokio::test]
async fn a_target_round_trips_over_the_wire() {
    let harness = Harness::start().await;
    let created = create_target(&harness, "edge-1", false).await;

    assert!(created.id.starts_with("tgt_"));
    assert_eq!(created.name, "edge-1");

    let fetched = harness
        .targets()
        .await
        .get(GetTargetRequest { id: created.id.clone() })
        .await
        .expect("get")
        .into_inner();
    assert_eq!(fetched.id, created.id);

    let listed = harness
        .targets()
        .await
        .list(ListTargetsRequest::default())
        .await
        .expect("list")
        .into_inner();
    assert_eq!(listed.targets.len(), 1);
}

#[tokio::test]
async fn a_missing_target_is_not_found_at_the_client() {
    let harness = Harness::start().await;
    let status = harness
        .targets()
        .await
        .get(GetTargetRequest {
            id: "tgt_nope".to_string(),
        })
        .await
        .expect_err("must fail");

    assert_eq!(status.code(), tonic::Code::NotFound);
    assert!(status.message().contains("no such target"));
}

#[tokio::test]
async fn the_latency_critical_guardrail_is_enforced_over_the_wire() {
    // The property the whole tool turns on: nothing touches the hot-path box
    // without saying so, and an agent gets a message telling it exactly what to
    // set if the action is sanctioned.
    let harness = Harness::start().await;
    let hot = create_target(&harness, "hft-box", true).await;
    let service = create_service(&harness, &hot.id, "hft").await;

    let refused = harness
        .deployments()
        .await
        .deploy(DeployRequest {
            mutation: Some(agent()),
            service_id: service.id.clone(),
            ..Default::default()
        })
        .await
        .expect_err("must be refused");

    assert_eq!(refused.code(), tonic::Code::FailedPrecondition);
    assert!(refused.message().contains("latency-critical"));
    assert!(refused.message().contains("allow_latency_critical"));

    // Nothing was queued.
    let history = harness
        .deployments()
        .await
        .list(ListDeploymentsRequest::default())
        .await
        .expect("list")
        .into_inner();
    assert!(history.deployments.is_empty());

    // The refusal is audited, so an operator can see the agent tried.
    let audit = harness
        .audit()
        .await
        .list(ListAuditRequest {
            actor_kind: actor::Kind::Agent as i32,
            ..Default::default()
        })
        .await
        .expect("audit")
        .into_inner();
    assert!(
        audit.entries.iter().any(|e| e.action.contains("refused")),
        "a refused mutation should still be recorded"
    );
}

#[tokio::test]
async fn a_dry_run_deploy_changes_nothing() {
    let harness = Harness::start().await;
    let target = create_target(&harness, "box", false).await;
    let service = create_service(&harness, &target.id, "bot").await;

    let planned = harness
        .deployments()
        .await
        .deploy(DeployRequest {
            mutation: Some(Mutation {
                actor: Some(Actor::agent("sess_1", "claude")),
                dry_run: true,
                ..Default::default()
            }),
            service_id: service.id.clone(),
            ..Default::default()
        })
        .await
        .expect("dry run")
        .into_inner();

    assert!(planned.id.is_empty(), "a dry run must not queue anything");
    assert!(
        harness
            .deployments()
            .await
            .list(ListDeploymentsRequest::default())
            .await
            .expect("list")
            .into_inner()
            .deployments
            .is_empty()
    );
}

#[tokio::test]
async fn an_idempotency_key_makes_a_retried_deploy_safe() {
    // A CI job whose connection dropped will retry; it must not deploy twice.
    let harness = Harness::start().await;
    let target = create_target(&harness, "box", false).await;
    let service = create_service(&harness, &target.id, "bot").await;

    let request = || DeployRequest {
        mutation: Some(Mutation {
            actor: Some(Actor::human("usr_1", "alice")),
            idempotency_key: "ci-run-7".to_string(),
            ..Default::default()
        }),
        service_id: service.id.clone(),
        ..Default::default()
    };

    let first = harness
        .deployments()
        .await
        .deploy(request())
        .await
        .expect("first")
        .into_inner();
    let second = harness
        .deployments()
        .await
        .deploy(request())
        .await
        .expect("second")
        .into_inner();

    assert_eq!(first.id, second.id);
    assert_eq!(
        harness
            .deployments()
            .await
            .list(ListDeploymentsRequest::default())
            .await
            .expect("list")
            .into_inner()
            .deployments
            .len(),
        1
    );
}

#[tokio::test]
async fn the_rendered_unit_comes_back_over_the_wire_with_its_latency_knobs() {
    let harness = Harness::start().await;
    let target = create_target(&harness, "box", false).await;
    let service = create_service(&harness, &target.id, "bot").await;

    let rendered = harness
        .services()
        .await
        .render_unit(RenderUnitRequest {
            service_id: service.id,
        })
        .await
        .expect("render")
        .into_inner()
        .unit_file;

    assert!(rendered.contains("[Unit]"));
    assert!(rendered.contains("ExecStart=/opt/bot/current/bin"));
    // The reason this tool exists rather than Docker.
    assert!(rendered.contains("CPUAffinity=2-5"));
    assert!(rendered.contains("Nice=-10"));
    assert!(rendered.contains("WantedBy=multi-user.target"));
}

#[tokio::test]
async fn a_secrets_value_never_comes_back_over_the_wire() {
    let harness = Harness::start().await;

    let stored = harness
        .secrets()
        .await
        .put(PutSecretRequest {
            mutation: Some(human()),
            name: "API_KEY".to_string(),
            value: "super-secret-value".to_string(),
            ..Default::default()
        })
        .await
        .expect("put")
        .into_inner();

    // Only a digest.
    assert_eq!(
        stored.digest,
        nudo_server::crypto::sha256_hex("super-secret-value")
    );
    assert!(!format!("{stored:?}").contains("super-secret-value"));

    let listed = harness
        .secrets()
        .await
        .list(ListSecretsRequest::default())
        .await
        .expect("list")
        .into_inner();
    assert!(
        !format!("{listed:?}").contains("super-secret-value"),
        "there must be no read path for a secret value"
    );
}

#[tokio::test]
async fn a_secret_name_that_is_not_a_valid_environment_variable_is_refused() {
    let harness = Harness::start().await;
    let status = harness
        .secrets()
        .await
        .put(PutSecretRequest {
            mutation: Some(human()),
            name: "has space".to_string(),
            value: "v".to_string(),
            ..Default::default()
        })
        .await
        .expect_err("must be refused");
    assert_eq!(status.code(), tonic::Code::InvalidArgument);
}

#[tokio::test]
async fn a_terminal_grant_carries_a_token_but_never_a_host() {
    let harness = Harness::start().await;
    let target = create_target(&harness, "box", false).await;

    let session = harness
        .terminals()
        .await
        .create_session(CreateTerminalSessionRequest {
            mutation: Some(human()),
            target_id: target.id,
            initial_command: String::new(),
            cols: 120,
            rows: 40,
        })
        .await
        .expect("create the session")
        .into_inner();

    assert!(!session.token.is_empty());
    assert!(session.expires_at.is_some());
    // The client is never told which host it will reach.
    assert!(!session.websocket_url.contains("10.0.0.5"));
    // And the token is not in the URL, so it stays out of history and logs.
    assert!(!session.websocket_url.contains(&session.token));
}

#[tokio::test]
async fn attaching_without_an_attach_frame_first_is_refused() {
    // Input must not reach a PTY that has not been authorized.
    let harness = Harness::start().await;

    let (outbound, outbound_rx) = tokio::sync::mpsc::channel(4);
    outbound
        .send(TerminalClientMessage {
            message: Some(terminal_client_message::Message::Stdin(b"ls\n".to_vec())),
        })
        .await
        .expect("send");
    drop(outbound);

    let result = harness
        .terminals()
        .await
        .attach(tokio_stream::wrappers::ReceiverStream::new(outbound_rx))
        .await;

    let status = match result {
        Ok(_) => panic!("stdin before an attach must be refused"),
        Err(status) => status,
    };
    assert_eq!(status.code(), tonic::Code::InvalidArgument);
}

#[tokio::test]
async fn attaching_with_a_forged_token_is_unauthenticated() {
    let harness = Harness::start().await;

    let (outbound, outbound_rx) = tokio::sync::mpsc::channel(4);
    outbound
        .send(TerminalClientMessage {
            message: Some(terminal_client_message::Message::Attach(TerminalSession {
                id: "term_guessed".to_string(),
                token: "guessed-token".to_string(),
                ..Default::default()
            })),
        })
        .await
        .expect("send");
    drop(outbound);

    let result = harness
        .terminals()
        .await
        .attach(tokio_stream::wrappers::ReceiverStream::new(outbound_rx))
        .await;

    let status = match result {
        Ok(_) => panic!("a forged token must not attach"),
        Err(status) => status,
    };
    assert_eq!(status.code(), tonic::Code::Unauthenticated);
}

#[tokio::test]
async fn a_finished_deployments_history_streams_back_and_ends() {
    // The stream has to terminate with a verdict, or a CLI waiting on it hangs.
    let harness = Harness::start().await;
    let target = create_target(&harness, "box", false).await;
    let service = create_service(&harness, &target.id, "bot").await;

    let deployment = harness
        .context
        .store
        .create_deployment(&nudo_server::store::NewDeployment {
            service_id: service.id.clone(),
            actor: Actor::human("usr_1", "alice"),
            previous_release_id: String::new(),
            git_ref: String::new(),
            trigger: nudo_server::store::DeployTrigger::Manual,
        })
        .await
        .expect("create");

    harness
        .context
        .store
        .append_deployment_log(&deployment.id, "compiling", false)
        .await
        .expect("log");
    harness
        .context
        .store
        .set_deployment_status(&deployment.id, deployment::Status::Succeeded)
        .await
        .expect("finish");

    let mut stream = harness
        .deployments()
        .await
        .watch(WatchDeploymentRequest {
            deployment_id: deployment.id.clone(),
        })
        .await
        .expect("watch")
        .into_inner();

    let mut lines = Vec::new();
    let mut verdict = None;
    while let Some(event) = stream.next().await {
        match event.expect("event").event {
            Some(deployment_event::Event::OutputLine(line)) => lines.push(line),
            Some(deployment_event::Event::TerminalState(state)) => verdict = Some(state),
            _ => {}
        }
    }

    assert_eq!(lines, vec!["compiling"]);
    assert_eq!(
        verdict.expect("a verdict").status,
        deployment::Status::Succeeded as i32
    );
}

#[tokio::test]
async fn a_run_command_dry_run_streams_the_plan_and_an_exit_code() {
    let harness = Harness::start().await;
    let target = create_target(&harness, "box", false).await;

    let mut stream = harness
        .logs()
        .await
        .run_command(RunCommandRequest {
            mutation: Some(Mutation {
                actor: Some(Actor::agent("sess_1", "claude")),
                dry_run: true,
                ..Default::default()
            }),
            target_id: target.id,
            command: "systemctl".to_string(),
            args: vec!["restart".to_string(), "bot.service".to_string()],
            timeout_seconds: 30,
        })
        .await
        .expect("run")
        .into_inner();

    let mut output = String::new();
    let mut exit_code = None;
    while let Some(chunk) = stream.next().await {
        match chunk.expect("chunk").chunk {
            Some(command_output::Chunk::Stdout(bytes)) => {
                output.push_str(&String::from_utf8_lossy(&bytes));
            }
            Some(command_output::Chunk::ExitCode(code)) => exit_code = Some(code),
            _ => {}
        }
    }

    assert!(output.contains("dry run"));
    assert!(output.contains("systemctl restart bot.service"));
    // The exit code terminates the stream, as the proto documents.
    assert_eq!(exit_code, Some(0));
}

#[tokio::test]
async fn a_rollback_with_nothing_to_roll_back_to_is_a_precondition_failure() {
    let harness = Harness::start().await;
    let target = create_target(&harness, "box", false).await;
    let service = create_service(&harness, &target.id, "bot").await;

    let status = harness
        .deployments()
        .await
        .rollback(RollbackRequest {
            mutation: Some(human()),
            service_id: service.id,
            release_id: String::new(),
        })
        .await
        .expect_err("must fail");
    assert_eq!(status.code(), tonic::Code::FailedPrecondition);
}

#[tokio::test]
async fn the_audit_log_records_every_mutation_with_its_actor() {
    let harness = Harness::start().await;
    let target = create_target(&harness, "box", false).await;
    create_service(&harness, &target.id, "bot").await;

    harness
        .secrets()
        .await
        .put(PutSecretRequest {
            mutation: Some(agent()),
            name: "AGENT_KEY".to_string(),
            value: "v".to_string(),
            ..Default::default()
        })
        .await
        .expect("put");

    let all = harness
        .audit()
        .await
        .list(ListAuditRequest::default())
        .await
        .expect("audit")
        .into_inner();

    let actions: Vec<&str> = all.entries.iter().map(|e| e.action.as_str()).collect();
    assert!(actions.contains(&"Targets.Create"));
    assert!(actions.contains(&"ServicesApi.Create"));
    assert!(actions.contains(&"Secrets.Put"));

    // And it can be narrowed to what the agent did.
    let by_agent = harness
        .audit()
        .await
        .list(ListAuditRequest {
            actor_kind: actor::Kind::Agent as i32,
            ..Default::default()
        })
        .await
        .expect("audit")
        .into_inner();
    assert_eq!(by_agent.entries.len(), 1);
    assert_eq!(by_agent.entries[0].action, "Secrets.Put");
    // The name, never the value.
    assert!(by_agent.entries[0].summary.contains("AGENT_KEY"));
    assert!(!by_agent.entries[0].summary.contains("value"));
}

#[tokio::test]
async fn listing_paginates_over_the_wire() {
    let harness = Harness::start().await;
    for i in 0..5 {
        create_target(&harness, &format!("box-{i}"), false).await;
    }

    let first = harness
        .targets()
        .await
        .list(ListTargetsRequest {
            page_size: 2,
            ..Default::default()
        })
        .await
        .expect("list")
        .into_inner();
    assert_eq!(first.targets.len(), 2);
    assert!(!first.next_page_token.is_empty());

    let second = harness
        .targets()
        .await
        .list(ListTargetsRequest {
            page_size: 2,
            page_token: first.next_page_token,
            ..Default::default()
        })
        .await
        .expect("list")
        .into_inner();
    assert!(
        first
            .targets
            .iter()
            .all(|a| second.targets.iter().all(|b| a.id != b.id))
    );
}

#[tokio::test]
async fn a_service_cannot_be_created_on_a_target_that_does_not_exist() {
    let harness = Harness::start().await;
    let status = harness
        .services()
        .await
        .create(CreateServiceRequest {
            mutation: Some(human()),
            service: Some(Service {
                target_id: "tgt_nope".to_string(),
                name: "orphan".to_string(),
                ..Default::default()
            }),
        })
        .await
        .expect_err("must fail");
    assert_eq!(status.code(), tonic::Code::NotFound);
}

#[tokio::test]
async fn deleting_a_target_removes_its_services_over_the_wire() {
    let harness = Harness::start().await;
    let target = create_target(&harness, "doomed", false).await;
    let service = create_service(&harness, &target.id, "bot").await;

    harness
        .targets()
        .await
        .delete(DeleteTargetRequest {
            mutation: Some(human()),
            id: target.id,
        })
        .await
        .expect("delete");

    let status = harness
        .services()
        .await
        .get(GetServiceRequest { id: service.id })
        .await
        .expect_err("must be gone");
    assert_eq!(status.code(), tonic::Code::NotFound);
}

#[tokio::test]
async fn an_update_applies_only_its_masked_fields_over_the_wire() {
    let harness = Harness::start().await;
    let target = create_target(&harness, "renameable", false).await;

    let updated = harness
        .targets()
        .await
        .update(UpdateTargetRequest {
            mutation: Some(human()),
            id: target.id.clone(),
            target: Some(Target {
                name: "renamed".to_string(),
                host: "192.168.1.1".to_string(),
                ..Default::default()
            }),
            update_mask: vec!["name".to_string()],
        })
        .await
        .expect("update")
        .into_inner();

    assert_eq!(updated.name, "renamed");
    assert_eq!(updated.host, "10.0.0.5", "outside the mask");
}
