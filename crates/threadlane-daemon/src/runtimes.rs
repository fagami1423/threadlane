//! Daemon-side ownership of live session runtimes.
//!
//! `WorktreePrepared` travels the event stream as serializable
//! [`crate::types::SessionInfo`] data — a runtime handle cannot cross the
//! wire — so the freshly constructed runtime waits here, keyed by
//! `session_id`, until the consumer that processes the prepared event claims
//! it into its own `session_runtimes` map. Same in-process hand-off as
//! before; the event itself now stays wire-clean.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};

use threadlane_coding_agent::controller::SessionRuntime;

fn prepared() -> &'static Mutex<HashMap<String, Arc<SessionRuntime>>> {
    static PREPARED: OnceLock<Mutex<HashMap<String, Arc<SessionRuntime>>>> = OnceLock::new();
    PREPARED.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Parks a runtime built by worktree preparation until the `WorktreePrepared`
/// event consumer claims it.
pub fn park_prepared_runtime(session_id: String, runtime: Arc<SessionRuntime>) {
    if let Ok(mut map) = prepared().lock() {
        map.insert(session_id, runtime);
    }
}

/// Claims the parked runtime for `session_id`, if one is still waiting.
pub fn take_prepared_runtime(session_id: &str) -> Option<Arc<SessionRuntime>> {
    prepared()
        .lock()
        .ok()
        .and_then(|mut map| map.remove(session_id))
}
