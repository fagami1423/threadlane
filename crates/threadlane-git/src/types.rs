use std::path::PathBuf;

pub(crate) const GIT_FIELD_SEPARATOR: char = '\u{1f}';
pub(crate) const GIT_RECORD_SEPARATOR: char = '\u{1e}';

// Canonical in `threadlane_protocol::repo` — the daemon wire contract
// (`SessionCommand::GitRequest`, `CommandResponse::Git`) carries them;
// re-exported here so `threadlane_git::*` paths keep working.
pub use threadlane_protocol::repo::{
    DiffOptions, GitBranchInfo, GitCommitInfo, GitFile, GitFileInventory, GitHubPrCommit,
    GitHubPrFile, GitHubPrInfo, GitStashInfo, GitStatus, PrCheckStatus,
    PrConversationComment, PrReview, PrReviewComment,
};

#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct GitHubRepository {
    pub host: String,
    pub owner: String,
    pub repo: String,
}

// Canonical in `threadlane_protocol::daemon` — the session-list contract
// (`SessionInfo::github_issue`) carries it on the wire; re-exported here so
// `threadlane_git::GitHubIssueRef` paths keep working.
pub use threadlane_protocol::daemon::GitHubIssueRef;

#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct GitHubLabel {
    pub name: String,
    pub color: String,
    pub description: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct GitHubIssueSummary {
    pub issue: GitHubIssueRef,
    pub title: String,
    pub state: String,
    pub author: String,
    pub updated_at: String,
    pub labels: Vec<GitHubLabel>,
    pub assignees: Vec<String>,
    pub comments_count: usize,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct GitHubIssueDetail {
    pub summary: GitHubIssueSummary,
    pub body: String,
    pub comments: Vec<GitHubIssueComment>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct GitHubIssueComment {
    pub(crate) remote_id: String,
    pub author: String,
    pub body: String,
    pub created_at: String,
    pub(crate) url: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct GitHubPullRequestSummary {
    pub repository: GitHubRepository,
    pub number: u64,
    pub title: String,
    pub state: String,
    pub url: String,
    pub is_draft: bool,
    pub head_ref: String,
    pub base_ref: String,
    pub author: String,
    pub updated_at: String,
    pub review_decision: Option<String>,
    pub checks: Vec<PrCheckStatus>,
}

/// Per-account review marker GitHub keeps for one file of a pull request.
/// These are personal reading markers, not approval or review state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PrFileViewedStatus {
    Unviewed,
    Viewed,
    /// Was viewed, then GitHub dismissed the marker after a push changed the file.
    ChangedSinceViewed,
    /// The file was not covered by the fetched viewed-state pages.
    Unknown,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GitHubPrFileViewed {
    pub path: String,
    pub status: PrFileViewedStatus,
}

/// Viewed markers for every reported pull request file, scoped to the
/// signed-in viewer that produced the read.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct GitHubPrViewedState {
    /// GraphQL node ID required by the mark/unmark mutations.
    pub pull_request_id: String,
    /// Head OID the markers were read against.
    pub head_oid: String,
    /// Login of the account these personal markers belong to.
    pub viewer: String,
    pub files: Vec<GitHubPrFileViewed>,
    /// False when pagination stopped before every file was reported.
    pub complete: bool,
}

impl GitHubPrViewedState {
    pub fn file_status(&self, path: &str) -> PrFileViewedStatus {
        self.files
            .iter()
            .find(|file| file.path == path)
            .map(|file| file.status)
            .unwrap_or(PrFileViewedStatus::Unknown)
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PullRequestReviewCommentDraft {
    pub(crate) path: String,
    pub(crate) body: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum PullRequestReviewVerdict {
    Comment,
    Approve,
    RequestChanges,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct GitWorktreeInfo {
    pub path: PathBuf,
    pub branch: Option<String>,
    pub(crate) head: String,
    pub(crate) is_bare: bool,
    pub(crate) is_detached: bool,
    pub(crate) is_locked: bool,
}

/// Whether the current checkout is ready to open a new draft pull request.
pub fn can_create_pull_request(worktree_available: bool, status: Option<&GitStatus>) -> bool {
    worktree_available
        && status.is_some_and(|status| {
            status.pr_ready
                && !status.detached
                && status
                    .branch
                    .as_deref()
                    .is_some_and(|branch| !branch.trim().is_empty())
                && status.remote.is_some()
                && status.has_upstream
                && status.pr_lookup_available
                && status.pr.is_none()
        })
}
