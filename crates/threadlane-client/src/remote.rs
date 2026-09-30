//! WebSocket transport: [`RemoteDaemon`] dials a running `threadlane-daemon`
//! process and speaks the `threadlane-protocol::daemon` session contract —
//! bare `SessionCommand` JSON outbound, `{"seq", "event"}` frames inbound.
//!
//! Reconnect semantics: the driver reconnects with capped exponential
//! backoff whenever the socket drops. The daemon journals every event with
//! a monotonic sequence; the client tracks its last-seen `seq` and passes
//! it back as `?since=` on the next attach, so the journal-tail replay
//! delivers exactly the events the client missed — deltas already applied
//! (a half-streamed `TextDelta`, say) never append twice.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use futures::{SinkExt, StreamExt};
use serde::Deserialize;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::Message;

use threadlane_protocol::daemon::{SessionCommand, SessionEvent};

use crate::DaemonClient;

/// First reconnect delay; doubles per failed dial up to
/// [`RECONNECT_BACKOFF_MAX`].
const RECONNECT_BACKOFF_INITIAL: Duration = Duration::from_millis(250);
/// Longest delay between reconnect attempts.
const RECONNECT_BACKOFF_MAX: Duration = Duration::from_secs(5);

/// Server-to-client frame: the journal sequence the event was written
/// under (`0` for frames the daemon synthesizes outside the journal, like
/// lag notices) plus the event itself.
#[derive(Deserialize)]
struct WireEvent {
    seq: u64,
    event: SessionEvent,
}

/// A [`DaemonClient`] that attaches to a remote `threadlane-daemon` over
/// WebSocket.
pub struct RemoteDaemon {
    /// Commands awaiting/in-flight to the server; the driver drains it.
    command_tx: mpsc::UnboundedSender<SessionCommand>,
    /// Every `subscribe()` caller's channel; the reader task fans events out.
    subscribers: Arc<Mutex<Vec<mpsc::UnboundedSender<SessionEvent>>>>,
    /// Errors raised before any subscriber could exist (e.g. a refused
    /// scheme); drained into the first `subscribe()` receiver.
    startup_errors: Mutex<Vec<SessionEvent>>,
    /// Whether a socket is live right now — commands sent while down fail
    /// fast instead of firing stale after a reconnect.
    connected: Arc<AtomicBool>,
    /// Highest journal sequence delivered so far; sent back as `?since=`
    /// on reconnect so the tail replay covers only the gap.
    last_seq: Arc<AtomicU64>,
}

impl RemoteDaemon {
    /// Construct the client and spawn its connection driver on the shared
    /// Threadlane reactor. The returned client is usable immediately —
    /// commands issued before the first successful dial are reported as
    /// errors through the event stream.
    ///
    /// A bearer token over plaintext `ws://` is refused for non-loopback
    /// hosts (the token, and everything it protects, would cross the wire
    /// readable); use `wss://` there. The refusal lands as a `DaemonError`
    /// on the first `subscribe()` receiver.
    pub fn connect(url: impl Into<String>, token: Option<String>) -> Arc<Self> {
        let url = url.into();
        let (command_tx, command_rx) = mpsc::unbounded_channel::<SessionCommand>();
        let client = Arc::new(Self {
            command_tx,
            subscribers: Arc::new(Mutex::new(Vec::new())),
            startup_errors: Mutex::new(Vec::new()),
            connected: Arc::new(AtomicBool::new(false)),
            last_seq: Arc::new(AtomicU64::new(0)),
        });
        if let Some(error) = Self::transport_policy_error(&url, &token) {
            client.push_startup_error(error);
            return client;
        }
        let subscribers = client.subscribers.clone();
        let connected = client.connected.clone();
        let last_seq = client.last_seq.clone();
        threadlane_daemon::chat::executor()
            .map(|executor| {
                executor.spawn(Self::drive(
                    url,
                    token,
                    command_rx,
                    subscribers,
                    connected,
                    last_seq,
                ));
            })
            .unwrap_or_else(|error| {
                client.push_startup_error(format!("daemon event executor unavailable: {error}"));
            });
        client
    }

