//! The terminal websocket.
//!
//! The endpoint accepts a session id in the query string and a single-use token
//! in the first frame — never a host, a port, a key or a command line. The
//! server redeems the token and looks the target up itself, so the worst a
//! compromised browser session can do is open a shell on a target it was already
//! granted one for.
//!
//! Coolify's equivalent has the browser send a full `ssh` command line and
//! validates only the host, leaving `-o ProxyCommand=` under client control.
//! That design is deliberately not reproduced.

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Query, State};
use axum::response::Response;
use russh::ChannelMsg;
use serde::{Deserialize, Serialize};

use crate::AppState;

/// The query string of the websocket URL.
#[derive(Debug, Deserialize)]
pub struct TerminalQuery {
    /// The grant's id. Not a secret — the token is what authenticates.
    pub session: String,
}

/// Frames the browser sends.
#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
enum ClientFrame {
    /// The first frame: spends the single-use token.
    Attach {
        token: String,
    },
    Stdin {
        data: String,
    },
    Resize {
        cols: u32,
        rows: u32,
    },
    /// Backpressure: the browser is behind on writes.
    Pause,
    Resume,
}

/// Frames the server sends.
///
/// Everything is tagged JSON rather than raw bytes with sentinel strings, so
/// program output that happens to equal a control word cannot be misread as one.
#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "lowercase")]
enum ServerFrame {
    Output { data: String },
    Exit { code: i32 },
    Error { message: String },
}

/// Upgrades the connection.
///
/// Nothing is authenticated here beyond the dashboard session: the grant is
/// redeemed inside the socket, because the token arrives in the first frame
/// rather than in the URL.
pub async fn websocket(
    State(state): State<AppState>,
    Query(query): Query<TerminalQuery>,
    upgrade: WebSocketUpgrade,
    // Requiring a logged-in user means an unauthenticated caller cannot even
    // reach the token check.
    _user: crate::auth::CurrentUser,
) -> Response {
    upgrade.on_upgrade(move |socket| handle(socket, state, query.session))
}

