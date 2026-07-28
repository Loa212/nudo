//! The `Terminals` service: session grants and the bidi attach stream.
//!
//! A client never names a host. `CreateSession` records which target the grant
//! is for and returns an opaque single-use token; `Attach` (and the web tier's
//! websocket) redeems the token and the server looks the host up itself.

use std::pin::Pin;

use futures_util::{Stream, StreamExt};
use nudo_proto::terminals_server::Terminals;
use nudo_proto::*;
use tonic::{Request, Response, Status};

use super::Context;

pub struct TerminalsService {
    context: Context,
}

impl TerminalsService {
    pub fn new(context: Context) -> Self {
        Self { context }
    }
}

#[tonic::async_trait]
impl Terminals for TerminalsService {
    async fn create_session(
        &self,
        request: Request<CreateTerminalSessionRequest>,
    ) -> Result<Response<TerminalSession>, Status> {
        let request = request.into_inner();
        let target = self.context.require_target(&request.target_id).await?;

        // A PTY on a latency-critical box is a mutation by any reasonable
        // reading: whoever holds it can do anything the SSH user can.
        self.context
            .authorize(
                request.mutation.as_ref(),
                "Terminals.CreateSession",
                &request.target_id,
                Some(&target),
                format!("opened a terminal session on {}", target.name),
            )
            .await?;

        let (id, token, expires_at) = self
            .context
            .store
            .create_terminal_session(
                &request.target_id,
                &request.initial_command,
                request.cols,
                request.rows,
            )
            .await
            .map_err(super::invalid)?;

        // The URL carries the session id, and the token goes in the first
        // websocket message rather than the query string, so it does not end up
        // in browser history or an access log.
        let websocket_url = format!(
            "{}/terminal/ws?session={}",
            self.context.config.base_url.trim_end_matches('/'),
            urlencoding::encode(&id)
        );

        Ok(Response::new(TerminalSession {
            id,
            token,
            websocket_url,
            expires_at: Some(nudo_proto::to_timestamp(expires_at)),
        }))
    }

    type AttachStream =
        Pin<Box<dyn Stream<Item = Result<TerminalServerMessage, Status>> + Send + 'static>>;

    async fn attach(
        &self,
        request: Request<tonic::Streaming<TerminalClientMessage>>,
    ) -> Result<Response<Self::AttachStream>, Status> {
        let mut inbound = request.into_inner();
        let context = self.context.clone();

        // The first message must be the attach, carrying the session id and
        // token. Nothing else is accepted before authentication.
        let first = inbound
            .next()
            .await
            .ok_or_else(|| Status::invalid_argument("the stream closed before attaching"))?
            .map_err(|error| Status::invalid_argument(error.to_string()))?;

        let attach = match first.message {
            Some(terminal_client_message::Message::Attach(session)) => session,
            _ => {
                return Err(Status::invalid_argument(
                    "the first message must be an attach carrying a session id and token",
                ));
            }
        };

        // Single-use: redeeming marks the grant consumed, so a captured token
        // cannot be replayed.
        let grant = context
            .store
            .consume_terminal_session(&attach.id, &attach.token)
            .await
            .map_err(super::internal)?
            .ok_or_else(|| {
                Status::unauthenticated(
                    "that terminal session token is unknown, already used, or expired",
                )
            })?;

        // The host comes from our own state, never from the client.
        let target = context.require_target(&grant.target_id).await?;
        let session = context.connect(&target).await?;

        let channel = session
            .open_pty(grant.cols, grant.rows, Some(&grant.initial_command))
            .await
            .map_err(|error| Status::internal(format!("opening a PTY: {error:#}")))?;

        let stream = pty_stream(channel, inbound, session);
        Ok(Response::new(Box::pin(stream)))
    }
}

