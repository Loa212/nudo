use super::*;

#[allow(clippy::too_many_arguments)]
pub(super) async fn deploy(
    cli: &Cli,
    service: &str,
    git_ref: Option<&str>,
    artifact_url: Option<&str>,
    artifact: Option<&std::path::Path>,
    skip_health_check: bool,
    wait: bool,
) -> anyhow::Result<()> {
    // A locally built binary is served over a short-lived loopback listener and
    // handed to the control plane as a URL. That reuses the artifact-fetch path
    // rather than adding an upload RPC, and the binary is streamed rather than
    // staged anywhere on the way.
    let upload = match artifact {
        Some(path) => Some(ArtifactServer::start(path).await?),
        None => None,
    };

    let effective_url = match (&upload, artifact_url) {
        (Some(server), _) => server.url.clone(),
        (None, Some(url)) => url.to_string(),
        (None, None) => String::new(),
    };

    if let Some(server) = &upload {
        println!(
            "serving {} ({} bytes) to the control plane at {}",
            server.name, server.size, server.url
        );
    }

    let mut client = cli.client()?.deployments();

    let deployment = client
        .deploy(DeployRequest {
            mutation: Some(mutation(cli)),
            service_id: service.to_string(),
            git_ref: git_ref.unwrap_or_default().to_string(),
            artifact_url: effective_url,
            skip_health_check,
            // Rolling back on a failed health check is the behaviour that
            // makes this safe to run from CI unattended.
            auto_rollback_on_failure: true,
        })
        .await?
        .into_inner();

    if cli.dry_run {
        println!("dry run: would deploy {service}");
        if !deployment.previous_release_id.is_empty() {
            println!("  would roll back to {}", deployment.previous_release_id);
        }
        return Ok(());
    }

    println!("deployment {} queued", deployment.id);

    if wait {
        // The listener must outlive the deploy, since the control plane fetches
        // the artifact partway through it.
        return follow_deployment(cli, &deployment.id).await;
    }

    if upload.is_some() {
        // Without --wait there is nobody left to serve the file once this
        // process exits, so pushing a local binary requires waiting.
        bail!(
            "pushing a local binary needs --wait, because the control plane \
             fetches it from this process while the deploy runs"
        );
    }

    println!("follow it with: nudo services deployments {service}");
    Ok(())
}

/// A one-shot HTTP listener that serves a single local file.
///
/// Bound to loopback and closed when dropped, so the binary is reachable only
/// for the length of the deploy and only from this machine. A random path means
/// another local process cannot guess the URL.
pub(super) struct ArtifactServer {
    pub(super) url: String,
    pub(super) name: String,
    pub(super) size: u64,
    _shutdown: tokio::sync::oneshot::Sender<()>,
}

impl ArtifactServer {
    pub(super) async fn start(path: &std::path::Path) -> anyhow::Result<Self> {
        use tokio::io::AsyncWriteExt as _;

        let bytes = tokio::fs::read(path)
            .await
            .with_context(|| format!("reading {}", path.display()))?;
        if bytes.is_empty() {
            bail!("{} is empty", path.display());
        }

        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "artifact".to_string());
        let size = bytes.len() as u64;

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .context("binding a local listener for the artifact")?;
        let port = listener.local_addr()?.port();

        // Unguessable, so another process on this machine cannot fetch it.
        let token = uuid::Uuid::new_v4().simple().to_string();
        let url = format!("http://127.0.0.1:{port}/{token}");

        let (shutdown, mut shutdown_rx) = tokio::sync::oneshot::channel();
        let expected_path = format!("/{token}");

        tokio::spawn(async move {
            loop {
                let accepted = tokio::select! {
                    _ = &mut shutdown_rx => return,
                    accepted = listener.accept() => accepted,
                };
                let Ok((mut socket, _)) = accepted else {
                    return;
                };

                let bytes = bytes.clone();
                let expected_path = expected_path.clone();
                tokio::spawn(async move {
                    use tokio::io::AsyncReadExt as _;

                    // Enough to read the request line; the rest is not needed.
                    let mut buffer = vec![0u8; 2048];
                    let read = socket.read(&mut buffer).await.unwrap_or(0);
                    let request = String::from_utf8_lossy(&buffer[..read]);
                    let wanted = request
                        .lines()
                        .next()
                        .and_then(|line| line.split_whitespace().nth(1))
                        .unwrap_or_default();

                    if wanted != expected_path {
                        let _ = socket
                            .write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n")
                            .await;
                        return;
                    }

                    let header = format!(
                        "HTTP/1.1 200 OK\r\n\
                         Content-Type: application/octet-stream\r\n\
                         Content-Length: {}\r\n\
                         Connection: close\r\n\r\n",
                        bytes.len()
                    );
                    let _ = socket.write_all(header.as_bytes()).await;
                    let _ = socket.write_all(&bytes).await;
                    let _ = socket.flush().await;
                });
            }
        });

        Ok(Self {
            url,
            name,
            size,
            _shutdown: shutdown,
        })
    }
}

pub(super) async fn rollback(
    cli: &Cli,
    service: &str,
    release: Option<&str>,
    wait: bool,
) -> anyhow::Result<()> {
    let mut client = cli.client()?.deployments();

    let deployment = client
        .rollback(RollbackRequest {
            mutation: Some(mutation(cli)),
            service_id: service.to_string(),
            release_id: release.unwrap_or_default().to_string(),
        })
        .await?
        .into_inner();

    if cli.dry_run {
        println!(
            "dry run: would roll {service} back to release {}",
            deployment.release_id
        );
        return Ok(());
    }

    println!(
        "rolling {service} back to release {}",
        deployment.release_id
    );

    if wait {
        return follow_deployment(cli, &deployment.id).await;
    }
    Ok(())
}

/// Streams a deployment to completion, exiting non-zero if it did not succeed.
///
/// This is the shape CI needs: the process's exit status has to reflect whether
/// the deploy actually worked, not merely whether it was accepted.
async fn follow_deployment(cli: &Cli, deployment_id: &str) -> anyhow::Result<()> {
    let mut client = cli.client()?.deployments();

    let mut stream = client
        .watch(WatchDeploymentRequest {
            deployment_id: deployment_id.to_string(),
        })
        .await?
        .into_inner();

    while let Some(event) = stream.next().await {
        let event = event?;
        match event.event {
            Some(deployment_event::Event::OutputLine(line)) => println!("{line}"),
            Some(deployment_event::Event::StatusChange(status)) => {
                let status =
                    deployment::Status::try_from(status).unwrap_or(deployment::Status::Unspecified);
                println!("--- {} ---", status.as_str());
            }
            Some(deployment_event::Event::TerminalState(state)) => {
                let status = deployment::Status::try_from(state.status)
                    .unwrap_or(deployment::Status::Unspecified);
                println!("--- {} ---", status.as_str());

                if !state.error.is_empty() {
                    eprintln!("{}", state.error);
                }

                return match status {
                    deployment::Status::Succeeded => Ok(()),
                    other => bail!("deployment {} ({})", other.as_str(), deployment_id),
                };
            }
            None => {}
        }
    }

    // The stream ended without a verdict, which is not something to report as
    // success.
    bail!("the deployment stream ended without a result")
}

// ---------------------------------------------------------------------------
// logs / exec / terminal
// ---------------------------------------------------------------------------
