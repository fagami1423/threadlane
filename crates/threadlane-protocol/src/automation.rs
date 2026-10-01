//! Automation wire contract: the durable recurring-task store remote
//! clients drive through `SessionCommand::AutomationRequest` and observe
//! through `SessionEvent::AutomationChanged`.
//!
//! The store types themselves are canonical in `threadlane-automation` —
//! re-exported here so a client names the same `Definition`/`Run` it would
//! build on desktop, with no wire-side mirror to drift.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::interaction::{PermissionRequest, QuestionRequest};

pub use threadlane_automation::{Definition, Run, RunStatus, Schedule, Snapshot};

/// One automation-store operation a client issues inside
/// `SessionCommand::AutomationRequest`. Every command answers
/// `CommandResponse::Automation` carrying the post-command projection —
/// reads and mutations share one reply shape so a client re-renders from a
/// single payload.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "command", rename_all = "snake_case")]
pub enum AutomationCommand {
    /// Current definitions, runs, and pending requests.
    GetSnapshot,
    /// Create or replace a definition (validated + persisted by the store).
    Save { definition: Definition },
    /// Toggle whether a definition dispatches on its schedule.
    SetEnabled { id: String, enabled: bool },
    /// Remove a definition and its recorded runs.
    Delete { id: String },
    /// Remove one run record (a deleted active run is cancelled).
    DeleteRun { id: String },
    /// Queue a manual run of the definition now.
    RunNow { id: String },
    /// Cancel the active or queued run with this run id.
    Cancel { id: String },
    /// Clear a finished run's unreviewed marker.
    Review { id: String },
}

/// The automation store's observable state as it crosses the wire. Mirrors
/// the daemon-side service projection except `active_runtime` — an
/// in-process handle remote clients reach as `active_session_id` (the live
/// run's chat session, opened with the usual session commands).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct AutomationProjection {
    pub snapshot: Snapshot,
    /// Permission requests raised by the live run, keyed by session id —
    /// answered with the normal `AnswerPermission` command.
    pub permissions: HashMap<String, PermissionRequest>,
    /// Question requests raised by the live run, keyed by session id —
    /// answered with the normal `AnswerQuestion` command.
    pub questions: HashMap<String, QuestionRequest>,
    /// Session id of the run currently executing, when one is.
    pub active_session_id: Option<String>,
}

/// Payload of `CommandResponse::Automation`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AutomationResponse {
    /// The store's post-command projection.
    Projection { projection: AutomationProjection },
}
