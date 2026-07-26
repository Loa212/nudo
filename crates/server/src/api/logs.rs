//! The `Logs` service: journal streaming and one-shot commands.

use std::pin::Pin;

use futures_util::Stream;
use nudo_proto::logs_server::Logs;
use nudo_proto::*;
use tonic::{Request, Response, Status};

use super::Context;
use crate::logs::LogQuery;
use crate::ssh::join_command;
use crate::systemd;

/// Ceiling on a one-shot command's runtime, so an agent cannot hold a channel
/// open indefinitely.
const MAX_COMMAND_TIMEOUT_SECONDS: u32 = 600;

pub struct LogsService {
    context: Context,
}

impl LogsService {
    pub fn new(context: Context) -> Self {
        Self { context }
    }
}

#[tonic::async_trait]
impl Logs for LogsService {
    type StreamStream = Pin<Box<dyn Stream<Item = Result<LogLine, Status>> + Send + 'static>>;

    async fn stream(
        &self,
        request: Request<StreamLogsRequest>,
    ) -> Result<Response<Self::StreamStream>, Status> {
        let request = request.into_inner();
        let (service, target) = self
            .context
            .require_service_and_target(&request.service_id)
            .await?;
        let unit_name = systemd::unit_file_name(&service);

        let context = self.context.clone();
        let service_id = request.service_id.clone();

        let stream = async_stream::try_stream! {
            // Backfill from the ring buffer first, so a fresh page load is not
            // an empty pane while journalctl starts up.
            let buffered = if request.since_cursor.trim().is_empty() {
                let limit = if request.tail_lines == 0 {
                    100
                } else {
                    request.tail_lines.min(50_000) as usize
                };
                context.bus.backfill(&service_id, limit)
            } else {
                context
                    .bus
                    .backfill_after_cursor(&service_id, request.since_cursor.trim())
            };

            let grep = request.grep.trim().to_lowercase();
            let mut last_cursor = String::new();

            for event in buffered {
                if !grep.is_empty() && !event.message.to_lowercase().contains(&grep) {
                    continue;
                }
                last_cursor = event.cursor.clone();
                yield LogLine {
                    at: Some(nudo_proto::to_timestamp(event.at)),
                    message: event.message,
                    priority: event.priority,
                    unit: event.unit,
                    cursor: event.cursor,
                };
            }

            // A non-following read is satisfied by the journal itself, since the
            // ring buffer only holds what we have already seen.
            let session = context
                .connect(&target)
                .await
                .map_err(|status| Status::unavailable(status.message().to_string()))?;

            let query = LogQuery {
                follow: request.follow,
                tail_lines: request.tail_lines,
                // Resume from wherever the backfill left off, so the journal
                // read does not duplicate what was just sent.
                since_cursor: if last_cursor.is_empty() {
                    request.since_cursor.clone()
                } else {
                    last_cursor
                },
                since: request.since.as_ref().and_then(nudo_proto::from_timestamp),
                grep: request.grep.clone(),
            };

            let (tx, mut rx) = tokio::sync::mpsc::channel(256);
            let reader = {
                let unit_name = unit_name.clone();
                tokio::spawn(async move {
                    let _ = crate::logs::stream(&session, &unit_name, &query, tx).await;
                })
            };

            while let Some(event) = rx.recv().await {
                let event: crate::events::LogEvent = event;
                // Keep the buffer warm for the next viewer.
                context.bus.publish_log(&service_id, event.clone());
                yield LogLine {
                    at: Some(nudo_proto::to_timestamp(event.at)),
                    message: event.message,
                    priority: event.priority,
                    unit: event.unit,
                    cursor: event.cursor,
                };
            }

            reader.abort();
        };

        Ok(Response::new(Box::pin(stream)))
    }

    type RunCommandStream =
        Pin<Box<dyn Stream<Item = Result<CommandOutput, Status>> + Send + 'static>>;