/// Pumps bytes between the client stream and the PTY channel.
///
/// The SSH session is moved in and dropped when the stream ends, which closes
/// the connection — that is what stops a disconnected client from leaving a
/// shell running on the target.
fn pty_stream(
    channel: russh::Channel<russh::client::Msg>,
    mut inbound: tonic::Streaming<TerminalClientMessage>,
    session: crate::ssh::SshSession,
) -> impl Stream<Item = Result<TerminalServerMessage, Status>> {
    async_stream::stream! {
        let mut channel = channel;
        // Held only to tie the connection's lifetime to this stream.
        let _session = session;

        loop {
            tokio::select! {
                // Client -> target.
                message = inbound.next() => {
                    match message {
                        Some(Ok(message)) => match message.message {
                            Some(terminal_client_message::Message::Stdin(bytes)) => {
                                if channel.data(&bytes[..]).await.is_err() {
                                    break;
                                }
                            }
                            Some(terminal_client_message::Message::Resize(resize)) => {
                                // A window-change request, so full-screen
                                // programs redraw at the new size.
                                let _ = channel
                                    .window_change(
                                        resize.cols.clamp(1, 1000),
                                        resize.rows.clamp(1, 1000),
                                        0,
                                        0,
                                    )
                                    .await;
                            }
                            Some(terminal_client_message::Message::Close(_)) => {
                                let _ = channel.eof().await;
                                break;
                            }
                            // A second attach mid-stream is a client bug; a
                            // re-authentication must open a new stream.
                            Some(terminal_client_message::Message::Attach(_)) | None => {}
                        },
                        // Client hung up or errored: tear the PTY down.
                        _ => {
                            let _ = channel.eof().await;
                            break;
                        }
                    }
                }

                // Target -> client.
                event = channel.wait() => {
                    match event {
                        Some(russh::ChannelMsg::Data { data }) => {
                            yield Ok(TerminalServerMessage {
                                message: Some(terminal_server_message::Message::Stdout(
                                    data.to_vec(),
                                )),
                            });
                        }
                        Some(russh::ChannelMsg::ExtendedData { data, .. }) => {
                            // A PTY merges the streams anyway; forwarding
                            // stderr as stdout is what a terminal shows.
                            yield Ok(TerminalServerMessage {
                                message: Some(terminal_server_message::Message::Stdout(
                                    data.to_vec(),
                                )),
                            });
                        }
                        Some(russh::ChannelMsg::ExitStatus { exit_status }) => {
                            yield Ok(TerminalServerMessage {
                                message: Some(terminal_server_message::Message::ExitCode(
                                    exit_status as i32,
                                )),
                            });
                            break;
                        }
                        Some(russh::ChannelMsg::Eof) | Some(russh::ChannelMsg::Close) | None => {
                            break;
                        }
                        _ => {}
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::test_support;

    async fn fixture() -> (TerminalsService, String) {
        let (context, target) = test_support::context_with_target().await;
        (TerminalsService::new(context), target.id)
    }

    fn create(target_id: &str) -> CreateTerminalSessionRequest {
        CreateTerminalSessionRequest {
            mutation: Some(Mutation::by(Actor::human("usr_1", "alice"))),
            target_id: target_id.to_string(),
            initial_command: String::new(),
            cols: 120,
            rows: 40,
        }
    }

    #[tokio::test]
    async fn a_session_grant_carries_a_token_and_an_expiry_but_no_host() {
        let (service, target_id) = fixture().await;
        let session = service
            .create_session(Request::new(create(&target_id)))
            .await
            .expect("create")
            .into_inner();

        assert!(session.id.starts_with("term_"));
        assert!(!session.token.is_empty());
        assert!(session.expires_at.is_some());

        // The client is never told which host it will reach, and the token is
        // not in the URL.
        assert!(!session.websocket_url.contains("10.0.0.1"));
        assert!(!session.websocket_url.contains(&session.token));
        assert!(session.websocket_url.contains(&session.id));
    }

    #[tokio::test]
    async fn a_grant_is_short_lived() {
        let (service, target_id) = fixture().await;
        let session = service
            .create_session(Request::new(create(&target_id)))
            .await
            .expect("create")
            .into_inner();

        let expires_at =
            nudo_proto::from_timestamp(&session.expires_at.expect("expiry")).expect("in range");
        let lifetime = expires_at - chrono::Utc::now();
        // Long enough for a browser to connect, short enough that a leaked
        // token is not useful.
        assert!(
            lifetime <= chrono::Duration::minutes(5),
            "grant lives too long"
        );
        assert!(lifetime > chrono::Duration::zero());
    }

    #[tokio::test]
    async fn a_terminal_on_a_latency_critical_target_is_refused_without_the_opt_in() {
        // Whoever holds a PTY can do anything the SSH user can, so this counts
        // as a mutation.
        let (service, _) = fixture().await;
        let hot = test_support::create_latency_critical_target(&service.context).await;

        let status = service
            .create_session(Request::new(create(&hot.id)))
            .await
            .expect_err("must be refused");
        assert_eq!(status.code(), tonic::Code::FailedPrecondition);
    }

    #[tokio::test]
    async fn a_session_for_a_missing_target_is_not_found() {
        let (service, _) = fixture().await;
        let status = service
            .create_session(Request::new(create("tgt_nope")))
            .await
            .expect_err("err");
        assert_eq!(status.code(), tonic::Code::NotFound);
    }

    #[tokio::test]
    async fn opening_a_terminal_is_recorded_in_the_audit_log() {
        let (service, target_id) = fixture().await;
        service
            .create_session(Request::new(create(&target_id)))
            .await
            .expect("create");

        let audit = service
            .context
            .store
            .list_audit(&target_id, actor::Kind::Unspecified, 50, 0)
            .await
            .expect("audit");
        assert!(audit.iter().any(|e| e.action == "Terminals.CreateSession"));
    }

    #[tokio::test]
    async fn a_grant_is_single_use() {
        let (service, target_id) = fixture().await;
        let session = service
            .create_session(Request::new(create(&target_id)))
            .await
            .expect("create")
            .into_inner();

        // The first redemption succeeds at the store level...
        assert!(
            service
                .context
                .store
                .consume_terminal_session(&session.id, &session.token)
                .await
                .expect("consume")
                .is_some()
        );
        // ...and a replay finds nothing, which is what Attach relies on.
        assert!(
            service
                .context
                .store
                .consume_terminal_session(&session.id, &session.token)
                .await
                .expect("consume")
                .is_none()
        );
    }

    #[tokio::test]
    async fn a_forged_token_cannot_redeem_a_grant() {
        let (service, target_id) = fixture().await;
        let session = service
            .create_session(Request::new(create(&target_id)))
            .await
            .expect("create")
            .into_inner();

        assert!(
            service
                .context
                .store
                .consume_terminal_session(&session.id, "guessed-token")
                .await
                .expect("consume")
                .is_none()
        );
    }

    #[tokio::test]
    async fn the_requested_dimensions_are_carried_into_the_grant() {
        let (service, target_id) = fixture().await;
        let session = service
            .create_session(Request::new(create(&target_id)))
            .await
            .expect("create")
            .into_inner();

        let grant = service
            .context
            .store
            .consume_terminal_session(&session.id, &session.token)
            .await
            .expect("consume")
            .expect("some");
        assert_eq!((grant.cols, grant.rows), (120, 40));
        assert_eq!(grant.target_id, target_id);
    }
}
