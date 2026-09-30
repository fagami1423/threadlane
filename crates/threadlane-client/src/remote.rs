//! WebSocket transport: [`RemoteDaemon`] dials a running `threadlane-daemon`
//! process and speaks the `threadlane-protocol::daemon` session contract —
//! bare `SessionCommand` JSON outbound, `SessionEvent` JSON inbound.
//!
//! Reconnect semantics: the driver reconnects with backoff whenever the
//! socket drops. The server replays its bounded journal tail on every
//! accept, so events queued while the client was away are recovered in
//! order; events already delivered before the drop can repeat at the tail
//! boundary, which consumers must tolerate (every snapshot-style event is
//! idempotent, and seq-bearing entries dedupe by sequence).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use futures::{SinkExt, StreamExt};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::Message;

use threadlane_protocol::daemon::{SessionCommand, SessionEvent};

use crate::DaemonClient;

/// Delay between reconnect attempts after the socket drops.
const RECONNECT_DELAY: Duration = Duration::from_millis(750);

/// A [`DaemonClient`] that attaches to a remote `threadlane-daemon` over
/// WebSocket.
pub struct RemoteDaemon {
    /// Commands awaiting/in-flight to the server; the driver drains it.
    command_tx: mpsc::UnboundedSender<SessionCommand>,
    /// Every `subscribe()` caller's channel; the reader task fans events out.
    subscribers: Arc<Mutex<Vec<mpsc::UnboundedSender<SessionEvent>>>>,
    /// Whether a socket is live right now — commands sent while down fail
    /// fast instead of firing stale after a reconnect.
    connected: Arc<AtomicBool>,
}

impl RemoteDaemon {
    /// Construct the client and spawn its connection driver on the shared
    /// Threadlane reactor. The returned client is usable immediately —
    /// commands issued before the first successful dial are reported as
    /// errors through the event stream.
    pub fn connect(url: impl Into<String>, token: Option<String>) -> Arc<Self> {
        let url = url.into();
        let (command_tx, command_rx) = mpsc::unbounded_channel::<SessionCommand>();
        let client = Arc::new(Self {
            command_tx,
            subscribers: Arc::new(Mutex::new(Vec::new())),
            connected: Arc::new(AtomicBool::new(false)),
        });
        let subscribers = client.subscribers.clone();
        let connected = client.connected.clone();
        threadlane_daemon::chat::executor()
            .map(|executor| {
                executor.spawn(Self::drive(url, token, command_rx, subscribers, connected));
            })
            .unwrap_or_else(|error| {
                client.emit_error(format!("daemon event executor unavailable: {error}"));
            });
        client
    }

    /// One connection/reconnect loop running for the client's lifetime.
    async fn drive(
        url: String,
        token: Option<String>,
        mut command_rx: mpsc::UnboundedReceiver<SessionCommand>,
        subscribers: Arc<Mutex<Vec<mpsc::UnboundedSender<SessionEvent>>>>,
        connected: Arc<AtomicBool>,
    ) {
        let mut first = true;
        loop {
            if !first {
                connected.store(false, Ordering::SeqCst);
                // A queued SubmitPrompt firing minutes late is worse than a
                // dropped command: fail them all and let the user resend.
                let mut dropped = 0usize;
                while command_rx.try_recv().is_ok() {
                    dropped += 1;
                }
                if dropped > 0 {
                    Self::fanout(&subscribers, SessionEvent::DaemonError {
                        session_id: None,
                        message: format!(
                            "daemon connection dropped {dropped} queued command(s)"
                        ),
                    });
                }
                Self::fanout(&subscribers, SessionEvent::DaemonError {
                    session_id: None,
                    message: "daemon connection lost; reconnecting".to_string(),
                });
                tokio::time::sleep(RECONNECT_DELAY).await;
            }
            first = false;
            let mut request = match url.clone().into_client_request() {
                Ok(request) => request,
                Err(error) => {
                    Self::fanout(&subscribers, SessionEvent::DaemonError {
                        session_id: None,
                        message: format!("invalid daemon url {url}: {error}"),
                    });
                    return;
                }
            };
            if let Some(token) = &token {
                if let Ok(header) =
                    format!("Bearer {token}").parse::<tokio_tungstenite::tungstenite::http::HeaderValue>()
                {
                    request.headers_mut().insert("Authorization", header);
                }
            }
            let (mut socket, _response) =
                match tokio_tungstenite::connect_async(request).await {
                    Ok(pair) => pair,
                    Err(error) => {
                        tracing::warn!("daemon connect to {url} failed: {error}");
                        continue;
                    }
                };
            connected.store(true, Ordering::SeqCst);
            // Server replays its journal tail first, then live events — the
            // same attach semantics LocalDaemon's subscribe() exposes.
            loop {
                tokio::select! {
                    command = command_rx.recv() => {
                        let Some(command) = command else { return };
                        let text = match serde_json::to_string(&command) {
                            Ok(text) => text,
                            Err(error) => {
                                Self::fanout(&subscribers, SessionEvent::DaemonError {
                                    session_id: None,
                                    message: format!("could not encode command: {error}"),
                                });
                                continue;
                            }
                        };
                        if socket.send(Message::Text(text.into())).await.is_err() {
                            break;
                        }
                    }
                    message = socket.next() => {
                        match message {
                            Some(Ok(Message::Text(text))) => {
                                match serde_json::from_str::<SessionEvent>(&text) {
                                    Ok(event) => {
                                        Self::fanout(&subscribers, event);
                                    }
                                    Err(error) => {
                                        Self::fanout(&subscribers, SessionEvent::DaemonError {
                                            session_id: None,
                                            message: format!(
                                                "undecodable daemon event: {error}"
                                            ),
                                        });
                                    }
                                }
                            }
                            Some(Ok(Message::Close(_))) | None => break,
                            Some(Err(error)) => {
                                tracing::warn!("daemon socket error: {error}");
                                break;
                            }
                            // Ping/Pong and binary frames carry no session
                            // semantics on this transport.
                            Some(Ok(_)) => {}
                        }
                    }
                }
            }
        }
    }

    fn fanout(
        subscribers: &Arc<Mutex<Vec<mpsc::UnboundedSender<SessionEvent>>>>,
        event: SessionEvent,
    ) {
        let mut subscribers = subscribers.lock().expect("daemon subscribers poisoned");
        // Prune dead subscribers as we go — a dropped view must not wedge
        // the fanout list.
        subscribers.retain(|tx| tx.send(event.clone()).is_ok());
    }

    fn emit_error(&self, message: String) {
        Self::fanout(&self.subscribers, SessionEvent::DaemonError {
            session_id: None,
            message,
        });
    }
}

#[async_trait]
impl DaemonClient for RemoteDaemon {
    async fn command(&self, command: SessionCommand) -> Result<(), String> {
        if !self.connected.load(Ordering::SeqCst) {
            return Err("daemon is not connected".to_string());
        }
        self.command_tx
            .send(command)
            .map_err(|_| "daemon connection driver is gone".to_string())
    }

    fn subscribe(&self) -> mpsc::UnboundedReceiver<SessionEvent> {
        let (tx, rx) = mpsc::unbounded_channel();
        self.subscribers
            .lock()
            .expect("daemon subscribers poisoned")
            .push(tx);
        rx
    }
}
