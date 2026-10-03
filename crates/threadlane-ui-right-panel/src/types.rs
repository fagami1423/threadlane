use std::path::{Path, PathBuf};

use gpui_component::IconName;
use threadlane_git::{GitFile, GitStatus};

pub fn can_publish_branch(worktree_available: bool, status: Option<&GitStatus>) -> bool {
    worktree_available
        && status.is_some_and(|status| {
            !status.has_upstream
                && !status.detached
                && status.branch.is_some()
                && status.remote.is_some()
        })
}

pub fn nonempty(value: &str) -> Option<&str> {
    let value = value.trim();
    (!value.is_empty()).then_some(value)
}

pub fn message_generated_matches_active_project(
    origin: &Path,
    active: Option<&Path>,
) -> bool {
    active == Some(origin)
}

pub fn normalize_generated_commit_message(raw: &str) -> String {
    threadlane_provider::normalize_commit_message(raw)
}

pub fn detect_language(path_str: &str) -> &'static str {
    let path = Path::new(path_str);
    match path
        .extension()
        .and_then(|ext| ext.to_str())
        .map(|s| s.to_lowercase())
        .as_deref()
    {
        Some("rs") => "rust",
        Some("py") => "python",
        Some("js" | "mjs" | "cjs") => "javascript",
        Some("ts" | "mts" | "cts" | "jsx" | "tsx") => "typescript",
        Some("json") => "json",
        Some("toml") => "toml",
        Some("yaml" | "yml") => "yaml",
        Some("html" | "htm") => "html",
        Some("css") => "css",
        Some("md" | "markdown") => "markdown",
        Some("sh" | "bash" | "zsh") => "bash",
        Some("go") => "go",
        Some("c" | "h") => "c",
        Some("cpp" | "hpp" | "cc" | "cxx" | "hh") => "cpp",
        Some("diff" | "patch") => "diff",
        Some("zig") => "zig",
        _ => match path
            .file_name()
            .and_then(|name| name.to_str())
            .map(|s| s.to_lowercase())
            .as_deref()
        {
            Some("dockerfile") => "bash",
            Some("cargo.lock") => "toml",
            _ => "text",
        },
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Default)]
pub enum ReviewTab {
    #[default]
    Changes,
    History,
}
#[derive(Copy, Clone, Debug, PartialEq, Eq, Default)]
pub enum ReviewViewMode {
    #[default]
    List,
    Tree,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ReviewDiffTarget {
    File(String),
    AllChanges,
}

impl ReviewDiffTarget {
    pub(crate) fn title(&self) -> String {
        match self {
            Self::File(path) => format!("Review · {path}"),
            Self::AllChanges => "Review · All changes".into(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ReviewDiffRequest {
    pub project: PathBuf,
    pub target: ReviewDiffTarget,
    pub options: threadlane_git::DiffOptions,
    pub revision: u64,
}

pub(crate) enum ReviewDiffState {
    Loading,
    Ready { empty: bool },
    Failed(String),
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Surface {
    Trajectory,
    Agents,
    Review,
    Files,
    Browser,
}

impl Surface {
    pub(crate) fn all() -> Vec<Self> {
        let mut surfaces = vec![Self::Trajectory, Self::Agents, Self::Review, Self::Files];
        #[cfg(any(target_os = "macos", target_os = "linux", target_os = "windows"))]
        surfaces.push(Self::Browser);
        surfaces
    }

    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Trajectory => "Trajectory",
            Self::Agents => "Agents",
            Self::Review => "Review",
            Self::Files => "Files",
            Self::Browser => "Browser",
        }
    }

    pub(crate) fn icon(self) -> IconName {
        match self {
            Self::Trajectory => IconName::GalleryVerticalEnd,
            Self::Agents => IconName::Bot,
            Self::Review => IconName::File,
            Self::Files => IconName::Folder,
            Self::Browser => IconName::Globe,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GitAction {
    Commit,
    CommitAndPush,
    StageFile(String),
    UnstageFile(String),
    StageFiles(Vec<String>),
    UnstageFiles(Vec<String>),
    StashPush {
        message: Option<String>,
        include_untracked: bool,
    },
    Push,
    Pull,
    Fetch,
    StageAll,
    UnstageAll,
    #[allow(dead_code)]
    CreatePullRequest,
    Checkout(String),
    CheckoutStash(String),
    CheckoutCarry(String),
    CreateBranch(String),
    DeleteBranch(String),
    Merge(String),
    PopStash(Option<usize>),
    DropStash(Option<usize>),
    DiscardFile(String),
    DiscardFiles(Vec<String>),
    DiscardAll,
    IgnoreFile(String),
    IgnoreExtension(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DiscardOption {
    Single(String),
    Selected(Vec<String>),
    All(usize),
}

impl DiscardOption {
    pub(crate) fn label(&self) -> String {
        match self {
            Self::Single(_) => "Discard Changes...".to_string(),
            Self::Selected(paths) => format!("Discard Selected Changes ({})...", paths.len()),
            Self::All(count) => format!("Discard All Changes ({count})..."),
        }
    }

    pub(crate) fn git_action(&self) -> GitAction {
        match self {
            Self::Single(path) => GitAction::DiscardFile(path.clone()),
            Self::Selected(paths) => GitAction::DiscardFiles(paths.clone()),
            Self::All(_) => GitAction::DiscardAll,
        }
    }

    pub(crate) fn requires_confirmation(&self) -> bool {
        matches!(self, Self::Selected(_) | Self::All(_))
    }

    pub(crate) fn confirmation_prompt(&self) -> Option<(String, String)> {
        match self {
            Self::Single(_) => None,
            Self::Selected(paths) => {
                let count = paths.len();
                let file_str = if count == 1 { "file" } else { "files" };
                Some((
                    "Discard selected changes?".to_string(),
                    format!(
                        "Are you sure you want to discard changes in {count} selected {file_str}? This cannot be undone."
                    ),
                ))
            }
            Self::All(count) => {
                let file_str = if *count == 1 { "file" } else { "files" };
                Some((
                    "Discard all changes?".to_string(),
                    format!(
                        "Are you sure you want to discard all changes across {count} {file_str}? This cannot be undone."
                    ),
                ))
            }
        }
    }
}

pub fn discard_options(
    clicked_path: &str,
    selected_paths: &[String],
    total_files: usize,
) -> Vec<DiscardOption> {
    let mut options = vec![DiscardOption::Single(clicked_path.to_string())];
    let selected_count = selected_paths.len();
    if selected_count > 1 && selected_count < total_files {
        options.push(DiscardOption::Selected(selected_paths.to_vec()));
    }
    if total_files > 1 {
        options.push(DiscardOption::All(total_files));
    }
    options
}

pub fn selection_bar_discard_options(
    selected_paths: &[String],
    total_files: usize,
) -> Vec<DiscardOption> {
    let mut options = Vec::new();
    let selected_count = selected_paths.len();
    if selected_count > 0 && selected_count < total_files {
        options.push(DiscardOption::Selected(selected_paths.to_vec()));
    }
    if total_files > 0 {
        options.push(DiscardOption::All(total_files));
    }
    options
}

/// Files-surface tree node, delivered daemon-side by
/// `SessionCommand::ListProjectFiles` (protocol v3).
pub type FileNode = threadlane_protocol::repo::ProjectFileNode;

pub enum PanelEvent {
    FilesLoaded {
        project: PathBuf,
        nodes: Vec<FileNode>,
    },
    ReviewLoaded {
        project: PathBuf,
        status: Option<GitStatus>,
        files: Vec<GitFile>,
        error: Option<String>,
    },
    WorkspaceChanged {
        project: PathBuf,
        git_dirty: bool,
        files_dirty: bool,
    },
    MessageGenerated {
        project: PathBuf,
        result: Result<String, String>,
        /// True when the diff sent to the model was truncated at the size
        /// limit (Synara DiffTruncationWarning pattern).
        diff_truncated: bool,
    },
    ActionFinished {
        project: PathBuf,
        status: Result<GitStatus, String>,
        action_error: Option<String>,
        action_message: Option<String>,
        checkout_succeeded: bool,
    },
    CommitFilesLoaded {
        sha: String,
        files: Vec<GitFile>,
    },
    StashFilesLoaded {
        project: PathBuf,
        index: usize,
        files: Vec<GitFile>,
    },
}
