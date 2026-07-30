use super::*;

pub(super) async fn terminal(cli: &Cli, target: &str, command: Option<&str>) -> anyhow::Result<()> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let mut terminals = cli.client()?.terminals();

    let (cols, rows) = terminal_size();
    let session = terminals
        .create_session(CreateTerminalSessionRequest {
            mutation: Some(mutation(cli)),
            target_id: target.to_string(),
            initial_command: command.unwrap_or_default().to_string(),
            cols,
            rows,
        })
        .await?
        .into_inner();

    if cli.dry_run {
        println!("dry run: would open a terminal on {target}");
        return Ok(());
    }

    // The first message attaches; the token is single-use, so this is the only
    // chance to spend it.
    let (outbound, outbound_rx) = tokio::sync::mpsc::channel(64);
    outbound
        .send(TerminalClientMessage {
            message: Some(terminal_client_message::Message::Attach(session)),
        })
        .await
        .map_err(|_| anyhow!("the terminal stream closed before attaching"))?;

    let mut inbound = terminals
        .attach(tokio_stream::wrappers::ReceiverStream::new(outbound_rx))
        .await?
        .into_inner();

    // Forward local stdin to the PTY.
    let stdin_forwarder = tokio::spawn(async move {
        let mut stdin = tokio::io::stdin();
        let mut buffer = vec![0u8; 4096];
        loop {
            match stdin.read(&mut buffer).await {
                Ok(0) => break,
                Ok(read) => {
                    let message = TerminalClientMessage {
                        message: Some(terminal_client_message::Message::Stdin(
                            buffer[..read].to_vec(),
                        )),
                    };
                    if outbound.send(message).await.is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    });

    let mut stdout = tokio::io::stdout();
    let mut exit_code = 0;

    while let Some(message) = inbound.next().await {
        match message?.message {
            Some(terminal_server_message::Message::Stdout(bytes)) => {
                stdout.write_all(&bytes).await?;
                stdout.flush().await?;
            }
            Some(terminal_server_message::Message::ExitCode(code)) => {
                exit_code = code;
                break;
            }
            Some(terminal_server_message::Message::Error(error)) => {
                bail!("{error}");
            }
            None => {}
        }
    }

    stdin_forwarder.abort();

    if exit_code != 0 {
        std::process::exit(exit_code);
    }
    Ok(())
}

/// The terminal's size, defaulting to a conventional 80x24.
///
/// Read from the environment rather than an ioctl so the CLI stays free of a
/// libc dependency; a wrong guess is corrected by the first resize.
pub(super) fn terminal_size() -> (u32, u32) {
    let read = |name: &str, fallback: u32| {
        std::env::var(name)
            .ok()
            .and_then(|v| v.parse::<u32>().ok())
            .filter(|v| *v > 0)
            .unwrap_or(fallback)
    };
    (read("COLUMNS", 80), read("LINES", 24))
}

// ---------------------------------------------------------------------------
// secrets / audit / sources
// ---------------------------------------------------------------------------