    async fn run_command(
        &self,
        request: Request<RunCommandRequest>,
    ) -> Result<Response<Self::RunCommandStream>, Status> {
        let request = request.into_inner();
        let target = self.context.require_target(&request.target_id).await?;

        let command = request.command.trim();
        if command.is_empty() {
            return Err(Status::invalid_argument("a command is required"));
        }

        // Assembled with each argument quoted separately, so an argument
        // containing a semicolon is an argument and not a second command.
        let full_command = join_command(command, &request.args);

        let authorized = self
            .context
            .authorize(
                request.mutation.as_ref(),
                "Logs.RunCommand",
                &request.target_id,
                Some(&target),
                format!("ran `{full_command}` on {}", target.name),
            )
            .await?;

        let timeout = std::time::Duration::from_secs(match request.timeout_seconds {
            0 => 60,
            n => n.min(MAX_COMMAND_TIMEOUT_SECONDS),
        } as u64);

        if authorized.dry_run {
            // Report what would run and stop, without connecting.
            let preview = full_command.clone();
            let stream = async_stream::try_stream! {
                yield CommandOutput {
                    chunk: Some(command_output::Chunk::Stdout(
                        format!("dry run: would execute `{preview}` on {}\n", target.host)
                            .into_bytes(),
                    )),
                };
                yield CommandOutput {
                    chunk: Some(command_output::Chunk::ExitCode(0)),
                };
            };
            return Ok(Response::new(Box::pin(stream)));
        }

        let session = self.context.connect(&target).await?;

        let stream = async_stream::try_stream! {
            let (tx, mut rx) = tokio::sync::mpsc::channel(256);

            let runner = {
                let full_command = full_command.clone();
                tokio::spawn(async move {
                    let result = tokio::time::timeout(
                        timeout,
                        session.exec_streaming(&full_command, tx),
                    )
                    .await;

                    match result {
                        Ok(Ok(code)) => code,
                        // A timeout is a distinct outcome from a non-zero exit,
                        // so it gets its own code rather than being reported as
                        // a normal failure.
                        Ok(Err(_)) | Err(_) => -1,
                    }
                })
            };

            while let Some(line) = rx.recv().await {
                let line: crate::ssh::OutputLine = line;
                let bytes = format!("{}\n", line.text).into_bytes();
                yield CommandOutput {
                    chunk: Some(if line.stderr {
                        command_output::Chunk::Stderr(bytes)
                    } else {
                        command_output::Chunk::Stdout(bytes)
                    }),
                };
            }

            // The exit code is the final message, as the proto documents.
            let code = runner.await.unwrap_or(-1);
            yield CommandOutput {
                chunk: Some(command_output::Chunk::ExitCode(code)),
            };
        };

        Ok(Response::new(Box::pin(stream)))
    }
}

