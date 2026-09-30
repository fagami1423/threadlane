//! WebSocket transport for [`DaemonCore`]: the standalone binary serves
//! this, and tests drive the real protocol roundtrip through it.
//!
//! Framing: one bare `SessionCommand` JSON per inbound text frame;
//! `SessionEvent` JSON frames outbound — the journal tail on attach, then
//! live broadcast. Errors travel as `DaemonError` events, so no error
//! frame shape exists.

use std::net::SocketAddr;
use std::sync::Arc;

use futures::{SinkExt, StreamExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::handshake::server::{Request, Response};
use tokio_tungstenite::tungstenite::Message;

use threadlane_protocol::daemon::{SessionCommand, SessionEvent};

use crate::core::DaemonCore;

/// Accept loop: every connection gets a [`serve_connection`] task.
pub async fn serve(listener: TcpListener, core: Arc<DaemonCore>, token: Option<String>) {
    loop {
        match listener.accept().await {
            Ok((stream, peer)) => {
                let core = core.clone();
                let token = token.clone();
                tokio::spawn(async move {
                    if let Err(error) = serve_connection(core, stream, peer, token).await {
                        tracing::debug!(%peer, %error, "daemon connection ended");
                    }
                });
            }
            Err(error) => tracing::warn!(%error, "daemon accept failed"),
        }
    }
}

/// One client: journal tail, live broadcast, command loop.
async fn serve_connection(
    core: Arc<DaemonCore>,
    stream: TcpStream,
    peer: SocketAddr,
    token: Option<String>,
) -> Result<(), String> {
    let socket = tokio_tungstenite::accept_hdr_async(
        stream,
        |request: &Request, response: Response| {
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
            Ok(response)
        },
    )
    .await
    .map_err(|error| format!("websocket handshake failed: {error}"))?;
    tracing::info!(%peer, "daemon client attached");

    // All outbound traffic funnels through one channel: the journal tail is
    // seeded first, then a forwarder streams live broadcast events.
    let (out_tx, mut out_rx) = mpsc::unbounded_channel::<Message>();
    let error_tx = out_tx.clone();
    let (tail, mut broadcast_rx) = core.subscribe_with_tail();
    for event in tail {
        let text = serde_json::to_string(&event).map_err(|error| error.to_string())?;
        let _ = out_tx.send(Message::Text(text.into()));
    }
    tokio::spawn(async move {
        loop {
            let event = match broadcast_rx.recv().await {
                Ok(event) => event,
                Err(tokio::sync::broadcast::error::RecvError::Lagged(skipped)) => {
                    SessionEvent::DaemonError {
                        session_id: None,
                        message: format!("dropped {skipped} daemon events; refresh the session"),
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
            };
            let Ok(text) = serde_json::to_string(&event) else {
                continue;
            };
            if out_tx.send(Message::Text(text.into())).is_err() {
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

    while let Some(message) = read.next().await {
        match message {
            Ok(Message::Text(text)) => {
                match serde_json::from_str::<SessionCommand>(&text) {
                    Ok(command) => {
                        // Dispatch errors also reach this client as
                        // DaemonError events — no error frame shape needed.
                        if let Err(error) = core.clone().dispatch(command).await {
                            tracing::warn!(%peer, %error, "daemon command rejected");
                        }
                    }
                    Err(error) => {
                        let _ = error_tx.send(Message::Text(
                            serde_json::to_string(&SessionEvent::DaemonError {
                                session_id: None,
                                message: format!("undecodable command: {error}"),
                            })
                            .unwrap_or_default()
                            .into(),
                        ));
                    }
                }
            }
            Ok(Message::Close(_)) | Err(_) => break,
            // tungstenite answers Ping frames itself when we don't split the
            // stream; any that surface here are safe to ignore.
            Ok(_) => {}
        }
    }
    Ok(())
}
