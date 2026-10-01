use std::path::PathBuf;

use threadlane_git::GitHubIssueRef;
use threadlane_protocol::{ImageAttachment, OrchestratorMode, ReasoningEffort};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AppAction {
    RecreateActiveWorktree,
    AttachProject(PathBuf),
    SelectSession {
        work_dir: PathBuf,
        session_id: String,
    },
    SettleSession {
        work_dir: PathBuf,
        session_id: String,
        delete_worktree: bool,
    },
    RemoveSession {
        work_dir: PathBuf,
        session_id: String,
        delete_worktree: bool,
    },
    ToggleProject(PathBuf),
    SetSidebarProjectFilter(Option<PathBuf>),
    TogglePinSession {
        work_dir: PathBuf,
        session_id: String,
    },
    BeginNewTask,
    SelectDraftProject(PathBuf),
    SelectWorkMode(crate::WorkMode),
    StartIssueWork {
        work_dir: PathBuf,
        issue: GitHubIssueRef,
        title: String,
    },
    SendPrompt(String),
    SendPromptWithImages {
        text: String,
        images: Vec<ImageAttachment>,
    },
    StageBusyMessage {
        text: String,
        images: Vec<ImageAttachment>,
    },
    QueuePendingMessage,
    SteerPendingMessage,
    DismissPendingMessage,
    /// Drop a still-pending queued follow-up by its queue entry id.
    RemoveQueuedMessage {
        entry_id: String,
    },
    /// Re-route a still-pending queued follow-up into the live steer queue.
    SteerQueuedMessage {
        entry_id: String,
    },
    ToggleToolActivity(String),
    CancelGeneration,
    SelectModel(String),
    SelectReasoningEffort(ReasoningEffort),
    SelectOrchestratorMode(OrchestratorMode),
    /// Applies one setting an external ACP agent exposes.
    ///
    /// Carries the agent's own option id rather than a Threadlane concept:
    /// the setting list is agent-defined and open-ended.
    SetAcpConfigOption {
        config_id: String,
        value: String,
    },
    OpenGitHub,
    OpenAutomations,
    OpenGitHubTab(crate::GitHubTab),
    OpenGitHubIssue {
        work_dir: PathBuf,
        number: u64,
    },
    CloseGitHub,
    OpenSettings,
    CloseSettings,
    SaveOpenAiKey(String),
    SaveOpenCodeKey(String),
    SetActiveCodexAccount(String),
    RemoveCodexAccount(String),
    ToggleReasoningExpanded(String),
    OpenFileInEditor(String),
    OpenFileInEditorAtLine {
        path: String,
        line: usize,
    },
    RunTerminalCommand(String),
    OpenTerminalAt(PathBuf),
}
