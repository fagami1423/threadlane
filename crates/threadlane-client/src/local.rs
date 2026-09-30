//! In-process transport: [`LocalDaemon`] wraps [`DaemonCore`] so the desktop
//! app exercises exactly the dispatch/event path a remote client would —
//! one code path, one set of semantics, no parallel in-process shortcut.

use std::sync::Arc;

use async_trait::async_trait;
use tokio::sync::{broadcast, mpsc};

use threadlane_daemon::core::DaemonCore;
use threadlane_protocol::daemon::{SessionCommand, SessionEvent};

use crate::DaemonClient;

/// In-process [`DaemonClient`] over an owned [`DaemonCore`].
pub struct LocalDaemon {
    core: Arc<DaemonCore>,
}

impl LocalDaemon {
    /// Construct the in-process client around a fresh [`DaemonCore`].
    pub fn new(core: Arc<DaemonCore>) -> Arc<Self> {
        Arc::new(Self { core })
    }

    /// The owned core, for hosts that manage session-local state the wire
    /// contract does not expose (runtime status sweeps, view caches).
    pub fn core(&self) -> &Arc<DaemonCore> {
        &self.core
    }
}

#[async_trait]
impl DaemonClient for LocalDaemon {
    async fn command(&self, command: SessionCommand) -> Result<(), String> {
        self.core.clone().dispatch(command).await
    }

    fn subscribe(&self) -> mpsc::UnboundedReceiver<SessionEvent> {
        let (tail, mut broadcast_rx) = self.core.subscribe_with_tail();
        let (tx, rx) = mpsc::unbounded_channel();
        match threadlane_daemon::chat::executor() {
            Ok(executor) => {
                executor.spawn(async move {
                    for event in tail {
                        if tx.send(event).is_err() {
                            return;
                        }
                    }
                    loop {
                        match broadcast_rx.recv().await {
                            Ok(event) => {
                                if tx.send(event).is_err() {
                                    return;
                                }
                            }
                            Err(broadcast::error::RecvError::Lagged(skipped)) => {
                                // The journal is the recovery surface: report
                                // the gap so the client can re-snapshot rather
                                // than believe a silently stale state.
                                if tx
                                    .send(SessionEvent::DaemonError {
                                        session_id: None,
                                        message: format!(
                                            "dropped {skipped} daemon events; refresh the session"
                                        ),
                                    })
                                    .is_err()
                                {
                                    return;
                                }
                            }
                            Err(broadcast::error::RecvError::Closed) => return,
                        }
                    }
                });
            }
            Err(error) => {
                let _ = tx.send(SessionEvent::DaemonError {
                    session_id: None,
                    message: format!("daemon event executor unavailable: {error}"),
                });
            }
        }
        rx
    }
}
