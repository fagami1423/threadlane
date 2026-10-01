//! WebSocket transport for [`DaemonCore`]: the standalone binary serves
//! this, and tests drive the real protocol roundtrip through it.
//!
//! Framing: one bare `SessionCommand` JSON per inbound text frame, or a
//! `{"request_id": N, "command": ...}` `CommandRequest` envelope when the
//! client wants the dispatch result back. Outbound frames are
//! `{"seq": N, "event": SessionEvent}` — the journal tail newer than the
//! client's `?since=` cursor on attach, then live broadcast — plus
//! `{"response": CommandReply}` replies sent only to the requesting
//! connection and never journaled. Errors travel as `DaemonError` events
//! (and as the reply's `Err`), so no error frame shape exists.
//! `seq` is the daemon's journal sequence; synthesized frames (undecodable
//! commands, lag notices) carry `seq: 0`.
//!
//! The handshake response carries `x-threadlane-protocol`
//! ([`PROTOCOL_VERSION_HEADER`]) so a client learns which wire features
//! this daemon speaks before it risks a frame an older daemon would
//! reject — the `CommandRequest` envelope is version-2 behavior.
//!
//! Auth today is a shared bearer token (`THREADLANE_DAEMON_TOKEN`) and the
//! deployment is localhost-only by design. Binding beyond localhost needs a
//! follow-up pairing flow (QR scan or entered pairing code that exchanges
//! for a per-client credential) — a shared static token is not a safe
//! network-exposed auth scheme.