/// Runs one terminal session.
async fn handle(mut socket: WebSocket, state: AppState, session_id: String) {
    // ---- redeem the grant ----
    let token = match first_attach(&mut socket).await {
        Some(token) => token,
        None => {
            send(
                &mut socket,
                ServerFrame::Error {
                    message: "the first message must attach with a session token".to_string(),
                },
            )
            .await;
            return;
        }
    };

    let grant = match state
        .store
        .consume_terminal_session(&session_id, &token)
        .await
    {
        Ok(Some(grant)) => grant,
        Ok(None) => {
            // Single-use: a replayed or expired token finds nothing.
            send(
                &mut socket,
                ServerFrame::Error {
                    message: "that terminal session is unknown, already used, or expired"
                        .to_string(),
                },
            )
            .await;
            return;
        }
        Err(error) => {
            tracing::error!(%error, "redeeming the terminal grant failed");
            send(
                &mut socket,
                ServerFrame::Error {
                    message: "could not open the session".to_string(),
                },
            )
            .await;
            return;
        }
    };

    // ---- connect, using our own record of the target ----
    let target = match state.store.get_target(&grant.target_id).await {
        Ok(Some(target)) => target,
        _ => {
            send(
                &mut socket,
                ServerFrame::Error {
                    message: "that target no longer exists".to_string(),
                },
            )
            .await;
            return;
        }
    };

    // Through the engine, so the host key is verified and a first-use pinning
    // or a refused change is recorded here as it is everywhere else. A changed
    // key ends the session before a PTY is opened: a terminal is exactly the
    // thing you least want attached to a host that is not the one you pinned.
    let session = match state.engine.connect(&target).await {
        Ok(session) => session,
        Err(error) => {
            send(
                &mut socket,
                ServerFrame::Error {
                    message: format!("could not reach {}: {error:#}", target.host),
                },
            )
            .await;
            return;
        }
    };

    let mut channel = match session
        .open_pty(grant.cols, grant.rows, Some(&grant.initial_command))
        .await
    {
        Ok(channel) => channel,
        Err(error) => {
            send(
                &mut socket,
                ServerFrame::Error {
                    message: format!("could not open a PTY: {error:#}"),
                },
            )
            .await;
            return;
        }
    };

    tracing::info!(target = %target.name, session = %grant.id, "terminal session opened");

    // ---- pump ----
    // `paused` implements the browser's backpressure request: while set, output
    // from the target is held rather than queued into a socket the browser is
    // not draining.
    let mut paused = false;
    let mut held: Vec<u8> = Vec::new();

    loop {
        tokio::select! {
            // Browser -> target.
            incoming = socket.recv() => {
                let Some(Ok(message)) = incoming else { break };

                match message {
                    Message::Text(text) => {
                        match serde_json::from_str::<ClientFrame>(&text) {
                            Ok(ClientFrame::Stdin { data }) => {
                                if channel.data(data.as_bytes()).await.is_err() {
                                    break;
                                }
                            }
                            Ok(ClientFrame::Resize { cols, rows }) => {
                                // A window-change request, so full-screen
                                // programs redraw at the new size.
                                let _ = channel
                                    .window_change(cols.clamp(1, 1000), rows.clamp(1, 1000), 0, 0)
                                    .await;
                            }
                            Ok(ClientFrame::Pause) => paused = true,
                            Ok(ClientFrame::Resume) => {
                                paused = false;
                                if !held.is_empty() {
                                    let data = String::from_utf8_lossy(&held).into_owned();
                                    held.clear();
                                    send(&mut socket, ServerFrame::Output { data }).await;
                                }
                            }
                            // A second attach mid-session, or an unknown frame:
                            // ignored rather than treated as input, so a bug in
                            // the client cannot type into the shell.
                            Ok(ClientFrame::Attach { .. }) | Err(_) => {}
                        }
                    }
                    Message::Close(_) => break,
                    // The client protocol is text-only; binary and ping/pong
                    // frames are not input.
                    _ => {}
                }
            }

            // Target -> browser.
            event = channel.wait() => {
                match event {
                    Some(ChannelMsg::Data { data }) | Some(ChannelMsg::ExtendedData { data, .. }) => {
                        if paused {
                            held.extend_from_slice(&data);
                            // Bound what a paused browser can make us buffer.
                            const MAX_HELD: usize = 1024 * 1024;
                            if held.len() > MAX_HELD {
                                let overflow = held.len() - MAX_HELD;
                                held.drain(..overflow);
                            }
                            continue;
                        }
                        let text = String::from_utf8_lossy(&data).into_owned();
                        send(&mut socket, ServerFrame::Output { data: text }).await;
                    }
                    Some(ChannelMsg::ExitStatus { exit_status }) => {
                        send(&mut socket, ServerFrame::Exit {
                            code: exit_status as i32,
                        })
                        .await;
                        break;
                    }
                    Some(ChannelMsg::Eof) | Some(ChannelMsg::Close) | None => break,
                    _ => {}
                }
            }
        }
    }

    // Dropping the session closes the SSH connection, which is what stops a
    // disconnected browser from leaving a shell running on the target.
    let _ = channel.eof().await;
    let _ = session.close().await;
    let _ = futures_util::SinkExt::close(&mut socket).await;

    tracing::info!(target = %target.name, session = %grant.id, "terminal session closed");
}

/// Reads frames until the attach arrives, returning its token.
///
/// Anything before the attach is discarded rather than acted on: input must not
/// reach a PTY that does not exist yet.
async fn first_attach(socket: &mut WebSocket) -> Option<String> {
    // Bounded so a client that never attaches cannot hold the connection.
    for _ in 0..5 {
        match socket.recv().await {
            Some(Ok(Message::Text(text))) => {
                if let Ok(ClientFrame::Attach { token }) =
                    serde_json::from_str::<ClientFrame>(&text)
                {
                    return Some(token);
                }
            }
            Some(Ok(Message::Close(_))) | None => return None,
            Some(Err(_)) => return None,
            _ => {}
        }
    }
    None
}

