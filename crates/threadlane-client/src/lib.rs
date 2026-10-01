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
//! Framing is one JSON value per WebSocket text message — bare commands
//! outbound (a `CommandRequest` envelope for reply-carrying commands),
//! `{"seq", "event"}` and `{"response"}` frames inbound. The client-facing
//! `subscribe()` receiver collapses every transport into one ordered
//! `SessionEvent` stream so views never learn which side they are on.

mod local;
mod remote;

pub use local::LocalDaemon;
pub use remote::RemoteDaemon;

use async_trait::async_trait;
use tokio::sync::mpsc;
use threadlane_protocol::daemon::{
    CommandRequest, CommandResponse, SessionCommand, SessionEvent,
};

/// The session contract a daemon serves and clients consume.
#[async_trait]
pub trait DaemonClient: Send + Sync {
    /// Dispatch one command. Callers must handle the returned `Err`;
    /// `SessionEvent::DaemonError` delivery is transport-specific
    /// (`AppState::dispatch_command` also converts `Err` into the event
    /// stream for its callers).
    async fn command(&self, command: SessionCommand) -> Result<(), String>;

    /// Dispatch one command expecting the daemon's [`CommandResponse`]
    /// back: `Ok` carries the payload (`Ack` for commands that return
    /// nothing), `Err` is the send or dispatch failure (the daemon also
    /// broadcasts a dispatch failure as `DaemonError`). `request_id` is
    /// caller-chosen and must be unique per client connection.
    async fn command_request(
        &self,
        request: CommandRequest,
    ) -> Result<CommandResponse, String>;

    /// Attach to the daemon's event stream. Each call returns an
    /// independent receiver; journal replay (for late attach/reconnect)
    /// is decided by the transport, not the caller.
    fn subscribe(&self) -> mpsc::UnboundedReceiver<SessionEvent>;
}