use std::future::Future;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use futures::{SinkExt, StreamExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::handshake::server::{Request, Response};
use tokio_tungstenite::tungstenite::Message;

use threadlane_protocol::daemon::{
    CommandReply, CommandRequest, SessionCommand, SessionEvent, PROTOCOL_VERSION_HEADER,
    WIRE_PROTOCOL_VERSION,
};

use crate::core::DaemonCore;

/// Accept loop: every connection gets a [`serve_connection`] task.
pub async fn serve(listener: TcpListener, core: Arc<DaemonCore>, token: Option<String>) {
    serve_until(listener, core, token, std::future::pending()).await;
}

/// [`serve`] plus a shutdown: when `shutdown` resolves the listener stops
/// accepting and every live client connection is aborted. Used by the LAN
/// pairing flow, where "stop sharing" must actually disconnect attached
/// clients rather than just closing the front door.
pub async fn serve_until(
    listener: TcpListener,
    core: Arc<DaemonCore>,
    token: Option<String>,
    shutdown: impl Future<Output = ()> + Send,
) {
    tokio::pin!(shutdown);
    let mut connections = tokio::task::JoinSet::new();
    loop {
        tokio::select! {
            _ = &mut shutdown => break,
            accept = listener.accept() => match accept {
                Ok((stream, peer)) => {
                    let core = core.clone();
                    let token = token.clone();
                    connections.spawn(async move {
                        if let Err(error) = serve_connection(core, stream, peer, token).await {
                            tracing::debug!(%peer, %error, "daemon connection ended");
                        }
                    });
                }
                Err(error) => tracing::warn!(%error, "daemon accept failed"),
            },
        }
    }
    connections.abort_all();
    while connections.join_next().await.is_some() {}
}

/// One outbound frame: the journal sequence (0 for frames the daemon
/// synthesizes outside the journal, like lag notices) plus the event.
fn wire_frame(seq: u64, event: &SessionEvent) -> Result<Message, serde_json::Error> {
    serde_json::to_string(&serde_json::json!({ "seq": seq, "event": event }))
        .map(|text| Message::Text(text.into()))
}

/// The point-to-point reply to a `CommandRequest`: rides the connection's
/// outbound channel but never the journal, so reconnects don't replay it.
fn reply_frame(reply: &CommandReply) -> Result<Message, serde_json::Error> {
    serde_json::to_string(&serde_json::json!({ "response": reply }))
        .map(|text| Message::Text(text.into()))
}

/// Track the terminals a connection opens and closes so disconnect
/// cleanup can kill whichever it leaves behind — including ones wrapped
/// in a `CommandRequest` envelope.
fn note_terminal(command: &SessionCommand, owned: &mut std::collections::HashSet<String>) {
    match command {
        SessionCommand::TerminalOpen { terminal_id, .. } => {
            owned.insert(terminal_id.clone());
        }
        SessionCommand::TerminalClose { terminal_id } => {
            owned.remove(terminal_id);
        }
        _ => {}
    }
}

/// [`WIRE_PROTOCOL_VERSION`] as a header value — `from_static` needs a
/// literal, so keep this in step with the constant. The const assert
/// below fails the build when one moves without the other.
const WIRE_PROTOCOL_VERSION_STR: &str = "3";

const _: () = assert!(
    WIRE_PROTOCOL_VERSION == 3,
    "WIRE_PROTOCOL_VERSION_STR must match WIRE_PROTOCOL_VERSION"
);

/// The client's last-seen journal sequence from `?since=` on the connect
/// URL; absent or unparsable means a full tail replay.
fn since_param(request: &Request) -> u64 {
    request
        .uri()
        .query()
        .and_then(|query| {
            query
                .split('&')
                .find_map(|pair| pair.strip_prefix("since="))
        })
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(0)
}

/// One client: journal tail, live broadcast, command loop.
async fn serve_connection(
    core: Arc<DaemonCore>,
    stream: TcpStream,
    peer: SocketAddr,
    token: Option<String>,
) -> Result<(), String> {
    let since: Arc<AtomicU64> = Arc::new(AtomicU64::new(0));
    let handshake_since = since.clone();
    let socket = tokio_tungstenite::accept_hdr_async(
        stream,
        move |request: &Request, response: Response| {
            // Browsers always send Origin; native clients do not. Refuse
            // browser pages outright: a website could otherwise open
            // ws://127.0.0.1 and drive the daemon (submit prompts, answer
            // its own permission requests) — cross-site WebSocket
            // hijacking, which CORS does not cover.
            if request.headers().contains_key("Origin") {
                return Err(tokio_tungstenite::tungstenite::http::Response::builder()
                    .status(403)
                    .body(Some("forbidden origin".to_string()))
                    .expect("static 403 response"));
            }
            if let Some(token) = &token {
                let presented = request
                    .headers()
                    .get("Authorization")
                    .and_then(|value| value.to_str().ok());
                if presented != Some(format!("Bearer {token}").as_str()) {
                    return Err(tokio_tungstenite::tungstenite::http::Response::builder()
                        .status(401)
                        .body(Some("unauthorized".to_string()))
                        .expect("static 401 response"));
                }
            }
            handshake_since.store(since_param(request), Ordering::SeqCst);
            // Announce the wire protocol version so the client can gate
            // versioned features (today: the CommandRequest envelope) on
            // what this daemon actually understands.
            let mut response = response;
            response.headers_mut().insert(
                PROTOCOL_VERSION_HEADER,
                tokio_tungstenite::tungstenite::http::HeaderValue::from_static(
                    WIRE_PROTOCOL_VERSION_STR,
                ),
            );
            Ok(response)
        },
    )
    .await
    .map_err(|error| format!("websocket handshake failed: {error}"))?;
    tracing::info!(%peer, "daemon client attached");

    // All outbound traffic funnels through one bounded channel: the
    // journal tail is seeded first, then a forwarder streams live
    // broadcast events. Bounded on purpose — a slow socket backs the
    // forwarder up into the broadcast receiver, where it surfaces as
    // `Lagged` (client gets a dropped-events notice) instead of growing
    // an unbounded queue per stalled client.
    let (out_tx, mut out_rx) = mpsc::channel::<Message>(256);
    let error_tx = out_tx.clone();
    // Point-to-point command replies share the connection's ordered
    // outbound channel; `out_tx` itself moves into the broadcast
    // forwarder below.
    let reply_tx = out_tx.clone();
    let (tail, mut broadcast_rx) =
        core.subscribe_with_tail(since.load(Ordering::SeqCst));
    for (seq, event) in tail {
        let frame = wire_frame(seq, &event).map_err(|error| error.to_string())?;
        if out_tx.send(frame).await.is_err() {
            return Ok(());
        }
    }
    tokio::spawn(async move {
        loop {
            let (seq, event) = match broadcast_rx.recv().await {
                Ok(pair) => pair,
                Err(tokio::sync::broadcast::error::RecvError::Lagged(skipped)) => {
                    (0, SessionEvent::DaemonError {
                        session_id: None,
                        message: format!("dropped {skipped} daemon events; refresh the session"),
                    })
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
            };
            let Ok(frame) = wire_frame(seq, &event) else {
                continue;
            };
            if out_tx.send(frame).await.is_err() {
                return;
            }
        }
    });

    let (mut write, mut read) = socket.split();
    tokio::spawn(async move {
        while let Some(message) = out_rx.recv().await {
            if write.send(message).await.is_err() {
                return;
            }
        }
    });

    // Terminals this connection opened but never closed. A crashed or
    // abruptly gone client can't deliver TerminalClose, so the connection
    // owns their cleanup — otherwise every leaked interactive shell and
    // its worker threads would outlive the client indefinitely.
    let mut owned_terminals = std::collections::HashSet::<String>::new();
    while let Some(message) = read.next().await {
        match message {
            Ok(Message::Text(text)) => {
                // A `CommandRequest` envelope asks for the dispatch result
                // back on this connection; bare commands stay
                // fire-and-forget. The shapes are disjoint — a request has
                // no top-level `type` tag.
                if let Ok(CommandRequest { request_id, command }) =
                    serde_json::from_str::<CommandRequest>(&text)
                {
                    note_terminal(&command, &mut owned_terminals);
                    let reply = CommandReply {
                        request_id,
                        result: core
                            .clone()
                            .dispatch_with_request_id(command, Some(request_id))
                            .await,
                    };
                    match reply_frame(&reply) {
                        Ok(frame) => {
                            if reply_tx.send(frame).await.is_err() {
                                break;
                            }
                        }
                        Err(error) => {
                            tracing::warn!(%peer, %error, "could not encode command reply");
                        }
                    }
                    continue;
                }
                match serde_json::from_str::<SessionCommand>(&text) {
                    Ok(command) => {
                        note_terminal(&command, &mut owned_terminals);
                        // Dispatch errors also reach this client as
                        // DaemonError events — no error frame shape needed.
                        if let Err(error) = core.clone().dispatch(command).await {
                            tracing::warn!(%peer, %error, "daemon command rejected");
                        }
                    }
                    Err(error) => {
                        // A jammed client misses the notice rather than
                        // stalling the command loop on its backlog.
                        if let Ok(frame) = wire_frame(0, &SessionEvent::DaemonError {
                            session_id: None,
                            message: format!("undecodable command: {error}"),
                        }) {
                            let _ = error_tx.try_send(frame);
                        }
                    }
                }
            }
            Ok(Message::Close(_)) | Err(_) => break,
            // tungstenite answers Ping frames itself when we don't split the
            // stream; any that surface here are safe to ignore.
            Ok(_) => {}
        }
    }
    for terminal_id in owned_terminals {
        core.close_terminal(&terminal_id);
    }
    Ok(())
}
