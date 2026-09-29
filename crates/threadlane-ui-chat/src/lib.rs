mod composer;
mod context_meter;
mod markdown;
mod trajectory;
mod trajectory_view;
mod transcript;
mod view;

pub use trajectory_view::TrajectoryView;
pub use view::{init, CentralTab, ChatListView};

// Owned by the surface that offers it; the workspace handles panel navigation.
gpui::actions!(
    threadlane_chat,
    [
        OpenWorkspaceReview,
        OpenWorkspaceBranches,
        OpenWorkspaceCommit,
        PullWorkspaceBranch,
        PushWorkspaceBranch,
        CreateWorkspacePullRequest,
        CreateWorkspaceBranch,
        OpenWorkspaceFiles,
        OpenWorkspaceAgents,
        OpenWorkspaceTrajectory
    ]
);