/// Sends a frame, ignoring a closed socket.
async fn send(socket: &mut WebSocket, frame: ServerFrame) {
    if let Ok(json) = serde_json::to_string(&frame) {
        let _ = socket.send(Message::Text(json.into())).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_attach_frame_parses_and_carries_only_a_token() {
        let frame: ClientFrame =
            serde_json::from_str(r#"{"type":"attach","token":"abc"}"#).expect("parse");
        match frame {
            ClientFrame::Attach { token } => assert_eq!(token, "abc"),
            other => panic!("expected attach, got {other:?}"),
        }
    }

    #[test]
    fn the_client_protocol_has_no_way_to_name_a_host_or_a_command() {
        // This is the property that makes the design safe: there is no field a
        // client could populate to redirect the connection or inject ssh flags.
        for hostile in [
            r#"{"type":"attach","token":"t","host":"evil.example.com"}"#,
            r#"{"type":"attach","token":"t","command":"ssh -o ProxyCommand=sh root@x"}"#,
        ] {
            let frame: ClientFrame = serde_json::from_str(hostile).expect("parse");
            // The extra fields are simply dropped.
            assert!(matches!(frame, ClientFrame::Attach { .. }));
        }
    }

    #[test]
    fn every_client_frame_kind_parses() {
        let parse = |json: &str| serde_json::from_str::<ClientFrame>(json).expect("parse");

        assert!(matches!(
            parse(r#"{"type":"stdin","data":"ls\n"}"#),
            ClientFrame::Stdin { .. }
        ));
        assert!(matches!(
            parse(r#"{"type":"resize","cols":120,"rows":40}"#),
            ClientFrame::Resize {
                cols: 120,
                rows: 40
            }
        ));
        assert!(matches!(parse(r#"{"type":"pause"}"#), ClientFrame::Pause));
        assert!(matches!(parse(r#"{"type":"resume"}"#), ClientFrame::Resume));
    }

    #[test]
    fn an_unknown_or_malformed_frame_is_a_parse_error_rather_than_input() {
        // Falling back to treating it as stdin would let a client bug type into
        // the shell.
        assert!(
            serde_json::from_str::<ClientFrame>(r#"{"type":"exec","cmd":"rm -rf /"}"#).is_err()
        );
        assert!(serde_json::from_str::<ClientFrame>("not json").is_err());
        assert!(serde_json::from_str::<ClientFrame>(r#"{"data":"no type"}"#).is_err());
    }

    #[test]
    fn server_frames_serialize_with_an_explicit_type() {
        // Tagged JSON, so output equal to a control word cannot be misread as
        // one — which is a real hazard in a raw-bytes protocol.
        let output = serde_json::to_string(&ServerFrame::Output {
            data: "pty-ready".to_string(),
        })
        .expect("serialize");
        assert_eq!(output, r#"{"type":"output","data":"pty-ready"}"#);

        let exit = serde_json::to_string(&ServerFrame::Exit { code: 130 }).expect("serialize");
        assert_eq!(exit, r#"{"type":"exit","code":130}"#);

        let error = serde_json::to_string(&ServerFrame::Error {
            message: "nope".to_string(),
        })
        .expect("serialize");
        assert_eq!(error, r#"{"type":"error","message":"nope"}"#);
    }

    #[test]
    fn output_containing_quotes_and_newlines_survives_the_json_round_trip() {
        // Terminal output is arbitrary bytes; it must not be able to break the
        // frame it travels in.
        let hostile = "he said \"hi\"\n\x1b[31mred\x1b[0m\\";
        let json = serde_json::to_string(&ServerFrame::Output {
            data: hostile.to_string(),
        })
        .expect("serialize");

        let parsed: serde_json::Value = serde_json::from_str(&json).expect("parse");
        assert_eq!(parsed["data"], hostile);
    }

    #[test]
    fn the_query_string_carries_only_the_session_id() {
        let query: TerminalQuery =
            serde_json::from_str(r#"{"session":"term_abc"}"#).expect("parse");
        assert_eq!(query.session, "term_abc");
    }
}
