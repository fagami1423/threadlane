//! Client surface for the Threadlane daemon.
//!
//! [`DaemonClient`] is the only boundary the UI talks to: commands go in as
//! serializable [`SessionCommand`]s, events come out as [`SessionEvent`]s.
//! Two transports implement it:
//!
//! - [`LocalDaemon`] — owns a [`DaemonCore`] in-process. The desktop app's
//!   zero-regression path; identical semantics to the pre-split in-process
//!   services.
//! - [`RemoteDaemon`] — a WebSocket client speaking bare `SessionCommand`
//!   JSON outbound and `SessionEvent` JSON inbound. On every (re)connect the
//!   server replays its bounded journal tail before live events, which is
//!   what makes attach-mid-run work.
//!
//! Framing is one JSON value per WebSocket text message — no envelope. The
//! client-facing `subscribe()` receiver collapses every transport into one
//! ordered `SessionEvent` stream so views never learn which side they are on.

mod local;
mod remote;

pub use local::LocalDaemon;
pub use remote::RemoteDaemon;

use async_trait::async_trait;
use tokio::sync::mpsc;
use threadlane_protocol::daemon::{SessionCommand, SessionEvent};

/// The session contract a daemon serves and clients consume.
#[async_trait]
pub trait DaemonClient: Send + Sync {
    /// Dispatch one command. `Err` is also surfaced to subscribers as
    /// `SessionEvent::DaemonError` by every transport, so UI error handling
    /// lives in the event drain alone.
    async fn command(&self, command: SessionCommand) -> Result<(), String>;

    /// Attach to the daemon's event stream. Each call returns an
    /// independent receiver; journal replay (for late attach/reconnect)
    /// is decided by the transport, not the caller.
    fn subscribe(&self) -> mpsc::UnboundedReceiver<SessionEvent>;
}
