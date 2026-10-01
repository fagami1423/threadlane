//! Minimal WebSocket client for the Threadlane daemon contract.
//!
//! A thin monitoring client needs only the fire-and-forget surface of
//! `SessionCommand`: the daemon answers every request with journaled
//! `SessionEvent`s, so there is no `CommandRequest` machinery here.
//! Reconnect semantics mirror `threadlane-client`'s `RemoteDaemon`:
//! capped exponential backoff, `?since=` journal replay, and synthesized
//! state events so the UI can show connection health.
//!
//! The driver runs on a dedicated Tokio runtime created on first use —
//! GPUI's executors have no reactor.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use futures::{SinkExt, StreamExt};
use serde::Deserialize;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::Message;

use threadlane_protocol::daemon::{SessionCommand, SessionEvent};

/// First reconnect delay; doubles per failed dial up to
/// [`RECONNECT_BACKOFF_MAX`].
const RECONNECT_BACKOFF_INITIAL: Duration = Duration::from_millis(250);
/// Longest delay between reconnect attempts.
const RECONNECT_BACKOFF_MAX: Duration = Duration::from_secs(5);

/// Everything the driver reports to the view: wire events plus the
/// connection-state transitions a `SessionEvent` cannot express.
#[derive(Debug)]
pub enum MobileEvent {
    Event(SessionEvent),
    /// The socket went down; the driver is backing off and will dial again.
    Reconnecting,
    /// The socket is live and the initial `GetProjects` was sent.
    Connected,
    /// The dial loop hit a fatal error (bad URL, auth header) and stopped.
    Fatal(String),
}

/// Server-to-client frame (mirrors the daemon's `{"seq", "event"}` shape;
/// `response` replies are ignored — this client sends no requests).
#[derive(Deserialize)]
struct WireFrame {
    #[serde(default)]
    seq: u64,
    event: Option<SessionEvent>,
}

/// Client half of a daemon connection. `send()` queues a command for the
/// live socket; `events` receives wire events plus synthesized
/// connection-state notifications.
pub struct MobileDaemon {
    command_tx: mpsc::UnboundedSender<SessionCommand>,
    /// Taken once by the view's pump task — the driver fans out to it.
    events: Option<mpsc::UnboundedReceiver<MobileEvent>>,
    connected: Arc<AtomicBool>,
}

impl MobileDaemon {
    /// Connect and spawn the reconnect driver on the shared runtime.
    /// `url` is `ws://host:port`; `token` goes in the `Authorization`
    /// header exactly as the desktop pairing server expects.
    pub fn connect(url: String, token: Option<String>) -> Self {
        let (command_tx, command_rx) = mpsc::unbounded_channel::<SessionCommand>();
        let (event_tx, events) = mpsc::unbounded_channel::<MobileEvent>();
        let connected = Arc::new(AtomicBool::new(false));
        runtime().spawn(Self::drive(
            url,
            token,
            command_rx,
            event_tx,
            connected.clone(),
        ));
        Self {
            command_tx,
            events: Some(events),
            connected,
        }
    }

    /// Whether a socket is live right now.
    pub fn is_connected(&self) -> bool {
        self.connected.load(Ordering::SeqCst)
    }

    /// Queue a command; silently dropped while the socket is down (the
    /// view disables sends while `Reconnecting`).
    pub fn send(&self, command: SessionCommand) {
        let _ = self.command_tx.send(command);
    }

    /// Hand the event stream to the view's pump task (once).
    pub fn take_events(&mut self) -> Option<mpsc::UnboundedReceiver<MobileEvent>> {
        self.events.take()
    }

    /// One connect/reconnect loop running until the client is dropped
    /// (dropping `MobileDaemon` closes `command_tx` and ends the driver).
    async fn drive(
        url: String,
        token: Option<String>,
        mut command_rx: mpsc::UnboundedReceiver<SessionCommand>,
        event_tx: mpsc::UnboundedSender<MobileEvent>,
        connected: Arc<AtomicBool>,
    ) {
        let last_seq = AtomicU64::new(0);
        let mut was_connected = false;
        let mut backoff = RECONNECT_BACKOFF_INITIAL;
        loop {
            if command_rx.is_closed() {
                return;
            }
            if was_connected {
                was_connected = false;
                connected.store(false, Ordering::SeqCst);
                // Queued commands can never reach the dead socket; drop
                // them and report instead of firing stale on reconnect.
                let mut dropped = 0usize;
                while command_rx.try_recv().is_ok() {
                    dropped += 1;
                }
                if dropped > 0 {
                    Self::fanout(&event_tx, SessionEvent::DaemonError {
                        session_id: None,
                        message: format!("connection dropped {dropped} queued command(s)"),
                    });
                }
                let _ = event_tx.send(MobileEvent::Reconnecting);
            }
            let since = last_seq.load(Ordering::SeqCst);
            let dial_url = if since == 0 {
                url.clone()
            } else {
                let separator = if url.contains('?') { '&' } else { '?' };
                format!("{url}{separator}since={since}")
            };
            let mut request = match dial_url.clone().into_client_request() {
                Ok(request) => request,
                Err(error) => {
                    let _ = event_tx
                        .send(MobileEvent::Fatal(format!("invalid url {dial_url}: {error}")));
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
                        let _ = event_tx
                            .send(MobileEvent::Fatal(format!("invalid auth header: {error}")));
                        return;
                    }
                }
            }
            let (mut socket, _response) = match tokio_tungstenite::connect_async(request).await {
                Ok(pair) => pair,
                Err(error) => {
                    log::warn!("daemon connect to {dial_url} failed: {error}");
                    if !was_connected {
                        let _ = event_tx.send(MobileEvent::Reconnecting);
                    }
                    tokio::time::sleep(backoff).await;
                    backoff = (backoff * 2).min(RECONNECT_BACKOFF_MAX);
                    continue;
                }
            };
            was_connected = true;
            backoff = RECONNECT_BACKOFF_INITIAL;
            connected.store(true, Ordering::SeqCst);
            let _ = event_tx.send(MobileEvent::Connected);
            // Ask for the attached project list after every (re)dial — the
            // journal tail may not still hold the original attach events.
            if let Ok(text) = serde_json::to_string(&SessionCommand::GetProjects) {
                if socket.send(Message::Text(text.into())).await.is_err() {
                    continue;
                }
            }
            loop {
                tokio::select! {
                    command = command_rx.recv() => {
                        let Some(command) = command else { return };
                        let text = match serde_json::to_string(&command) {
                            Ok(text) => text,
                            Err(error) => {
                                Self::fanout(&event_tx, SessionEvent::DaemonError {
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
                                match serde_json::from_str::<WireFrame>(&text) {
                                    Ok(frame) => {
                                        if let Some(event) = frame.event {
                                            if frame.seq > 0 {
                                                last_seq.fetch_max(frame.seq, Ordering::SeqCst);
                                            }
                                            let _ = event_tx.send(MobileEvent::Event(event));
                                        }
                                    }
                                    Err(error) => {
                                        Self::fanout(&event_tx, SessionEvent::DaemonError {
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
                                log::warn!("daemon socket error: {error}");
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

    fn fanout(event_tx: &mpsc::UnboundedSender<MobileEvent>, event: SessionEvent) {
        let _ = event_tx.send(MobileEvent::Event(event));
    }
}

/// Shared Tokio reactor for every driver. Created lazily so the static
/// library links without touching Tokio on non-mobile targets.
fn runtime() -> &'static tokio::runtime::Runtime {
    static RUNTIME: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
    RUNTIME.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("mobile daemon runtime")
    })
}