    /// Transport rules enforced before any dial: a token may only cross a
    /// plaintext socket on loopback.
    fn transport_policy_error(url: &str, token: &Option<String>) -> Option<String> {
        let Some(_) = token else {
            return None;
        };
        let request = match url.to_string().into_client_request() {
            Ok(request) => request,
            Err(error) => return Some(format!("invalid daemon url {url}: {error}")),
        };
        let uri = request.uri();
        let plaintext = matches!(uri.scheme_str(), Some("ws") | Some("http"));
        let loopback = uri
            .host()
            .is_some_and(|host| {
                host == "localhost"
                    || host.ends_with(".localhost")
                    || host
                        .parse::<std::net::IpAddr>()
                        .is_ok_and(|ip| ip.is_loopback())
            });
        (plaintext && !loopback).then(|| {
            format!(
                "refusing {url}: a bearer token may not cross a plaintext \
                 ws:// connection to a non-loopback host; use wss://"
            )
        })
    }

    /// Stash an error subscribers have not had a chance to attach for.
    fn push_startup_error(&self, message: String) {
        self.startup_errors
            .lock()
            .expect("startup errors poisoned")
            .push(SessionEvent::DaemonError {
                session_id: None,
                message,
            });
    }

    /// The dial URL for the next attempt, carrying the resume cursor.
    fn dial_url(url: &str, last_seq: &AtomicU64) -> String {
        let since = last_seq.load(Ordering::SeqCst);
        if since == 0 {
            return url.to_string();
        }
        let separator = if url.contains('?') { '&' } else { '?' };
        format!("{url}{separator}since={since}")
    }

    /// One connection/reconnect loop running for the client's lifetime.
    /// Exits once `command_tx` is dropped — a driver must not outlive its
    /// client dialing forever.
    async fn drive(
        url: String,
        token: Option<String>,
        mut command_rx: mpsc::UnboundedReceiver<SessionCommand>,
        subscribers: Arc<Mutex<Vec<mpsc::UnboundedSender<SessionEvent>>>>,
        connected: Arc<AtomicBool>,
        last_seq: Arc<AtomicU64>,
    ) {
        let mut was_connected = false;
        let mut backoff = RECONNECT_BACKOFF_INITIAL;
        loop {
            if command_rx.is_closed() {
                return;
            }
            if was_connected {
                was_connected = false;
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
            }
            let dial_url = Self::dial_url(&url, &last_seq);
            let mut request = match dial_url.clone().into_client_request() {
                Ok(request) => request,
                Err(error) => {
                    Self::fanout(&subscribers, SessionEvent::DaemonError {
                        session_id: None,
                        message: format!("invalid daemon url {dial_url}: {error}"),
                    });
                    return;
                }
            };
            if let Some(token) = &token {
                match format!("Bearer {token}")
                    .parse::<tokio_tungstenite::tungstenite::http::HeaderValue>()
                {
                    Ok(header) => {
                        request.headers_mut().insert("Authorization", header);
                    }
                    Err(error) => {
                        Self::fanout(&subscribers, SessionEvent::DaemonError {
                            session_id: None,
                            message: format!("could not build auth header: {error}"),
                        });
                        return;
                    }
                }
            }
            let (mut socket, _response) =
                match tokio_tungstenite::connect_async(request).await {
                    Ok(pair) => pair,
                    Err(error) => {
                        tracing::warn!("daemon connect to {dial_url} failed: {error}");
                        tokio::time::sleep(backoff).await;
                        backoff = (backoff * 2).min(RECONNECT_BACKOFF_MAX);
                        continue;
                    }
                };
            was_connected = true;
            backoff = RECONNECT_BACKOFF_INITIAL;
            connected.store(true, Ordering::SeqCst);
            // Server replays the journal tail newer than `?since=` first,
            // then live events — the same attach semantics LocalDaemon's
            // subscribe() exposes.
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
                                match serde_json::from_str::<WireEvent>(&text) {
                                    Ok(frame) => {
                                        if frame.seq > 0 {
                                            last_seq.fetch_max(frame.seq, Ordering::SeqCst);
                                        }
                                        Self::fanout(&subscribers, frame.event);
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
        // Errors raised before this receiver existed (transport policy,
        // executor startup) are delivered first, then live events.
        for event in self
            .startup_errors
            .lock()
            .expect("startup errors poisoned")
            .drain(..)
        {
            if tx.send(event).is_err() {
                return rx;
            }
        }
        self.subscribers
            .lock()
            .expect("daemon subscribers poisoned")
            .push(tx);
        rx
    }
}
