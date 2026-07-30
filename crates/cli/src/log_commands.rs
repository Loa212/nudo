use super::*;

pub(super) async fn logs(
    cli: &Cli,
    service: &str,
    follow: bool,
    lines: u32,
    grep: Option<&str>,
) -> anyhow::Result<()> {
    let mut client = cli.client()?.logs();

    let mut stream = client
        .stream(StreamLogsRequest {
            service_id: service.to_string(),
            follow,
            tail_lines: lines,
            since_cursor: String::new(),
            since: None,
            grep: grep.unwrap_or_default().to_string(),
        })
        .await?
        .into_inner();

    while let Some(line) = stream.next().await {
        let line = line?;
        match cli.output {
            Output::Json => println!("{}", serde_json::to_string(&JsonLogLine::from(&line))?),
            Output::Table => {
                let when = nudo_proto::from_timestamp(line.at.as_ref().unwrap_or(&Timestamp {
                    seconds: 0,
                    nanos: 0,
                }))
                .map(|t| t.format("%H:%M:%S").to_string())
                .unwrap_or_else(|| "--:--:--".to_string());
                println!("{when}  {}", line.message);
            }
        }
    }

    Ok(())
}

pub(super) async fn exec(
    cli: &Cli,
    target: &str,
    command: &[String],
    timeout: u32,
) -> anyhow::Result<()> {
    let mut client = cli.client()?.logs();

    let (program, args) = command
        .split_first()
        .ok_or_else(|| anyhow!("a command is required"))?;

    let mut stream = client
        .run_command(RunCommandRequest {
            mutation: Some(mutation(cli)),
            target_id: target.to_string(),
            command: program.clone(),
            args: args.to_vec(),
            timeout_seconds: timeout,
        })
        .await?
        .into_inner();

    let mut exit_code = -1;
    while let Some(chunk) = stream.next().await {
        match chunk?.chunk {
            Some(command_output::Chunk::Stdout(bytes)) => {
                print!("{}", String::from_utf8_lossy(&bytes));
            }
            Some(command_output::Chunk::Stderr(bytes)) => {
                eprint!("{}", String::from_utf8_lossy(&bytes));
            }
            Some(command_output::Chunk::ExitCode(code)) => exit_code = code,
            None => {}
        }
    }

    // The remote command's status becomes ours, so `nudo exec ... && next` works.
    if exit_code != 0 {
        std::process::exit(if exit_code < 0 { 1 } else { exit_code });
    }
    Ok(())
}