/// Extracts the error from a streaming call's result.
///
/// The success type is a boxed `Stream`, which has no `Debug`, so `expect_err`
/// cannot be used directly.
#[cfg(test)]
fn expect_status<T>(result: Result<tonic::Response<T>, Status>, what: &str) -> Status {
    match result {
        Ok(_) => panic!("expected an error: {what}"),
        Err(status) => status,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::Bus;
    use crate::store::{Store, TargetInput};
    use std::sync::Arc;
    use tokio_stream::StreamExt;

    async fn fixture() -> (LogsService, String, String) {
        let context = Context::new(
            Store::open_in_memory().await.expect("store"),
            Bus::default(),
            crate::crypto::SecretKey::generate(),
            Arc::new(crate::Config::default()),
        );
        let target = context
            .store
            .create_target(&TargetInput {
                name: "box".to_string(),
                host: "10.0.0.1".to_string(),
                ..Default::default()
            })
            .await
            .expect("target");
        let service = context
            .store
            .create_service(&Service {
                target_id: target.id.clone(),
                name: "bot".to_string(),
                ..Default::default()
            })
            .await
            .expect("service");
        (LogsService::new(context), target.id, service.id)
    }

    fn log_event(message: &str, cursor: &str) -> crate::events::LogEvent {
        crate::events::LogEvent {
            at: chrono::Utc::now(),
            message: message.to_string(),
            priority: "6".to_string(),
            unit: "bot.service".to_string(),
            cursor: cursor.to_string(),
        }
    }

    #[tokio::test]
    async fn streaming_logs_for_a_missing_service_is_not_found() {
        let (service, _, _) = fixture().await;
        let status = expect_status(
            service
                .stream(Request::new(StreamLogsRequest {
                    service_id: "svc_nope".to_string(),
                    ..Default::default()
                }))
                .await,
            "err",
        );
        assert_eq!(status.code(), tonic::Code::NotFound);
    }

    #[tokio::test]
    async fn buffered_lines_are_delivered_before_the_journal_is_read() {
        // This is what stops a fresh page load from showing an empty pane.
        let (service, _, service_id) = fixture().await;
        for i in 0..3 {
            service.context.bus.publish_log(
                &service_id,
                log_event(&format!("line-{i}"), &format!("c{i}")),
            );
        }

        let mut stream = service
            .stream(Request::new(StreamLogsRequest {
                service_id: service_id.clone(),
                tail_lines: 10,
                ..Default::default()
            }))
            .await
            .expect("stream")
            .into_inner();

        // The backfill arrives even though the target is unreachable, and the
        // stream then ends rather than erroring.
        let mut messages = Vec::new();
        while let Some(Ok(line)) = stream.next().await {
            messages.push(line.message);
            if messages.len() == 3 {
                break;
            }
        }
        assert_eq!(messages, vec!["line-0", "line-1", "line-2"]);
    }

    #[tokio::test]
    async fn a_grep_filters_the_backfill_too() {
        let (service, _, service_id) = fixture().await;
        service
            .context
            .bus
            .publish_log(&service_id, log_event("all good", "c1"));
        service
            .context
            .bus
            .publish_log(&service_id, log_event("ERROR: boom", "c2"));

        let mut stream = service
            .stream(Request::new(StreamLogsRequest {
                service_id: service_id.clone(),
                tail_lines: 10,
                grep: "error".to_string(),
                ..Default::default()
            }))
            .await
            .expect("stream")
            .into_inner();

        let first = stream.next().await.expect("line").expect("ok");
        // Case-insensitive, and the non-matching line is skipped.
        assert_eq!(first.message, "ERROR: boom");
    }

    #[tokio::test]
    async fn a_cursor_resumes_after_what_the_client_already_has() {
        let (service, _, service_id) = fixture().await;
        for i in 0..4 {
            service.context.bus.publish_log(
                &service_id,
                log_event(&format!("line-{i}"), &format!("c{i}")),
            );
        }

        let mut stream = service
            .stream(Request::new(StreamLogsRequest {
                service_id: service_id.clone(),
                since_cursor: "c1".to_string(),
                ..Default::default()
            }))
            .await
            .expect("stream")
            .into_inner();

        let mut messages = Vec::new();
        while let Some(Ok(line)) = stream.next().await {
            messages.push(line.message);
            if messages.len() == 2 {
                break;
            }
        }
        assert_eq!(messages, vec!["line-2", "line-3"], "no duplicates");
    }

    #[tokio::test]
    async fn a_command_is_required() {
        let (service, target_id, _) = fixture().await;
        let status = expect_status(
            service
                .run_command(Request::new(RunCommandRequest {
                    mutation: Some(Mutation::by(Actor::human("u", "alice"))),
                    target_id,
                    command: "   ".to_string(),
                    ..Default::default()
                }))
                .await,
            "err",
        );
        assert_eq!(status.code(), tonic::Code::InvalidArgument);
    }

    #[tokio::test]
    async fn running_a_command_on_a_missing_target_is_not_found() {
        let (service, _, _) = fixture().await;
        let status = expect_status(
            service
                .run_command(Request::new(RunCommandRequest {
                    mutation: Some(Mutation::by(Actor::human("u", "alice"))),
                    target_id: "tgt_nope".to_string(),
                    command: "uptime".to_string(),
                    ..Default::default()
                }))
                .await,
            "err",
        );
        assert_eq!(status.code(), tonic::Code::NotFound);
    }

    #[tokio::test]
    async fn a_command_against_a_latency_critical_target_is_refused_without_the_opt_in() {
        let (service, _, _) = fixture().await;
        let hot = service
            .context
            .store
            .create_target(&TargetInput {
                name: "hot-box".to_string(),
                host: "10.0.0.2".to_string(),
                latency_critical: true,
                ..Default::default()
            })
            .await
            .expect("target");

        let status = expect_status(
            service
                .run_command(Request::new(RunCommandRequest {
                    mutation: Some(Mutation::by(Actor::agent("s", "claude"))),
                    target_id: hot.id,
                    command: "systemctl".to_string(),
                    args: vec!["restart".to_string(), "hft".to_string()],
                    ..Default::default()
                }))
                .await,
            "must be refused",
        );
        assert_eq!(status.code(), tonic::Code::FailedPrecondition);
    }

    #[tokio::test]
    async fn a_dry_run_command_reports_what_it_would_run_without_connecting() {
        let (service, target_id, _) = fixture().await;
        let mut stream = service
            .run_command(Request::new(RunCommandRequest {
                mutation: Some(Mutation {
                    actor: Some(Actor::agent("s", "claude")),
                    dry_run: true,
                    ..Default::default()
                }),
                target_id,
                command: "systemctl".to_string(),
                args: vec!["restart".to_string(), "bot.service".to_string()],
                ..Default::default()
            }))
            .await
            .expect("dry run")
            .into_inner();

        let first = stream.next().await.expect("chunk").expect("ok");
        let text = match first.chunk {
            Some(command_output::Chunk::Stdout(bytes)) => {
                String::from_utf8_lossy(&bytes).into_owned()
            }
            other => panic!("expected stdout, got {other:?}"),
        };
        assert!(text.contains("dry run"));
        assert!(text.contains("systemctl restart bot.service"));

        // The exit code still terminates the stream, as the proto requires.
        let last = stream.next().await.expect("chunk").expect("ok");
        assert!(matches!(
            last.chunk,
            Some(command_output::Chunk::ExitCode(0))
        ));
    }

    #[tokio::test]
    async fn command_arguments_are_quoted_so_they_cannot_become_a_second_command() {
        let (service, target_id, _) = fixture().await;
        let mut stream = service
            .run_command(Request::new(RunCommandRequest {
                mutation: Some(Mutation {
                    actor: Some(Actor::agent("s", "claude")),
                    dry_run: true,
                    ..Default::default()
                }),
                target_id,
                command: "echo".to_string(),
                args: vec!["; rm -rf /".to_string()],
                ..Default::default()
            }))
            .await
            .expect("dry run")
            .into_inner();

        let first = stream.next().await.expect("chunk").expect("ok");
        let text = match first.chunk {
            Some(command_output::Chunk::Stdout(bytes)) => {
                String::from_utf8_lossy(&bytes).into_owned()
            }
            other => panic!("expected stdout, got {other:?}"),
        };
        assert!(
            text.contains("'; rm -rf /'"),
            "argument was not quoted: {text}"
        );
    }

    #[tokio::test]
    async fn a_command_is_recorded_in_the_audit_log_with_its_arguments() {
        let (service, target_id, _) = fixture().await;
        let _ = service
            .run_command(Request::new(RunCommandRequest {
                mutation: Some(Mutation {
                    actor: Some(Actor::agent("sess_1", "claude")),
                    dry_run: true,
                    ..Default::default()
                }),
                target_id: target_id.clone(),
                command: "uptime".to_string(),
                ..Default::default()
            }))
            .await;

        let audit = service
            .context
            .store
            .list_audit(&target_id, actor::Kind::Agent, 50, 0)
            .await
            .expect("audit");
        let entry = audit
            .iter()
            .find(|e| e.action == "Logs.RunCommand")
            .expect("entry");
        assert!(entry.summary.contains("uptime"));
    }
}
