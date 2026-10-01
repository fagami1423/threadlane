//! Terminal backend that runs shells on the attached daemon host.
//!
//! The workspace selects this when `AppState::daemon_remote` is set so the
//! PTY belongs to the remote machine, per the daemon-split goal: output and
//! lifecycle frames stream back over `SessionEvent::TerminalEvent` routed by
//! a client-chosen `terminal_id`, and `TerminalOpen`/`Input`/`Resize`/
//! `Close` commands drive the other direction through `TerminalBus`.

use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc;

use threadlane_protocol::daemon::{SessionCommand, TerminalEvent};
use threadlane_ui_state::TerminalBus;
use threadlane_ui_terminal::{TerminalBackend, TerminalEventSink, TerminalIo};

/// Terminal ids are client-chosen and unique per spawn: a restarted view's
/// new id keeps late frames from the old PTY out of the fresh emulator.
static NEXT_TERMINAL_ID: AtomicU64 = AtomicU64::new(1);

/// Spawns shells on the daemon this `TerminalBus` points at. The daemon
/// owns the real PTY; this side only forwards bytes and resizes.
pub struct RemoteTerminalBackend {
    bus: TerminalBus,
}

impl RemoteTerminalBackend {
    pub fn new(bus: TerminalBus) -> Self {
        Self { bus }
    }
}

impl TerminalBackend for RemoteTerminalBackend {
    fn spawn(
        &self,
        cwd: &Path,
        rows: u16,
        cols: u16,
        output_tx: mpsc::SyncSender<Vec<u8>>,
        sink: TerminalEventSink,
    ) -> Result<Box<dyn TerminalIo>, String> {
        let terminal_id = format!(
            "gpui-{}-{}",
            std::process::id(),
            NEXT_TERMINAL_ID.fetch_add(1, Ordering::SeqCst)
        );
        let mut events = self.bus.events();
        // Drains daemon frames for this terminal into the view's parser
        // channel. A std thread with blocking_recv, not the shared runtime:
        // the sync mpsc send could block a runtime worker. The thread exits
        // on the daemon's Exited frame (TerminalClose → kill → EOF produces
        // one) or when the parser channel closes with the view.
        let wanted_id = terminal_id.clone();
        std::thread::Builder::new()
            .name(format!("threadlane-remote-terminal-{terminal_id}"))
            .spawn(move || loop {
                match events.blocking_recv() {
                    Ok(TerminalEvent::Output { terminal_id, data })
                        if terminal_id == wanted_id =>
                    {
                        if output_tx.send(data.into_bytes()).is_err() {
                            break;
                        }
                    }
                    Ok(TerminalEvent::Exited { terminal_id, .. })
                        if terminal_id == wanted_id =>
                    {
                        sink.closed();
                        break;
                    }
                    Ok(_) => {}
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            })
            .map_err(|error| format!("could not spawn remote terminal drainer: {error}"))?;
        self.bus.command(SessionCommand::TerminalOpen {
            terminal_id: terminal_id.clone(),
            cwd: cwd.to_path_buf(),
            cols,
            rows,
        });
        Ok(Box::new(RemoteTerminalIo {
            terminal_id,
            bus: self.bus.clone(),
        }))
    }
}

/// Write/resize side of a daemon-hosted terminal.
struct RemoteTerminalIo {
    terminal_id: String,
    bus: TerminalBus,
}

impl TerminalIo for RemoteTerminalIo {
    fn write(&self, bytes: &[u8]) {
        self.bus.command(SessionCommand::TerminalInput {
            terminal_id: self.terminal_id.clone(),
            data: String::from_utf8_lossy(bytes).into_owned(),
        });
    }

    fn resize(&self, rows: u16, cols: u16) {
        self.bus.command(SessionCommand::TerminalResize {
            terminal_id: self.terminal_id.clone(),
            cols,
            rows,
        });
    }
}

impl Drop for RemoteTerminalIo {
    fn drop(&mut self) {
        self.bus.command(SessionCommand::TerminalClose {
            terminal_id: self.terminal_id.clone(),
        });
    }
}
