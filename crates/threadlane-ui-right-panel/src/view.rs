use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::Duration;

use gpui::prelude::FluentBuilder;
use gpui::*;
use gpui_component::button::{Button, ButtonVariant, ButtonVariants};
use gpui_component::dialog::DialogButtonProps;
use gpui_component::checkbox::Checkbox;
use gpui_component::input::{Editor, EditorState, Input, InputEvent, InputState, TabSize};
use gpui_component::list::ListItem;
use gpui_component::menu::{ContextMenuExt, PopupMenuItem};
use gpui_component::notification::Notification;
use gpui_component::radio::{Radio, RadioGroup};
use gpui_component::scroll::ScrollableElement;
use gpui_component::separator::Separator;
use gpui_component::spinner::Spinner;
use gpui_component::tab::{Tab, TabBar};
use gpui_component::tag::{Tag, TagVariant};
use gpui_component::text::{TextView, TextViewState};
use gpui_component::tree::{Tree, TreeEvent, TreeItem, TreeState};
use gpui_component::tooltip::Tooltip;
use gpui_component::{ActiveTheme, Disableable, Icon, IconName, Selectable, Sizable, WindowExt};
use threadlane_git::{can_create_pull_request, GitBranchInfo, GitCommitInfo, GitFile, GitStatus};

use threadlane_project::watcher::WorkspaceWatcher;
use threadlane_ui_state::AppState;
use threadlane_ui_state::next_event_batch;

use super::agents::AgentsPanel;
use super::browser::BrowserView;
use super::draft_pr::{DraftPrContextKey, DraftPrDialogView, draft_pr_prefill};
pub use super::types::{
    can_publish_branch, detect_language, discard_options, message_generated_matches_active_project,
    normalize_generated_commit_message, selection_bar_discard_options, DiscardOption, FileNode,
    GitAction, PanelEvent, ReviewTab, ReviewViewMode, Surface,
};
use super::types::{ReviewDiffRequest, ReviewDiffState, ReviewDiffTarget};

pub struct RightPanelView {
    pub(crate) model: Entity<AppState>,
    agents: Entity<AgentsPanel>,
    /// Live trajectory surface mounted from the workspace (owned by
    /// threadlane-ui-chat; injected type-erased to keep this crate chat-free).
    trajectory_view: Option<AnyView>,
    active_surface: Option<Surface>,
    visible: bool,
    project: Option<PathBuf>,
    worktree_unavailable: bool,
    /// Whether the attached daemon answered `supports_project_io` at the
    /// last sync. Part of the sync key: a remote client's handshake can
    /// still be in flight when the panel first syncs, and the capability
    /// flipping true later must re-run the watch arm instead of leaving
    /// the client-side `WorkspaceWatcher` in place.
    project_io_supported: bool,
    tree_state: Entity<TreeState>,
    expanded_paths: HashSet<String>,
    review_tab: ReviewTab,
    history_filter_input: Entity<InputState>,
    selected_commit_sha: Option<String>,
    selected_commit_files: Vec<GitFile>,
    loading_commit_sha: Option<String>,
    review_files: Vec<GitFile>,
    review_files_list_state: ListState,
    selected_files: HashSet<String>,
    review_selection_initialized: bool,
    review_filter_input: Entity<InputState>,
    review_view_mode: ReviewViewMode,
    stash_dialog_open: bool,
    stash_message_input: Entity<InputState>,
    stash_include_untracked: bool,
    collapsed_tree_folders: HashSet<String>,
    review_diff_revision: u64,
    review_diff_options: threadlane_git::DiffOptions,
    review_diff_request: Option<ReviewDiffRequest>,
    review_diff_state: Option<ReviewDiffState>,
    #[cfg(test)]
    review_diff_load_count: usize,
    git_status: Option<GitStatus>,
    draft_pr_context_revision: u64,
    review_error: Option<String>,
    commit_message_input: Entity<InputState>,
    generated_commit_message: Option<String>,
    should_clear_commit_message: bool,
    git_busy: bool,
    git_checkout_pending: bool,
    git_message_pending: bool,
    pub(crate) git_feedback: Option<String>,
    pending_git_notifications: Vec<Notification>,
    branch_popover_open: bool,
    branch_filter_input: Entity<InputState>,
    new_branch_dialog_open: bool,
    git_dialog_presented: bool,
    new_branch_name_input: Entity<InputState>,
    merge_dialog_open: bool,
    merge_filter_input: Entity<InputState>,
    merge_selected_branch: Option<String>,
    switch_dialog_open: bool,
    switch_target_branch: Option<String>,
    switch_stash_mode: bool,
    stash_expanded: bool,
    pr_expanded: bool,
    stash_files: Option<(usize, Vec<GitFile>)>,
    loading_stash_index: Option<usize>,
    last_fetched_time: Option<std::time::Instant>,
    document_title: Option<String>,
    document_state: Entity<TextViewState>,
    editor_state: Option<Entity<EditorState>>,
    editor_subscription: Option<Subscription>,
    saved_content: String,
    is_dirty: bool,
    pending_document: Option<(String, String)>,
    browser: Option<Entity<BrowserView>>,
    event_tx: tokio::sync::mpsc::UnboundedSender<PanelEvent>,
    _watcher: Option<WorkspaceWatcher>,
    /// Project root currently held by a daemon-side `WatchProject`
    /// subscription; unwatched on switch. `None` when the attached daemon
    /// predates project-io (the local `WorkspaceWatcher` covers that
    /// single-host case) or when nothing is selected.
    watched_project: Option<PathBuf>,
    _subscriptions: Vec<Subscription>,
}

impl RightPanelView {
    pub fn new(model: Entity<AppState>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let agents = cx.new(|cx| AgentsPanel::new(model.clone(), window, cx));
        let document_state = cx.new(|cx| TextViewState::markdown("", cx));
        let tree_state = cx.new(|cx| TreeState::new(cx));
        let commit_message_input =
            cx.new(|cx| InputState::new(window, cx).placeholder("Summary (required)"));
        let branch_filter_input =
            cx.new(|cx| InputState::new(window, cx).placeholder("Filter branches…"));
        let new_branch_name_input =
            cx.new(|cx| InputState::new(window, cx).placeholder("e.g. feature/new-workflow"));
        let merge_filter_input =
            cx.new(|cx| InputState::new(window, cx).placeholder("Filter branches to merge…"));
        let history_filter_input =
            cx.new(|cx| InputState::new(window, cx).placeholder("Filter commits…"));
        let review_filter_input =
            cx.new(|cx| InputState::new(window, cx).placeholder("Filter changes…"));
        let stash_message_input =
            cx.new(|cx| InputState::new(window, cx).placeholder("Stash message (optional)"));
        let (event_tx, mut event_rx) = tokio::sync::mpsc::unbounded_channel();

        cx.spawn(async move |this, cx| {
            while let Some(events) = next_event_batch(&mut event_rx).await {
                let _ = this.update(cx, |this, cx| {
                    for event in events {
                        this.apply_event(event, cx);
                    }
                    cx.notify();
                });
            }
        })
        .detach();

        // Daemon-side `WorkspaceChanged` events (emitted by its
        // `WatchProject` registry) reach this client's own subscription;
        // forwarding them as `PanelEvent`s gives the surfaces the same
        // refresh signal the local watcher used to send. A pre-3 daemon
        // emits none — the local watcher remains the source there.
        {
            let daemon_client = model.read(cx).daemon_client.clone();
            let mut daemon_events = daemon_client.subscribe();
            let workspace_tx = event_tx.clone();
            if let Ok(executor) = threadlane_ui_state::chat::executor() {
                executor.spawn(async move {
                    while let Some(event) = daemon_events.recv().await {
                        if let threadlane_protocol::daemon::SessionEvent::WorkspaceChanged {
                            work_dir,
                            git_dirty,
                            files_dirty,
                        } = event
                        {
                            let _ = workspace_tx.send(PanelEvent::WorkspaceChanged {
                                project: work_dir,
                                git_dirty,
                                files_dirty,
                            });
                        }
                    }
                });
            }
        }

        // Agent browser commands arrive from tokio tool workers, which cannot
        // touch entities directly. The first panel to construct claims the
        // shared receiver and pumps commands into the live browser view.
        // Script evaluations park here on a oneshot without blocking the UI;
        // the session side bounds every round-trip with its own timeout.
        if let Some(mut browser_rx) = model.read(cx).browser_bridge.take_receiver() {
            cx.spawn_in(window, async move |this, cx| {
                while let Some(request) = browser_rx.recv().await {
                    let step = this
                        .update_in(cx, |this, window, cx| {
                            start_browser_request(this, request.command, window, cx)
                        })
                        .unwrap_or_else(|_| {
                            BrowserReply::Ready(Err(
                                "The browser panel is no longer available.".to_string()
                            ))
                        });
                    let reply = match step {
                        BrowserReply::Ready(reply) => reply,
                        BrowserReply::PendingEval(rx) => rx
                            .await
                            .map_err(|_| "The browser dropped the evaluation.".to_string())
                            .map(|payload| finalize_browser_eval(&payload)),
                        BrowserReply::PendingSnapshot(rx) => match rx.await {
                            Ok(Ok((bytes, width, height))) => {
                                let (path, data_url) = this
                                    .update(cx, |this, _cx| this.save_browser_screenshot(&bytes))
                                    .unwrap_or_else(|_| (None, base64_data_url(&bytes)));
                                let payload = serde_json::json!({
                                    "width": width,
                                    "height": height,
                                    "data_url": data_url,
                                    "path": path,
                                })
                                .to_string();
                                Ok(payload)
                            }
                            Ok(Err(err)) => Err(err),
                            Err(_) => Err("The browser dropped the snapshot.".to_string()),
                        },
                        BrowserReply::PendingWait {
                            selector,
                            text,
                            deadline,
                        } => {
                            let mut outcome =
                                Err("Timed out waiting for condition in browser.".to_string());
                            while std::time::Instant::now() < deadline {
                                let check_script = super::browser::wait_check_js(
                                    selector.as_deref(),
                                    text.as_deref(),
                                );
                                let eval_rx = this.update(cx, |this, cx| {
                                    this.start_browser_eval(&check_script, cx)
                                });
                                match eval_rx {
                                    Ok(Ok(rx)) => {
                                        if let Ok(raw) = rx.await {
                                            let payload =
                                                super::browser::unwrap_callback_payload(&raw);
                                            if let Ok(v) =
                                                serde_json::from_str::<serde_json::Value>(&payload)
                                            {
                                                if v.get("ok").and_then(|b| b.as_bool())
                                                    == Some(true)
                                                {
                                                    let sel_found = v
                                                        .get("selectorFound")
                                                        .and_then(|b| b.as_bool());
                                                    let txt_found = v
                                                        .get("textFound")
                                                        .and_then(|b| b.as_bool());
                                                    let ready = v
                                                        .get("readyState")
                                                        .and_then(|s| s.as_str())
                                                        == Some("complete");

                                                    let sel_ok = selector.is_none()
                                                        || sel_found == Some(true);
                                                    let txt_ok =
                                                        text.is_none() || txt_found == Some(true);
                                                    let ready_ok = (selector.is_some()
                                                        || text.is_some())
                                                        || ready;

                                                    if sel_ok && txt_ok && ready_ok {
                                                        outcome =
                                                            Ok("Condition satisfied in browser."
                                                                .to_string());
                                                        break;
                                                    }
                                                } else if let Some(err) =
                                                    v.get("error").and_then(|s| s.as_str())
                                                {
                                                    outcome = Err(err.to_string());
                                                    break;
                                                }
                                            }
                                        }
                                    }
                                    _ => {
                                        outcome =
                                            Err("Browser panel closed during wait.".to_string());
                                        break;
                                    }
                                }
                                // The bridge pump runs on GPUI, outside Tokio's reactor.
                                cx.background_executor()
                                    .timer(Duration::from_millis(150))
                                    .await;
                            }
                            outcome
                        }
                    };
                    let _ = request.reply.send(reply);
                }
            })
            .detach();
        }

        let observe_model = cx.observe(&model, |this, _model, cx| {
            this.sync_project(cx);
            cx.notify();
        });
        let tree_subscription =
            cx.subscribe(
                &tree_state,
                |this, _tree, event: &TreeEvent, _cx| match event {
                    TreeEvent::Expanded(id) => {
                        this.expanded_paths.insert(id.to_string());
                    }
                    TreeEvent::Collapsed(id) => {
                        this.expanded_paths.remove(id.as_ref());
                    }
                },
            );
        // Eager so agent browser commands always have a live view to act on,
        // even before the user opens the tab. Hidden until selected.
        // GPUI test windows have no native handle for Wry; Git UI tests do not use the browser.
        let browser_model = model.clone();
        let browser = (!cfg!(test)).then(|| {
            let browser = cx.new(|cx| BrowserView::new(browser_model.clone(), window, cx));
            browser.update(cx, |browser, cx| browser.set_visible(false, cx));
            browser
        });

        let mut panel = Self {
            model,
            agents,
            trajectory_view: None,
            active_surface: None,
            visible: false,
            project: None,
            worktree_unavailable: false,
            project_io_supported: false,
            tree_state,
            expanded_paths: HashSet::new(),
            review_tab: ReviewTab::Changes,
            history_filter_input,
            selected_commit_sha: None,
            selected_commit_files: Vec::new(),
            loading_commit_sha: None,
            review_files: Vec::new(),
            review_files_list_state: ListState::new(
                0,
                ListAlignment::Top,
                window.rem_size() * 10.0,
            )
            .with_uniform_item_height(window.rem_size() * 2.0),
            selected_files: HashSet::new(),
            review_selection_initialized: false,
            review_filter_input,
            review_view_mode: ReviewViewMode::List,
            stash_dialog_open: false,
            stash_message_input,
            stash_include_untracked: true,
            collapsed_tree_folders: HashSet::new(),
            review_diff_revision: 0,
            review_diff_options: threadlane_git::DiffOptions::default(),
            review_diff_request: None,
            review_diff_state: None,
            #[cfg(test)]
            review_diff_load_count: 0,
            git_status: None,
            draft_pr_context_revision: 0,
            review_error: None,
            commit_message_input,
            generated_commit_message: None,
            should_clear_commit_message: false,
            git_busy: false,
            git_checkout_pending: false,
            git_message_pending: false,
            git_feedback: None,
            pending_git_notifications: Vec::new(),
            branch_popover_open: false,
            branch_filter_input,
            new_branch_dialog_open: false,
            git_dialog_presented: false,
            new_branch_name_input,
            merge_dialog_open: false,
            merge_filter_input,
            merge_selected_branch: None,
            switch_dialog_open: false,
            switch_target_branch: None,
            switch_stash_mode: true,
            stash_expanded: false,
            pr_expanded: true,
            stash_files: None,
            loading_stash_index: None,
            last_fetched_time: None,
            document_title: None,
            document_state,
            editor_state: None,
            editor_subscription: None,
            saved_content: String::new(),
            is_dirty: false,
            pending_document: None,
            browser,
            event_tx,
            _watcher: None,
            watched_project: None,
            _subscriptions: vec![observe_model, tree_subscription],
        };
        panel.sync_project(cx);
        panel
    }

    fn sync_project(&mut self, cx: &mut Context<Self>) {
        let (project, worktree_unavailable, project_io_supported) = {
            let state = self.model.read(cx);
            let project = state.active_git_work_dir();
            let unavailable = state.active_work_dir.is_some()
                && state.active_session_id.is_some()
                && project.is_none();
            (
                project,
                unavailable,
                state.daemon_client.supports_project_io(),
            )
        };
        if self.project == project
            && self.worktree_unavailable == worktree_unavailable
            && self.project_io_supported == project_io_supported
        {
            return;
        }
        self.project = project.clone();
        self.worktree_unavailable = worktree_unavailable;
        self.project_io_supported = project_io_supported;
        self.draft_pr_context_revision = self.draft_pr_context_revision.wrapping_add(1);
        self.tree_state
            .update(cx, |state, cx| state.set_items(Vec::new(), cx));
        self.expanded_paths.clear();
        self.review_files.clear();
        self.selected_files.clear();
        self.review_selection_initialized = false;
        self.review_diff_revision = self.review_diff_revision.wrapping_add(1);
        self.pending_document = None;
        self.review_diff_options = threadlane_git::DiffOptions::default();
        self.review_diff_request = None;
        self.review_diff_state = None;
        self.git_checkout_pending = false;
        self.git_status = None;
        self.review_error = None;
        self.git_feedback = None;
        self.git_message_pending = false;
        self.generated_commit_message = None;
        self.should_clear_commit_message = false;
        self.selected_commit_sha = None;
        self.selected_commit_files.clear();
        self.loading_commit_sha = None;
        self.stash_files = None;
        self.loading_stash_index = None;
        self.stash_expanded = false;
        self.document_title = None;
        self.document_state
            .update(cx, |state, cx| state.set_text("", cx));

        let daemon_client = self.model.read(cx).daemon_client.clone();
        if let Some(work_dir) = project {
            if daemon_client.supports_project_io() {
                self._watcher = None;
                if self.watched_project.as_ref() != Some(&work_dir) {
                    if let Some(previous) = self.watched_project.take() {
                        let client = daemon_client.clone();
                        if let Ok(executor) = threadlane_ui_state::chat::executor() {
                            executor.spawn(async move {
                                let _ = threadlane_ui_state::project_io::unwatch_project(
                                    &client, &previous,
                                )
                                .await;
                            });
                        }
                    }
                    if let Ok(executor) = threadlane_ui_state::chat::executor() {
                        let proj = work_dir.clone();
                        let client = daemon_client.clone();
                        executor.spawn(async move {
                            let _ =
                                threadlane_ui_state::project_io::watch_project(&client, &proj)
                                    .await;
                        });
                    }
                    self.watched_project = Some(work_dir);
                }
            } else {
                // Pre-3 remote daemon: the only reachable filesystem is
                // this client's, so the local watcher stays.
                self.watched_project = None;
                let tx = self.event_tx.clone();
                let proj = work_dir.clone();
                self._watcher = WorkspaceWatcher::start(
                    work_dir,
                    Duration::from_millis(200),
                    move |change| {
                        let _ = tx.send(PanelEvent::WorkspaceChanged {
                            project: proj.clone(),
                            git_dirty: change.git_dirty,
                            files_dirty: change.files_dirty,
                        });
                    },
                )
                .ok();
            }
        } else {
            self._watcher = None;
            if let Some(previous) = self.watched_project.take() {
                if let Ok(executor) = threadlane_ui_state::chat::executor() {
                    let client = daemon_client.clone();
                    executor.spawn(async move {
                        let _ = threadlane_ui_state::project_io::unwatch_project(
                            &client, &previous,
                        )
                        .await;
                    });
                }
            }
        }

        self.refresh_active_surface(cx);
    }

    pub fn open_review(&mut self, cx: &mut Context<Self>) {
        self.open_surface(Surface::Review, cx);
    }

    pub fn open_commit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.open_review(cx);
        self.close_document(cx);
        self.review_tab = ReviewTab::Changes;
        self.commit_message_input
            .update(cx, |input, cx| input.focus(window, cx));
        cx.notify();
    }

    pub fn open_branch_popover(&mut self, cx: &mut Context<Self>) {
        self.open_surface(Surface::Review, cx);
        self.branch_popover_open = true;
        cx.notify();
    }

    pub fn open_new_branch_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.open_surface(Surface::Review, cx);
        self.new_branch_dialog_open = true;
        self.branch_popover_open = false;
        // Synara ThreadWorktreeHandoffDialog pattern: autofocus the name
        // input and select existing text so typing replaces it.
        self.new_branch_name_input.update(cx, |input, cx| {
            let len = input.value().len();
            input.set_selected_range(0..len, cx);
            input.focus(window, cx);
        });
        cx.notify();
    }

    pub fn open_merge_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.open_surface(Surface::Review, cx);
        self.merge_dialog_open = true;
        self.merge_selected_branch = None;
        self.branch_popover_open = false;
        self.merge_filter_input.update(cx, |input, cx| {
            input.focus(window, cx);
        });
        cx.notify();
    }

    fn replace_git_status(&mut self, status: Option<GitStatus>, cx: &mut Context<Self>) {
        let previous_branch = self
            .git_status
            .as_ref()
            .and_then(|status| status.branch.as_deref());
        let next_branch = status.as_ref().and_then(|status| status.branch.as_deref());
        if previous_branch != next_branch {
            self.draft_pr_context_revision = self.draft_pr_context_revision.wrapping_add(1);
            if self.git_status.is_some() && status.is_some() {
                self.review_diff_options = threadlane_git::DiffOptions::default();
                if self.review_diff_request.is_some() {
                    self.close_document(cx);
                }
            }
        }
        self.git_status = status;
    }

    pub(crate) fn draft_pr_checkout_key(&self) -> Option<DraftPrContextKey> {
        let project = self.project.clone()?;
        let status = self.git_status.as_ref()?;
        if self.worktree_unavailable || status.detached {
            return None;
        }
        let branch = status
            .branch
            .as_deref()
            .filter(|branch| !branch.trim().is_empty())?
            .to_string();
        Some(DraftPrContextKey {
            project,
            branch,
            revision: self.draft_pr_context_revision,
        })
    }

    pub(crate) fn draft_pr_creation_key(&self) -> Option<DraftPrContextKey> {
        can_create_pull_request(!self.worktree_unavailable, self.git_status.as_ref())
            .then(|| self.draft_pr_checkout_key())
            .flatten()
    }

    pub fn open_draft_pr_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.sync_project(cx);
        if self.git_busy {
            return;
        }
        // The hidden panel may still hold status for an earlier branch in this checkout.
        let status = self
            .project
            .as_ref()
            .and_then(|project| self.model.read(cx).git_statuses.get(project).cloned());
        self.replace_git_status(status, cx);
        let Some(key) = self.draft_pr_creation_key() else {
            let message = "Publish this named branch and refresh pull request status before creating a draft.";
            self.git_feedback = Some(message.into());
            window.push_notification(Notification::warning(message), cx);
            cx.notify();
            return;
        };
        let Some(status) = self.git_status.as_ref() else {
            return;
        };
        let fields = draft_pr_prefill(status);
        let panel = cx.entity().downgrade();
        let dialog_state =
            cx.new(|dialog_cx| DraftPrDialogView::new(panel, key, fields, window, dialog_cx));
        let content = dialog_state.clone();
        window.open_dialog(cx, move |dialog, _window, _cx| {
            let submit_state = content.clone();
            let cancel_state = content.clone();
            dialog
                .title("Create draft pull request")
                .child(content.clone())
                .close_button(false)
                .on_ok(move |_, window, cx| {
                    submit_state.update(cx, |state, cx| state.start_request(false, window, cx));
                    false
                })
                .on_cancel(move |_, _, cx| {
                    let state = cancel_state.read(cx);
                    !state.attempts.is_busy()
                        || state.current_key(false, cx).as_ref() != Some(&state.key)
                })
        });
        let base_input = dialog_state.read(cx).base_input.clone();
        base_input.update(cx, |input, cx| input.focus(window, cx));
    }

    /// Mounts the workspace-owned trajectory surface. `AnyView` keeps this
    /// crate independent of threadlane-ui-chat while letting the same live
    /// view render inside the right panel.
    pub fn set_trajectory_view(&mut self, view: AnyView) {
        self.trajectory_view = Some(view);
    }

    pub fn open_surface(&mut self, surface: Surface, cx: &mut Context<Self>) {
        self.sync_project(cx);
        if self.active_surface != Some(surface) {
            self.close_document(cx);
        }
        self.active_surface = Some(surface);
        self.refresh_surface(surface, cx);
        self.sync_browser_visibility(cx);
        cx.notify();
    }

    fn refresh_active_surface(&mut self, cx: &mut Context<Self>) {
        if let Some(surface) = self.active_surface {
            self.refresh_surface(surface, cx);
        }
    }

    pub(crate) fn refresh_surface(&mut self, surface: Surface, cx: &mut Context<Self>) {
        if surface == Surface::Review {
            self.invalidate_review_diff(cx);
        }
        let Some(project) = self.project.clone() else {
            return;
        };
        let tx = self.event_tx.clone();
        let daemon_client = self.model.read(cx).daemon_client.clone();
        let Ok(executor) = threadlane_ui_state::chat::executor() else {
            if surface == Surface::Review {
                let _ = tx.send(PanelEvent::ReviewLoaded {
                    project,
                    status: None,
                    files: Vec::new(),
                    error: Some("The git runtime is unavailable.".into()),
                });
            }
            return;
        };
        executor.spawn(async move {
            match surface {
                Surface::Agents => {}
                Surface::Trajectory => {
                    // Renders live off AppState; nothing to fetch.
                }
                Surface::Files => {
                    let nodes = threadlane_ui_state::project_io::project_files(
                        &daemon_client,
                        &project,
                        500,
                    )
                    .await
                    .unwrap_or_default();
                    let _ = tx.send(PanelEvent::FilesLoaded { project, nodes });
                }
                Surface::Review => {
                    // Keep ahead/behind and PR checks current when the user
                    // refreshes Review; the daemon tolerates fetch failures so
                    // local status remains available offline.
                    let (status, files, error) = match threadlane_ui_state::project_io::inspect(
                        &daemon_client,
                        &project,
                        true,
                    )
                    .await
                    {
                        Ok(status) => {
                            let files = status.files.clone();
                            (Some(status), files, None)
                        }
                        Err(error) => (None, Vec::new(), Some(error)),
                    };
                    let _ = tx.send(PanelEvent::ReviewLoaded {
                        project,
                        status,
                        files,
                        error,
                    });
                }
                Surface::Browser => {
                    // The live webview needs no background refresh.
                }
            }
        });
    }

    fn open_file_diff(&mut self, path: String, cx: &mut Context<Self>) {
        self.open_review_diff(ReviewDiffTarget::File(path), cx);
    }

    fn open_combined_diff(&mut self, cx: &mut Context<Self>) {
        self.open_review_diff(ReviewDiffTarget::AllChanges, cx);
    }

    fn set_ignore_whitespace(&mut self, checked: bool, cx: &mut Context<Self>) {
        self.review_diff_options.ignore_whitespace = checked;
        self.reload_review_diff(cx);
    }

    fn reload_review_diff(&mut self, cx: &mut Context<Self>) {
        if self.git_checkout_pending {
            return;
        }
        if let Some(request) = &self.review_diff_request {
            self.open_review_diff(request.target.clone(), cx);
        }
    }

    fn invalidate_review_diff(&mut self, cx: &mut Context<Self>) {
        self.review_diff_revision = self.review_diff_revision.wrapping_add(1);
        if let Some(request) = &mut self.review_diff_request {
            request.revision = self.review_diff_revision;
            self.pending_document = None;
            self.review_diff_state = Some(ReviewDiffState::Loading);
            self.document_state
                .update(cx, |state, cx| state.set_text("", cx));
            cx.notify();
        }
    }

    fn open_review_diff(&mut self, target: ReviewDiffTarget, cx: &mut Context<Self>) {
        let Some(project) = self.project.clone() else {
            return;
        };
        self.review_diff_revision = self.review_diff_revision.wrapping_add(1);
        let request = ReviewDiffRequest {
            project,
            target,
            options: self.review_diff_options,
            revision: self.review_diff_revision,
        };
        self.pending_document = None;
        self.document_title = Some(request.target.title());
        self.review_diff_request = Some(request.clone());
        self.review_diff_state = Some(ReviewDiffState::Loading);
        self.editor_state = None;
        self.editor_subscription = None;
        self.saved_content.clear();
        self.is_dirty = false;
        self.document_state
            .update(cx, |state, cx| state.set_text("", cx));
        if self.git_checkout_pending {
            cx.notify();
            return;
        }
        #[cfg(test)]
        {
            self.review_diff_load_count += 1;
        }
        let daemon_client = self.model.read(cx).daemon_client.clone();
        cx.spawn(async move |this, cx| {
            let background_request = request.clone();
            let result = cx
                .background_executor()
                .spawn(async move {
                    match &background_request.target {
                        ReviewDiffTarget::File(path) => {
                            threadlane_ui_state::project_io::diff_file(
                                &daemon_client,
                                &background_request.project,
                                path.clone(),
                                background_request.options,
                            )
                            .await
                        }
                        ReviewDiffTarget::AllChanges => {
                            threadlane_ui_state::project_io::diff_worktree(
                                &daemon_client,
                                &background_request.project,
                                background_request.options,
                            )
                            .await
                        }
                    }
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                this.apply_review_diff_result(request, result, cx);
            });
        })
        .detach();
        cx.notify();
    }

    fn close_document(&mut self, cx: &mut Context<Self>) {
        self.review_diff_revision = self.review_diff_revision.wrapping_add(1);
        self.review_diff_request = None;
        self.review_diff_state = None;
        self.document_title = None;
        self.editor_state = None;
        self.editor_subscription = None;
        self.saved_content.clear();
        self.is_dirty = false;
        self.pending_document = None;
        self.document_state
            .update(cx, |state, cx| state.set_text("", cx));
        cx.notify();
    }

    fn apply_review_diff_result(
        &mut self,
        request: ReviewDiffRequest,
        result: Result<String, String>,
        cx: &mut Context<Self>,
    ) {
        if self.git_checkout_pending
            || self.review_diff_request.as_ref() != Some(&request)
            || self.project.as_ref() != Some(&request.project)
            || self.model.read(cx).active_git_work_dir().as_ref() != Some(&request.project)
        {
            return;
        }
        match result {
            Ok(content) => {
                self.review_diff_state = Some(ReviewDiffState::Ready {
                    empty: content.is_empty(),
                });
                let markdown = format!("```diff\n{}\n```", content.replace("```", "` ` `"));
                self.document_state
                    .update(cx, |state, cx| state.set_text(&markdown, cx));
            }
            Err(error) => {
                self.review_diff_state = Some(ReviewDiffState::Failed(error));
            }
        }
        cx.notify();
    }

    fn sync_pending_document(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some((title, content)) = self.pending_document.take() else {
            return;
        };
        self.close_document(cx);
        self.document_title = Some(title.clone());
        self.saved_content = content.clone();
        self.is_dirty = false;

        if title.starts_with("Review ·") {
            self.editor_state = None;
            self.editor_subscription = None;
            let markdown = format!("```diff\n{}\n```", content.replace("```", "` ` `"));
            self.document_state
                .update(cx, |state, cx| state.set_text(&markdown, cx));
        } else {
            let lang = detect_language(&title);
            let editor = cx.new(|cx| {
                EditorState::new(window, cx)
                    .language(lang)
                    .line_number(true)
                    .folding(true)
                    .show_whitespaces(false)
                    .tab_size(TabSize {
                        tab_size: 4,
                        hard_tabs: false,
                    })
                    .default_value(&content)
            });
            let subscription = cx.subscribe(&editor, |this, editor, event: &InputEvent, cx| {
                if matches!(event, InputEvent::Change) {
                    let current = editor.read(cx).value();
                    let dirty = current.as_str() != this.saved_content.as_str();
                    if this.is_dirty != dirty {
                        this.is_dirty = dirty;
                        cx.notify();
                    }
                }
            });
            self.editor_state = Some(editor);
            self.editor_subscription = Some(subscription);
        }
    }

    fn save_active_document(&mut self, cx: &mut Context<Self>) {
        let Some(editor) = self.editor_state.as_ref() else {
            return;
        };
        let Some(title) = self.document_title.as_ref() else {
            return;
        };
        let Some(project) = self.project.as_ref() else {
            return;
        };
        // `title` is the project-relative document path.
        let path = title.clone();
        let content = editor.read(cx).value().to_string();
        let daemon_client = self.model.read(cx).daemon_client.clone();
        let work_dir = project.clone();
        cx.spawn(async move |this, cx| {
            let write_content = content.clone();
            let result = cx
                .background_executor()
                .spawn(async move {
                    threadlane_ui_state::project_io::write_file(
                        &daemon_client,
                        &work_dir,
                        path,
                        write_content,
                    )
                    .await
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                match result {
                    Ok(()) => {
                        this.saved_content = content;
                        this.is_dirty = false;
                    }
                    Err(error) => {
                        this.git_feedback = Some(format!("Save failed: {error}"));
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn apply_event(&mut self, event: PanelEvent, cx: &mut Context<Self>) {
        match event {
            PanelEvent::WorkspaceChanged {
                project,
                git_dirty,
                files_dirty,
            } if self.project.as_ref() == Some(&project) => {
                if git_dirty {
                    self.refresh_surface(Surface::Review, cx);
                }
                if files_dirty {
                    self.refresh_surface(Surface::Files, cx);
                }
            }
            PanelEvent::FilesLoaded { project, nodes }
                if self.project.as_ref() == Some(&project) =>
            {
                let expanded_paths = &self.expanded_paths;
                let items = nodes
                    .into_iter()
                    .map(|node| convert_node_to_tree_item(node, expanded_paths))
                    .collect::<Vec<_>>();
                self.tree_state
                    .update(cx, |state, cx| state.set_items(items, cx));
            }
            PanelEvent::ReviewLoaded {
                project,
                status,
                files,
                error,
            } if self.project.as_ref() == Some(&project) => {
                if let Some(status_ref) = &status {
                    self.model.update(cx, |state, cx| {
                        state
                            .git_statuses
                            .insert(project.clone(), status_ref.clone());
                        cx.notify();
                    });
                }
                self.replace_git_status(status, cx);
                let current_set: HashSet<String> = files.iter().map(|f| f.path.clone()).collect();
                retain_review_selection(
                    &mut self.selected_files,
                    current_set,
                    &mut self.review_selection_initialized,
                );
                self.review_files = files;
                self.review_error = error;
                self.reload_review_diff(cx);
                self.stash_files = None;
                self.loading_stash_index = None;
            }
            PanelEvent::MessageGenerated {
                project,
                result,
                diff_truncated,
            } if message_generated_matches_active_project(
                &project,
                self.model.read(cx).active_git_work_dir().as_deref(),
            ) =>
            {
                self.git_message_pending = false;
                match result {
                    Ok(message) => {
                        self.generated_commit_message = Some(message);
                        // Synara DiffTruncationWarning pattern: never present a
                        // truncated diff as complete.
                        self.git_feedback = diff_truncated.then(|| {
                            "Partial diff — message generated from the first 24,000 characters."
                                .into()
                        });
                        self.pending_git_notifications
                            .push(Notification::success("Commit message generated."));
                    }
                    Err(error) => {
                        self.git_feedback = Some(error.clone());
                        self.pending_git_notifications
                            .push(Notification::error(error));
                    }
                }
            }
            PanelEvent::ActionFinished {
                project,
                status,
                action_error,
                action_message,
                checkout_succeeded,
            } => {
                self.git_checkout_pending = false;
                if self.project.as_ref() != Some(&project) {
                    // The action is complete, but its project is no longer active.
                    // Release the guard without applying stale status to the new project.
                    self.git_busy = false;
                    return;
                }
                self.git_busy = false;
                if checkout_succeeded {
                    self.review_diff_options = threadlane_git::DiffOptions::default();
                    if self.review_diff_request.is_some() {
                        self.close_document(cx);
                    }
                }
                match status {
                    Ok(status) => {
                        self.model.update(cx, |state, cx| {
                            state.git_statuses.insert(project, status.clone());
                            cx.notify();
                        });
                        self.replace_git_status(Some(status.clone()), cx);
                        self.reload_review_diff(cx);
                        self.stash_files = None;
                        self.loading_stash_index = None;
                        self.selected_files = status.files.iter().map(|f| f.path.clone()).collect();
                        self.review_files = status.files;
                        self.review_error = None;
                        self.should_clear_commit_message = true;
                        self.branch_popover_open = false;
                        self.new_branch_dialog_open = false;
                        self.merge_dialog_open = false;
                        self.switch_dialog_open = false;
                        self.switch_target_branch = None;
                        self.last_fetched_time = Some(std::time::Instant::now());
                        let action_failed = action_error.is_some();
                        let message = action_error
                            .or(action_message)
                            .unwrap_or_else(|| "Git action completed successfully.".into());
                        self.git_feedback = Some(message.clone());
                        self.pending_git_notifications.push(if action_failed {
                            Notification::error(message)
                        } else {
                            Notification::success(message)
                        });
                    }
                    Err(status_error) => {
                        self.review_error = Some(status_error.clone());
                        self.reload_review_diff(cx);
                        let message = action_error.unwrap_or(status_error);
                        self.git_feedback = Some(message.clone());
                        self.pending_git_notifications
                            .push(Notification::error(message));
                    }
                }
            }
            PanelEvent::CommitFilesLoaded { sha, files } => {
                if self.loading_commit_sha.as_deref() == Some(&sha) {
                    self.loading_commit_sha = None;
                    self.selected_commit_sha = Some(sha);
                    self.selected_commit_files = files;
                }
            }
            PanelEvent::StashFilesLoaded {
                project,
                index,
                files,
            } if self.project.as_ref() == Some(&project) => {
                if self.loading_stash_index == Some(index) {
                    self.loading_stash_index = None;
                    self.stash_files = Some((index, files));
                }
            }
            _ => {}
        }
    }

    pub fn restore_current_stash(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(index) = self
            .git_status
            .as_ref()
            .and_then(|status| status.current_stash.as_ref())
            .map(|stash| stash.index)
        else {
            self.git_feedback = Some("No stash found for the current branch.".into());
            cx.notify();
            return;
        };
        self.run_git_action(GitAction::PopStash(Some(index)), window, cx);
    }

    fn generate_commit_message(&mut self, cx: &mut Context<Self>) {
        let Some(work_dir) = self.project.clone() else {
            self.git_feedback = Some("Attach a project to generate a commit message.".into());
            cx.notify();
            return;
        };
        let selected_paths: Vec<String> = self.selected_files.iter().cloned().collect();
        if selected_paths.is_empty() {
            self.git_feedback =
                Some("Select at least one file to generate a commit message.".into());
            cx.notify();
            return;
        }
        let total_count = self.review_files.len();
        let model = self.model.read(cx).selected_model.clone();
        if model.is_empty() {
            self.git_feedback =
                Some("Select a model in chat before generating a commit message.".into());
            cx.notify();
            return;
        }
        let (api_key, account_id) =
            threadlane_coding_agent::credentials::provider_credentials(&model);
        let tx = self.event_tx.clone();
        let Ok(executor) = threadlane_ui_state::chat::executor() else {
            self.git_feedback = Some("Unable to start the model runtime.".into());
            cx.notify();
            return;
        };

        self.git_message_pending = true;
        self.git_feedback = Some("Generating a commit message…".into());
        let daemon_client = self.model.read(cx).daemon_client.clone();
        executor.spawn(async move {
            let (result, diff_truncated) = async {
                let diff = if selected_paths.len() == total_count {
                    match threadlane_ui_state::project_io::commit_message_diff(
                        &daemon_client,
                        &work_dir,
                    )
                    .await
                    {
                        Ok(diff) => diff,
                        Err(error) => return (Err(error), false),
                    }
                } else {
                    match threadlane_ui_state::project_io::diff_files(
                        &daemon_client,
                        &work_dir,
                        selected_paths.clone(),
                        threadlane_git::DiffOptions::default(),
                    )
                    .await
                    {
                        Ok(diff) => diff,
                        Err(error) => return (Err(error), false),
                    }
                };
                let diff_truncated = diff.chars().count() > 24_000;
                let diff = if diff_truncated {
                    format!(
                        "{}\n\n[Diff truncated for message generation]",
                        diff.chars().take(24_000).collect::<String>()
                    )
                } else {
                    diff
                };
                let generated = if let Some(agent_id) = threadlane_acp_engine::acp_agent_id(&model)
                {
                    threadlane_acp_engine::generate_commit_message(
                        threadlane_project::default_global_threadlane_dir(),
                        work_dir.clone(),
                        agent_id,
                        &diff,
                    )
                    .await
                } else {
                    threadlane_coding_agent::credentials::provider_client_for(api_key, account_id)
                        .generate_commit_message(&model, &diff)
                        .await
                };
                let raw = match generated {
                    Ok(raw) => raw,
                    Err(error) => return (Err(error.to_string()), diff_truncated),
                };
                let message = normalize_generated_commit_message(&raw);
                if message.is_empty() {
                    (
                        Err("The model returned an empty commit message.".to_string()),
                        diff_truncated,
                    )
                } else {
                    (Ok(message), diff_truncated)
                }
            }
            .await;
            let _ = tx.send(PanelEvent::MessageGenerated {
                project: work_dir,
                result,
                diff_truncated,
            });
        });
        cx.notify();
    }

    pub fn run_git_action(
        &mut self,
        action: GitAction,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.execute_git_action(action, Some(window), cx);
    }

    pub(crate) fn run_git_action_without_window(
        &mut self,
        action: GitAction,
        cx: &mut Context<Self>,
    ) {
        self.execute_git_action(action, None, cx);
    }

    fn execute_git_action(
        &mut self,
        action: GitAction,
        mut window: Option<&mut Window>,
        cx: &mut Context<Self>,
    ) {
        let Some(work_dir) = self.project.clone() else {
            self.git_feedback = Some("Attach a project to use Git actions.".into());
            let notif = Notification::warning("Attach a project to use Git actions");
            if let Some(ref mut window) = window {
                window.push_notification(notif, cx);
            } else {
                self.pending_git_notifications.push(notif);
            }
            cx.notify();
            return;
        };
        if self.git_busy {
            return;
        }
        let message = self
            .commit_message_input
            .read(cx)
            .value()
            .trim()
            .to_string();
        let selected_paths: Vec<String> = self.selected_files.iter().cloned().collect();

        if matches!(action, GitAction::Commit | GitAction::CommitAndPush) {
            if selected_paths.is_empty() {
                self.git_feedback = Some("Select at least one file to commit.".into());
                let notif = Notification::warning("Select at least one file to commit");
                if let Some(ref mut window) = window {
                    window.push_notification(notif, cx);
                } else {
                    self.pending_git_notifications.push(notif);
                }
                cx.notify();
                return;
            }
        }
        if matches!(action, GitAction::Commit | GitAction::CommitAndPush) && message.is_empty()
        {
            self.git_feedback = Some("Enter a commit message first.".into());
            let notif = Notification::warning("Enter a commit message first");
            if let Some(ref mut window) = window {
                window.push_notification(notif, cx);
            } else {
                self.pending_git_notifications.push(notif);
            }
            cx.notify();
            return;
        }

        let checkout = matches!(
            action,
            GitAction::Checkout(_)
                | GitAction::CheckoutStash(_)
                | GitAction::CheckoutCarry(_)
                | GitAction::CreateBranch(_)
        );
        self.git_checkout_pending = checkout;
        if checkout {
            self.invalidate_review_diff(cx);
        }
        self.git_busy = true;
        let feedback = match &action {
            GitAction::Commit => "Committing…".to_string(),
            GitAction::CommitAndPush => "Committing and pushing…".to_string(),
            GitAction::StageFile(p) => format!("Staging {p}…"),
            GitAction::UnstageFile(p) => format!("Unstaging {p}…"),
            GitAction::StageFiles(paths) => format!("Staging {} files…", paths.len()),
            GitAction::UnstageFiles(paths) => format!("Unstaging {} files…", paths.len()),
            GitAction::StashPush { .. } => "Stashing changes…".to_string(),
            GitAction::Push => "Pushing…".to_string(),
            GitAction::Pull => "Pulling from origin…".to_string(),
            GitAction::Fetch => "Fetching origin…".to_string(),
            GitAction::StageAll => "Staging all changes…".to_string(),
            GitAction::UnstageAll => "Unstaging all changes…".to_string(),
            GitAction::CreatePullRequest => "Creating pull request…".to_string(),
            GitAction::Checkout(b) => format!("Switching to {b}…"),
            GitAction::CheckoutStash(b) => format!("Stashing changes & switching to {b}…"),
            GitAction::CheckoutCarry(b) => format!("Switching to {b} with changes…"),
            GitAction::CreateBranch(b) => format!("Creating branch {b}…"),
            GitAction::DeleteBranch(b) => format!("Deleting branch {b}…"),
            GitAction::Merge(b) => format!("Merging {b}…"),
            GitAction::PopStash(_) => "Restoring stashed changes…".to_string(),
            GitAction::DropStash(_) => "Discarding stash…".to_string(),
            GitAction::DiscardFile(p) => format!("Discarding changes in {p}…"),
            GitAction::DiscardFiles(paths) => {
                if paths.len() == 1 {
                    format!("Discarding changes in {}…", paths[0])
                } else {
                    format!("Discarding changes in {} files…", paths.len())
                }
            }
            GitAction::DiscardAll => "Discarding all changes…".to_string(),
            GitAction::IgnoreFile(p) => format!("Adding {p} to .gitignore…"),
            GitAction::IgnoreExtension(ext) => format!("Ignoring *.{ext} files…"),
        };
        self.git_feedback = Some(feedback);
        let tx = self.event_tx.clone();
        let daemon_client = self.model.read(cx).daemon_client.clone();
        let operation = git_action_to_operation(&action, message, selected_paths);
        let Ok(executor) = threadlane_ui_state::chat::executor() else {
            self.git_busy = false;
            self.git_checkout_pending = false;
            self.git_feedback = Some("The git runtime is unavailable.".into());
            cx.notify();
            return;
        };
        executor.spawn(async move {
            let outcome =
                threadlane_ui_state::project_io::run_action(&daemon_client, &work_dir, operation)
                    .await;
            let (action_error, action_message, status) = match outcome {
                Ok(outcome) => (outcome.action_error, outcome.message, outcome.status),
                Err(error) => (Some(error.clone()), None, Err(error)),
            };
            let _ = tx.send(PanelEvent::ActionFinished {
                project: work_dir,
                status,
                checkout_succeeded: checkout && action_error.is_none(),
                action_error,
                action_message,
            });
        });
        cx.notify();
    }

    /// Returns the live browser view, creating it on first use.
    fn ensure_browser(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Entity<BrowserView> {
        if let Some(browser) = &self.browser {
            return browser.clone();
        }
        let browser_model = self.model.clone();
        let browser = cx.new(|cx| BrowserView::new(browser_model, window, cx));
        self.browser = Some(browser.clone());
        browser
    }

    /// Human terminal navigation uses a new tab, never the address/search
    /// resolver. Opens in the embedded browser where supported, else errors
    /// so the caller can fall back to the system browser.
    pub fn open_terminal_url(
        &mut self,
        url: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Result<(), String> {
        if self.is_dirty {
            return Err("Save or discard the editor's changes, then retry Open link…".into());
        }
        #[cfg(any(target_os = "macos", target_os = "linux", target_os = "windows"))]
        {
            let browser = self.ensure_browser(window, cx);
            browser.update(cx, |browser, cx| browser.try_open_tab(url, window, cx))?;
            self.visible = true;
            self.open_surface(Surface::Browser, cx);
            browser.update(cx, |browser, cx| browser.focus_address(window, cx));
            Ok(())
        }
        #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
        {
            let _ = (url, window, cx);
            Err(
                "Threadlane browser is not supported on this platform. Choose Open in default browser."
                    .into(),
            )
        }
    }

    /// Native browser views must be hidden explicitly when the panel leaves the layout.
    pub fn set_visible(&mut self, visible: bool, cx: &mut Context<Self>) {
        self.visible = visible;
        self.sync_browser_visibility(cx);
    }

    /// Shows the browser's webviews only while the Browser surface is
    /// active and the panel is open; hides them otherwise.
    fn sync_browser_visibility(&mut self, cx: &mut Context<Self>) {
        let Some(browser) = self.browser.clone() else {
            return;
        };
        let visible = self.visible && self.active_surface == Some(Surface::Browser);
        browser.update(cx, |browser, cx| browser.set_visible(visible, cx));
    }

    /// Start a script evaluation against the live view, returning the
    /// pending result channel. The caller awaits it off the UI thread.
    fn start_browser_eval(
        &mut self,
        script: &str,
        cx: &mut Context<Self>,
    ) -> Result<tokio::sync::oneshot::Receiver<String>, String> {
        let Some(browser) = self.browser.clone() else {
            return Err("The browser panel is not ready.".to_string());
        };
        browser.update(cx, |browser, cx| browser.evaluate_script(script, cx))
    }

    /// Writes a browser snapshot into `.threadlane/previews/` and returns
    /// the saved path (if any) plus a base64 data URL for the reply.
    fn save_browser_screenshot(&self, bytes: &[u8]) -> (Option<String>, String) {
        let data_url = base64_data_url(bytes);
        let Some(project) = &self.project else {
            return (None, data_url);
        };
        let dir = project.join(".threadlane").join("previews");
        if std::fs::create_dir_all(&dir).is_err() {
            return (None, data_url);
        }
        let ext = snapshot_file_ext(bytes);
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0);
        let path = dir.join(format!("browser-{stamp}.{ext}"));
        let _ = std::fs::write(&path, bytes);
        let _ = std::fs::write(dir.join(format!("latest-browser.{ext}")), bytes);
        (Some(path.display().to_string()), data_url)
    }

    /// Apply one agent `BrowserCommand` on the UI thread; called from the
    /// bridge pump, never from a tool worker directly. Unsupported
    /// platforms return a user-facing error.
    fn apply_browser_command(
        &mut self,
        command: threadlane_protocol::browser::BrowserCommand,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Result<String, String> {
        #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
        {
            let _ = (command, window, cx);
            return Err("The embedded browser is not supported on this platform.".to_string());
        }
        #[cfg(any(target_os = "macos", target_os = "linux", target_os = "windows"))]
        {
            use super::browser::{AddressTarget, resolve_address, search_url};
            use threadlane_protocol::browser::BrowserCommand;
            let Some(browser) = self.browser.clone() else {
                return Err("The browser panel is not ready.".to_string());
            };
            match command {
                BrowserCommand::Tabs { action } => {
                    use threadlane_protocol::browser::BrowserTabAction;
                    let reveal = !matches!(action, BrowserTabAction::List);
                    browser.update(cx, |browser, cx| -> Result<(), String> {
                        match action {
                            BrowserTabAction::List => {}
                            BrowserTabAction::Open { url } => {
                                let url = match resolve_address(&url) {
                                    Some(AddressTarget::Url(url)) => url,
                                    Some(AddressTarget::Search(query)) => search_url(&query),
                                    None => return Err("`browser_tabs` open requires a non-empty `url`.".into()),
                                };
                                browser.open_tab(&url, window, cx);
                            }
                            BrowserTabAction::Select { tab_id } | BrowserTabAction::Close { tab_id } => {
                                if !browser.tabs(cx).iter().any(|(id, _)| *id == tab_id) {
                                    return Err(format!("Browser tab {tab_id} does not exist. Use browser_tabs list to get current IDs."));
                                }
                                if matches!(action, BrowserTabAction::Select { .. }) {
                                    browser.switch_tab(tab_id, window, cx);
                                } else {
                                    browser.close_tab(tab_id, window, cx);
                                }
                            }
                        }
                        Ok(())
                    })?;
                    if reveal {
                        self.open_surface(Surface::Browser, cx);
                    }
                    let browser = browser.read(cx);
                    let tabs: Vec<_> = browser
                        .tabs(cx)
                        .into_iter()
                        .map(|(id, url)| serde_json::json!({"tab_id": id, "url": url}))
                        .collect();
                    Ok(serde_json::json!({
                        "active_tab_id": browser.active_tab_id(),
                        "tabs": tabs,
                    }).to_string())
                }
                BrowserCommand::Navigate { url } => {
                    let final_url = match resolve_address(&url) {
                        None => {
                            return Err(
                                "`browser_navigate` requires a non-empty `url`.".to_string()
                            );
                        }
                        Some(AddressTarget::Url(url)) => url,
                        Some(AddressTarget::Search(query)) => search_url(&query),
                    };
                    browser.update(cx, |browser, cx| browser.load_url(&final_url, cx));
                    // Keep the agent's browsing visible to the user.
                    self.open_surface(Surface::Browser, cx);
                    Ok(format!("Opened {final_url} in the browser panel."))
                }
                BrowserCommand::Back => {
                    browser.update(cx, |browser, cx| browser.go_back(cx));
                    self.open_surface(Surface::Browser, cx);
                    Ok("Went back in the browser panel.".to_string())
                }
                BrowserCommand::Reload => {
                    browser.update(cx, |browser, cx| browser.reload(cx));
                    Ok("Reloaded the browser panel.".to_string())
                }
                BrowserCommand::CurrentUrl => {
                    let url = browser.read(cx).current_url(cx).unwrap_or_default();
                    Ok(if url.is_empty() {
                        "The browser panel has no page open yet.".to_string()
                    } else {
                        url
                    })
                }
                // Script-backed commands route through start_browser_eval;
                // reaching here is a pump bug, not a page problem.
                _ => Err("Internal browser routing error.".to_string()),
            }
        }
    }

    /// Renders the browser surface, or the platform-stub message where the
    /// embedded browser is unavailable.
    fn render_browser(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let browser = self.ensure_browser(window, cx);
        self.sync_browser_visibility(cx);
        div().flex_1().min_h_0().child(browser).into_any_element()
    }

    fn render_workspace_context(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().colors;
        // `self.project` is the session's git checkout: for worktree sessions
        // that is a `session_<id>` directory, which is meaningless to show.
        // Name the attached project and flag the worktree instead.
        let (repository, is_worktree_session) = {
            let state = self.model.read(cx);
            let project_name = state
                .active_work_dir
                .as_ref()
                .and_then(|path| path.file_name())
                .and_then(|name| name.to_str())
                .map(str::to_owned);
            let is_worktree_session = match (&state.active_work_dir, &self.project) {
                (Some(root), Some(git_dir)) => root != git_dir,
                _ => false,
            };
            (project_name, is_worktree_session)
        };
        let repository = repository
            .or_else(|| {
                self.project
                    .as_ref()
                    .and_then(|path| path.file_name())
                    .and_then(|name| name.to_str())
                    .map(str::to_owned)
            })
            .unwrap_or_else(|| "No repository".to_owned());
        let branch = self
            .git_status
            .as_ref()
            .and_then(|status| status.branch.as_deref())
            .unwrap_or("no branch")
            .to_owned();
        let git_state = match self.git_status.as_ref() {
            Some(status) if status.has_changes => format!("{} files changed", status.files.len()),
            Some(_) => "No changes".into(),
            None => "Git status unavailable".into(),
        };
        let has_changes = self
            .git_status
            .as_ref()
            .map(|status| status.has_changes)
            .unwrap_or(false);
        let file_context = self
            .document_title
            .clone()
            .unwrap_or_else(|| "No active file".to_owned());
        div()
            .flex_none()
            .px_3()
            .py_1p5()
            .bg(theme.list_head)
            .text_xs()
            .child(
                div()
                    .flex()
                    .items_center()
                    .flex_wrap()
                    .gap_2()
                    .child(div().font_weight(FontWeight::MEDIUM).child(repository))
                    .children(is_worktree_session.then(|| {
                        Tag::secondary().child("worktree").xsmall()
                    }))
                    .child(
                        div()
                            .min_w_0()
                            .truncate()
                            .text_color(theme.muted_foreground)
                            .child(format!("· {branch}")),
                    )
                    // Synara EnvironmentPanel "Changes" row pattern: expose an
                    // explicit labeled Review entry when reliable change data
                    // exists. Scope is workspace changes, not per-turn diffs.
                    .when(has_changes, |this| {
                        this.child(
                            Button::new("open-review-from-context")
                                .label(git_state.clone())
                                .ghost()
                                .xsmall()
                                .accessibility_label(format!("Open Review, {}", git_state))
                                .tooltip("Open Review (workspace changes)")
                                .on_click(cx.listener(|this, _event, _window, cx| {
                                    this.open_surface(Surface::Review, cx);
                                })),
                        )
                    })
                    .when(!has_changes, |this| {
                        this.child(
                            div()
                                .text_color(theme.muted_foreground)
                                .child(git_state.clone()),
                        )
                    }),
            )
            .child(
                div()
                    .mt_0p5()
                    .text_color(theme.muted_foreground)
                    .truncate()
                    .child(format!(
                        "{} · {}",
                        if self.worktree_unavailable {
                            "worktree unavailable"
                        } else {
                            "active worktree"
                        },
                        file_context
                    )),
            )
    }

    fn render_header(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().colors;
        let surfaces = Surface::all();
        let selected_surface = self
            .active_surface
            .and_then(|active| surfaces.iter().position(|surface| *surface == active));
        let selected_index = selected_surface.unwrap_or(0);
        div()
            .flex_none()
            // Keep the top 3rem clear for the workspace's floating overlay
            // buttons. Surface controls occupy a separate small row below it.
            .flex()
            .flex_col()
            .border_b_1()
            .border_color(theme.title_bar_border)
            .bg(theme.title_bar)
            .child(div().h(rems(3.0)).flex_none())
            .child(
                div()
                    .flex_none()
                    .min_h(rems(2.0))
                    .flex()
                    .items_center()
                    .px_3()
                    .child(
                    div()
                        .flex()
                        .items_center()
                        .gap_1()
                        .w_full()
                        .child(
                            TabBar::new("right-panel-surface-tabs")
                                .flex_1()
                                .min_w_0()
                                .segmented()
                                .small()
                                .selected_index(selected_index)
                                .children(surfaces.iter().map(|surface| {
                                    Tab::new()
                                        .icon(surface.icon())
                                        .debug_selector({
                                            let label = surface.label();
                                            move || format!("right-panel-tab-{label}")
                                        })
                                        .aria_label(format!("{} panel", surface.label()))
                                        .tooltip({
                                            let label = surface.label();
                                            move |window, cx| Tooltip::new(label).build(window, cx)
                                        })
                                }))
                                .on_click(cx.listener(move |this, ix, _window, cx| {
                                    if let Some(surface) = Surface::all().get(*ix).copied() {
                                        this.open_surface(surface, cx);
                                    }
                                })),
                        )
                        .children((!matches!(self.active_surface, Some(Surface::Agents | Surface::Trajectory))).then(|| {
                            Button::new("right-panel-refresh")
                                .accessibility_label("Refresh surface")
                                .icon(Icon::default().path("icons/refresh-cw.svg"))
                                .tooltip("Refresh surface")
                                .ghost()
                                .xsmall()
                                .on_click(cx.listener(|this, _event, _window, cx| {
                                    this.refresh_active_surface(cx);
                                    cx.notify();
                                }))
                        })),
                ),
            )
    }

    fn render_chooser(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().colors;
        div()
            .min_w_0()
            .flex_1()
            .flex()
            .items_center()
            .justify_center()
            .p_6()
            .child(
                div()
                    .w_full()
                    .max_w(rems(26.0))
                    .flex()
                    .flex_col()
                    .items_center()
                    .child(
                        div()
                            .text_sm()
                            .font_weight(FontWeight::MEDIUM)
                            .child("Open a surface"),
                    )
                    .child(
                        div()
                            .mt_1()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child("Choose what to show in the right panel"),
                    )
                    .child(div().mt_4().w_full().flex().flex_col().gap_2().children(
                        Surface::all().into_iter().map(|surface| {
                            Button::new(SharedString::from(format!(
                                "right-panel-card-{}",
                                surface.label().to_lowercase()
                            )))
                            .accessibility_label(surface.label())
                            .debug_selector({
                                let label = surface.label();
                                move || format!("right-panel-choice-{label}")
                            })
                            .icon(surface.icon())
                            .label(surface.label())
                            .outline()
                            .w_full()
                            .justify_start()
                            .on_click(cx.listener(
                                move |this, _event, _window, cx| {
                                    this.open_surface(surface, cx);
                                },
                            ))
                        }),
                    )),
            )
    }

    fn render_review_diff(&self, state: &ReviewDiffState, cx: &mut Context<Self>) -> AnyElement {
        let body = div().flex_1().min_h_0().overflow_y_scrollbar().p_3();
        match state {
            ReviewDiffState::Loading => body
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap_2()
                        .child(Spinner::new().small())
                        .child("Updating diff…"),
                )
                .into_any_element(),
            ReviewDiffState::Failed(error) => body
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .items_start()
                        .gap_2()
                        .child("Could not load diff")
                        .child(
                            div()
                                .text_xs()
                                .text_color(cx.theme().muted_foreground)
                                .child(error.clone()),
                        )
                        .child(
                            Button::new("retry-review-diff")
                                .debug_selector(|| "retry-review-diff".into())
                                .small()
                                .label("Retry")
                                .on_click(cx.listener(|this, _event, _window, cx| {
                                    this.reload_review_diff(cx)
                                })),
                        ),
                )
                .into_any_element(),
            ReviewDiffState::Ready { empty: true } => body
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .items_start()
                        .gap_2()
                        .child(if self.review_diff_options.ignore_whitespace {
                            "No text changes to show with whitespace ignored"
                        } else {
                            "No text changes to show"
                        })
                        .children(self.review_diff_options.ignore_whitespace.then(|| {
                            Button::new("show-whitespace-changes")
                                .debug_selector(|| "show-whitespace-changes".into())
                                .small()
                                .label("Show whitespace changes")
                                .on_click(cx.listener(|this, _event, _window, cx| {
                                    this.set_ignore_whitespace(false, cx)
                                }))
                        })),
                )
                .into_any_element(),
            ReviewDiffState::Ready { empty: false } => body
                .child(TextView::new(&self.document_state).selectable(true))
                .into_any_element(),
        }
    }

    fn render_files(&self, cx: &mut Context<Self>) -> AnyElement {
        if let Some(title) = &self.document_title {
            let is_dirty = self.is_dirty;
            let has_editor = self.editor_state.is_some();
            let lang = detect_language(title);
            return div()
                .flex_1()
                .min_h_0()
                .flex()
                .flex_col()
                .child(
                    div()
                        .h(rems(2.375))
                        .px_2()
                        .flex()
                        .items_center()
                        .justify_between()
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .gap_1()
                                .min_w_0()
                                .flex_1()
                                .child(
                                    Button::new("right-panel-document-back")
                                        .debug_selector(|| "right-panel-document-back".into())
                                        .accessibility_label(match self.active_surface {
                                            Some(Surface::Review) => "Back to changed files",
                                            _ => "Back to project files",
                                        })
                                        .icon(IconName::ArrowLeft)
                                        .tooltip(match self.active_surface {
                                            Some(Surface::Review) => "Back to changed files",
                                            _ => "Back to project files",
                                        })
                                        .ghost()
                                        .xsmall()
                                        .on_click(cx.listener(|this, _event, _window, cx| {
                                            this.close_document(cx);
                                        })),
                                )
                                .child(IconName::File)
                                .child(
                                    div()
                                        .min_w_0()
                                        .truncate()
                                        .text_xs()
                                        .font_weight(FontWeight::MEDIUM)
                                        .child(title.clone()),
                                )
                                .children(
                                    is_dirty.then(|| Tag::warning().child("modified").xsmall()),
                                )
                                .children(
                                    has_editor
                                        .then(|| Tag::secondary().child(lang).outline().xsmall()),
                                ),
                        )
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .gap_1()
                                .children(has_editor.then(|| {
                                    Button::new("save-document")
                                        .small()
                                        .label("Save")
                                        .icon(IconName::Check)
                                        .accessibility_label(if is_dirty {
                                            "Save the open document"
                                        } else {
                                            "No unsaved changes"
                                        })
                                        .tooltip(if is_dirty {
                                            "Save the open document"
                                        } else {
                                            "No unsaved changes"
                                        })
                                        .disabled(!is_dirty)
                                        .on_click(cx.listener(|this, _event, _window, cx| {
                                            this.save_active_document(cx);
                                        }))
                                }))
                                .child(
                                    Button::new("close-document")
                                        .debug_selector(|| "close-document".into())
                                        .accessibility_label("Close document")
                                        .small()
                                        .ghost()
                                        .icon(IconName::Close)
                                        .tooltip("Close document")
                                        .on_click(cx.listener(|this, _event, _window, cx| {
                                            this.close_document(cx);
                                        })),
                                ),
                        ),
                )
                .children(self.review_diff_request.as_ref().map(|_| {
                    div()
                        .px_3()
                        .py_2()
                        .flex()
                        .flex_col()
                        .gap_2()
                        .child(
                            Checkbox::new("review-ignore-whitespace")
                                .debug_selector(|| "review-ignore-whitespace".into())
                                .small()
                                .label("Ignore whitespace")
                                .checked(self.review_diff_options.ignore_whitespace)
                                .accessibility_label("Ignore whitespace. Ignores whitespace when comparing lines. Whitespace can affect program behavior. File counts and commit selection are unchanged.")
                                .tooltip("Ignores whitespace when comparing lines. Whitespace can affect program behavior. File counts and commit selection are unchanged.")
                                .on_click(cx.listener(|this, checked, _window, cx| {
                                    this.set_ignore_whitespace(*checked, cx);
                                })),
                        )
                        .children(self.review_diff_options.ignore_whitespace.then(|| {
                            div()
                                .text_xs()
                                .text_color(cx.theme().muted_foreground)
                                .child("Whitespace ignored · Display only")
                        }))
                }))
                .child(Separator::horizontal())
                .child(if let Some(ref editor) = self.editor_state {
                    div()
                        .flex_1()
                        .min_h_0()
                        .w_full()
                        .h_full()
                        .child(Editor::new(editor).bordered(false).size_full())
                        .into_any_element()
                } else if let Some(state) = &self.review_diff_state {
                    self.render_review_diff(state, cx)
                } else {
                    div()
                        .flex_1()
                        .min_h_0()
                        .overflow_y_scrollbar()
                        .p_3()
                        .child(TextView::new(&self.document_state).selectable(true))
                        .into_any_element()
                })
                .into_any_element();
        }
        let model = self.model.clone();

        div()
            .flex_1()
            .min_h_0()
            .py_2()
            .child(Button::new("find-in-files").label("Find in files…").ghost().small()
                .on_click(cx.listener(|this, _, window, cx| {
                    super::file_search::open(this.model.clone(), window, cx);
                })))
            .child(
                Tree::new(
                    &self.tree_state,
                    move |ix, entry, is_selected, _window, cx| {
                        let relative_path = entry.item().id.to_string();
                        let name = entry.item().label.to_string();
                        let is_folder = entry.is_folder();
                        let is_expanded = entry.is_expanded();
                        let depth = entry.depth();

                        let target_path = relative_path.clone();
                        let click_model = model.clone();
                        let theme = cx.theme().colors;

                        ListItem::new(format!("tree-item-{ix}"))
                            .mx_1()
                            .rounded_md()
                            .px_1p5()
                            .py_1()
                            .pl(rems(0.375 + depth as f32 * 0.75))
                            .selected(is_selected)
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap_1p5()
                                    .text_xs()
                                    .text_color(if is_selected {
                                        theme.foreground
                                    } else {
                                        theme.muted_foreground
                                    })
                                    .child(if is_folder {
                                        div()
                                            .w(rems(0.875))
                                            .flex_none()
                                            .flex()
                                            .items_center()
                                            .justify_center()
                                            .child(if is_expanded {
                                                Icon::new(IconName::ChevronDown)
                                                    .xsmall()
                                                    .into_any_element()
                                            } else {
                                                Icon::new(IconName::ChevronRight)
                                                    .xsmall()
                                                    .into_any_element()
                                            })
                                            .into_any_element()
                                    } else {
                                        div().w(rems(0.875)).flex_none().into_any_element()
                                    })
                                    .child(if is_folder {
                                        Icon::new(IconName::Folder).xsmall().into_any_element()
                                    } else {
                                        Icon::new(IconName::File).xsmall().into_any_element()
                                    })
                                    .child(name),
                            )
                            .when(!is_folder, move |item| {
                                item.on_click(move |_event, _window, cx| {
                                    click_model.update(cx, |state, cx| {
                                        state.request_open_file(target_path.clone());
                                        cx.notify();
                                    });
                                })
                            })
                    },
                )
                .context_menu({
                    let model = self.model.clone();
                    let project = self.project.clone();
                    move |_ix, entry, menu, _window, _cx| {
                        let relative_path = entry.item().id.to_string();
                        let is_folder = entry.is_folder();
                        let absolute_path = project
                            .as_ref()
                            .map(|p| p.join(&relative_path).display().to_string());
                        let ed_path = relative_path.clone();
                        let text = relative_path.clone();
                        let model_ref = model.clone();

                        let mut menu = menu;
                        if !is_folder {
                            menu = menu.item(PopupMenuItem::new("Open in Editor Tab").on_click(
                                move |_event, _window, cx| {
                                    model_ref.update(cx, |state, cx| {
                                        state.request_open_file(ed_path.clone());
                                        cx.notify();
                                    });
                                },
                            ));
                        }
                        menu = menu.item(PopupMenuItem::new("Copy Relative Path").on_click(
                            move |_event, window, cx| {
                                cx.write_to_clipboard(ClipboardItem::new_string(text.clone()));
                                window.push_notification(
                                    Notification::info("Copied relative path"),
                                    cx,
                                );
                            },
                        ));
                        if let Some(abs) = absolute_path {
                            menu = menu.item(PopupMenuItem::new("Copy Absolute Path").on_click(
                                move |_event, window, cx| {
                                    cx.write_to_clipboard(ClipboardItem::new_string(abs.clone()));
                                    window.push_notification(
                                        Notification::info("Copied absolute path"),
                                        cx,
                                    );
                                },
                            ));
                        }
                        menu
                    }
                }),
            )
            .into_any_element()
    }

    pub(crate) fn handle_discard_option(
        panel: Entity<RightPanelView>,
        opt: DiscardOption,
        window: &mut Window,
        cx: &mut App,
    ) {
        if opt.requires_confirmation() {
            let (title, description) = opt.confirmation_prompt().unwrap_or((
                "Discard changes?".into(),
                "Are you sure you want to discard these changes? This cannot be undone.".into(),
            ));
            let action = opt.git_action();
            cx.spawn(async move |cx| {
                let confirmed = rfd::AsyncMessageDialog::new()
                    .set_title(&title)
                    .set_description(&description)
                    .set_buttons(rfd::MessageButtons::YesNo)
                    .show()
                    .await;
                if matches!(confirmed, rfd::MessageDialogResult::Yes) {
                    let _ = panel.update(cx, |this, cx| {
                        this.run_git_action_without_window(action, cx);
                    });
                }
            })
            .detach();
        } else {
            let action = opt.git_action();
            panel.update(cx, |this, cx| {
                this.run_git_action(action, window, cx);
            });
        }
    }

    fn filtered_review_files(&self, cx: &Context<Self>) -> Vec<GitFile> {
        let query = self
            .review_filter_input
            .read(cx)
            .value()
            .trim()
            .to_lowercase();
        if query.is_empty() {
            self.review_files.clone()
        } else {
            self.review_files
                .iter()
                .filter(|f| f.path.to_lowercase().contains(&query))
                .cloned()
                .collect()
        }
    }

    pub(crate) fn diff_addition_percent(additions: u32, deletions: u32) -> f32 {
        match (additions, deletions) {
            (0, _) => 0.0,
            (_, 0) => 100.0,
            _ => (additions as f32 / (additions + deletions) as f32 * 100.0).clamp(5.0, 95.0),
        }
    }

    fn render_file_item(
        &self,
        file: &GitFile,
        is_tree_node: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = cx.theme().colors;
        let panel_entity = cx.entity().clone();

        let path = file.path.clone();
        let (directory, filename) = path.rsplit_once('/').unwrap_or(("", path.as_str()));
        let filename = filename.to_owned();
        let directory = directory.to_owned();
        let path_for_chk = path.clone();
        let is_selected = self.selected_files.contains(&path);
        let absolute_path = self
            .project
            .as_ref()
            .map(|root| root.join(&path).display().to_string());
        let status = file.status_char().to_string();
        let is_staged = file.staged;
        let context_path = path.clone();
        let is_open = self
            .document_title
            .as_deref()
            .is_some_and(|title| title == format!("Review · {path}").as_str());

        let (status_color, status_bg) = match file.status_char() {
            'A' | '?' => (theme.success, theme.success.opacity(0.15)),
            'D' => (theme.danger, theme.danger.opacity(0.15)),
            'R' => (theme.link, theme.link.opacity(0.15)),
            _ => (theme.warning, theme.warning.opacity(0.15)),
        };

        let row_id = SharedString::from(format!("review-file-{path}"));
        let row = div()
            .id(row_id)
            .debug_selector(|| "review-file-row".into())
            .w_full()
            .min_w_0()
            .h_8()
            .min_h_8()
            .max_h_8()
            .flex_shrink_0()
            .overflow_hidden()
            .px_2()
            .rounded_md()
            .flex()
            .items_center()
            .gap_2()
            .bg(if is_selected {
                theme.list_active
            } else {
                gpui::transparent_black()
            })
            .hover(|row| {
                row.bg(if is_selected {
                    theme.list_active_border.opacity(0.35)
                } else {
                    theme.list_hover
                })
            })
            .focus(|row| row.border_color(theme.ring))
            .child(
                Checkbox::new(SharedString::from(format!("chk-{path}")))
                    .accessibility_label(format!("Select {path} for Git actions"))
                    .checked(is_selected)
                    .small()
                    .on_click(cx.listener(move |this, checked, _window, cx| {
                        if *checked {
                            this.selected_files.insert(path_for_chk.clone());
                        } else {
                            this.selected_files.remove(&path_for_chk);
                        }
                        cx.notify();
                    })),
            )
            .child(
                Button::new(SharedString::from(format!("review-file-btn-{path}")))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .items_center()
                            .gap_2()
                            .child(
                                div()
                                    .debug_selector(|| "review-filename".into())
                                    .min_w(px(40.0))
                                    .max_w_full()
                                    .truncate()
                                    .child(filename),
                            )
                            .when(!directory.is_empty() && !is_tree_node, |row| {
                                row.child(
                                    div()
                                        .min_w_0()
                                        .flex_1()
                                        .truncate()
                                        .text_ellipsis_start()
                                        .text_xs()
                                        .text_color(theme.muted_foreground)
                                        .child(directory),
                                )
                            }),
                    )
                    .accessibility_label(format!(
                        "Review {path}, status {status}, {} additions, {} deletions",
                        file.additions, file.deletions
                    ))
                    .tooltip(format!(
                        "Review {path} · {status} · +{} −{}{}",
                        file.additions,
                        file.deletions,
                        absolute_path
                            .as_deref()
                            .map(|abs| format!("\n{abs}"))
                            .unwrap_or_default()
                    ))
                    .ghost()
                    .small()
                    .flex_1()
                    .min_w_0()
                    .overflow_hidden()
                    .justify_start()
                    .selected(is_open)
                    .on_click(cx.listener({
                        let path = path.clone();
                        move |this, _, _, cx| {
                            this.open_file_diff(path.clone(), cx);
                        }
                    })),
            )
            .child(
                div()
                    .debug_selector(|| "review-file-status".into())
                    .flex_none()
                    .px_1p5()
                    .py_0p5()
                    .rounded_sm()
                    .bg(status_bg)
                    .text_xs()
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(status_color)
                    .child(status),
            )
            .children((file.additions > 0 || file.deletions > 0).then(|| {
                div()
                    .debug_selector(|| "review-file-stats".into())
                    .flex_none()
                    .flex()
                    .items_center()
                    .gap_1()
                    .text_xs()
                    .children((file.additions > 0).then(|| {
                        div()
                            .text_color(theme.success)
                            .child(format!("+{}", file.additions))
                    }))
                    .children((file.deletions > 0).then(|| {
                        div()
                            .text_color(theme.danger)
                            .child(format!("\u{2212}{}", file.deletions))
                    }))
            }))
            .context_menu({
                let path = context_path.clone();
                let absolute_path = absolute_path.clone();
                let project = self.project.clone();
                let model = self.model.clone();
                let panel = panel_entity.clone();
                let ext = std::path::Path::new(&path)
                    .extension()
                    .and_then(|e| e.to_str())
                    .map(|e| e.to_string());
                move |menu, _window, _cx| {
                    let diff_path = path.clone();
                    let discard_path = path.clone();
                    let ignore_path = path.clone();
                    let rel_path_1 = path.clone();
                    let project_ref = project.clone();
                    let model_ref = model.clone();
                    let panel_ignore = panel.clone();
                    let panel_ignore_ext = panel.clone();
                    let panel_stage = panel.clone();

                    let mut menu = menu;
                    if is_staged {
                        let unstage_p = path.clone();
                        menu = menu.item(PopupMenuItem::new("Unstage File").on_click(
                            move |_event, window, cx| {
                                panel_stage.update(cx, |this, cx| {
                                    this.run_git_action(
                                        GitAction::UnstageFile(unstage_p.clone()),
                                        window,
                                        cx,
                                    );
                                });
                            },
                        ));
                    } else {
                        let stage_p = path.clone();
                        menu = menu.item(PopupMenuItem::new("Stage File").on_click(
                            move |_event, window, cx| {
                                panel_stage.update(cx, |this, cx| {
                                    this.run_git_action(
                                        GitAction::StageFile(stage_p.clone()),
                                        window,
                                        cx,
                                    );
                                });
                            },
                        ));
                    }

                    let (selected_paths, total_files) = {
                        let panel_ref = panel.read(_cx);
                        let selected_paths: Vec<String> =
                            panel_ref.selected_files.iter().cloned().collect();
                        (selected_paths, panel_ref.review_files.len())
                    };
                    for opt in discard_options(&discard_path, &selected_paths, total_files) {
                        let panel_action = panel.clone();
                        let label = opt.label();
                        let opt_action = opt.clone();
                        menu = menu.item(PopupMenuItem::new(label).on_click(
                            move |_event, window, cx| {
                                Self::handle_discard_option(
                                    panel_action.clone(),
                                    opt_action.clone(),
                                    window,
                                    cx,
                                );
                            },
                        ));
                    }

                    menu = menu.separator().item(
                        PopupMenuItem::new("Ignore File (.gitignore)").on_click(
                            move |_event, window, cx| {
                                panel_ignore.update(cx, |this, cx| {
                                    this.run_git_action(
                                        GitAction::IgnoreFile(ignore_path.clone()),
                                        window,
                                        cx,
                                    );
                                });
                            },
                        ),
                    );

                    if let Some(ref ext) = ext {
                        let ext_label = format!("Ignore all *.{ext} files");
                        let ext_to_ignore = ext.clone();
                        menu = menu.item(PopupMenuItem::new(ext_label).on_click(
                            move |_event, window, cx| {
                                panel_ignore_ext.update(cx, |this, cx| {
                                    this.run_git_action(
                                        GitAction::IgnoreExtension(ext_to_ignore.clone()),
                                        window,
                                        cx,
                                    );
                                });
                            },
                        ));
                    }

                    menu = menu.separator().item(
                        PopupMenuItem::new("Open Diff in Editor Tab").on_click(
                            move |_event, _window, cx| {
                                let Some(proj) = project_ref.clone() else {
                                    return;
                                };
                                let diff_project = proj.clone();
                                let target = diff_path.clone();
                                let m = model_ref.clone();
                                let client = m.read(cx).daemon_client.clone();
                                cx.spawn(async move |cx| {
                                    let diff_target = target.clone();
                                    let content = cx
                                        .background_executor()
                                        .spawn(async move {
                                            threadlane_ui_state::project_io::diff_file(
                                                &client,
                                                &diff_project,
                                                diff_target,
                                                threadlane_git::DiffOptions::default(),
                                            )
                                            .await
                                            .unwrap_or_else(|error| error)
                                        })
                                        .await;
                                    let _ = m.update(cx, |state, cx| {
                                        state.request_open_diff(proj, target, content);
                                        cx.notify();
                                    });
                                })
                                .detach();
                            },
                        ),
                    );

                    menu = menu
                        .separator()
                        .item(PopupMenuItem::new("Copy File Path").on_click(
                            move |_event, window, cx| {
                                cx.write_to_clipboard(ClipboardItem::new_string(
                                    rel_path_1.clone(),
                                ));
                                window
                                    .push_notification(Notification::info("Copied file path"), cx);
                            },
                        ));

                    if let Some(ref abs_path) = absolute_path {
                        let abs_text = abs_path.clone();
                        let reveal_text = abs_path.clone();
                        #[cfg(target_os = "macos")]
                        let reveal_label = "Reveal in Finder";
                        #[cfg(target_os = "windows")]
                        let reveal_label = "Reveal in File Explorer";
                        #[cfg(all(unix, not(target_os = "macos")))]
                        let reveal_label = "Reveal in File Manager";

                        menu = menu
                            .item(PopupMenuItem::new("Copy Absolute File Path").on_click(
                                move |_event, window, cx| {
                                    cx.write_to_clipboard(ClipboardItem::new_string(
                                        abs_text.clone(),
                                    ));
                                    window.push_notification(
                                        Notification::info("Copied absolute file path"),
                                        cx,
                                    );
                                },
                            ))
                            .separator()
                            .item(PopupMenuItem::new(reveal_label).on_click(
                                move |_event, _window, _cx| {
                                    threadlane_git::reveal_in_file_manager(std::path::Path::new(
                                        &reveal_text,
                                    ));
                                },
                            ));
                    }
                    menu
                }
            });
        div()
            .w_full()
            .min_w_0()
            .px_2()
            .child(row)
            .into_any_element()
    }

    fn render_review_file_row(
        &mut self,
        index: usize,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let files = self.filtered_review_files(cx);
        let Some(file) = files.get(index).cloned() else {
            return div().into_any_element();
        };
        self.render_file_item(&file, false, cx)
    }

    fn render_review(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let filtered_count = self.filtered_review_files(cx).len();
        if self.review_files_list_state.item_count() != filtered_count {
            self.review_files_list_state
                .reset_with_uniform_height(filtered_count, window.rem_size() * 2.0);
        }
        let panel_entity = cx.entity().clone();
        let theme = cx.theme().colors;
        if let Some(error) = self.review_error.clone() {
            return self.render_review_error(&error, cx);
        }
        let total_files = self.review_files.len();
        let selected_count = self.selected_files.len();
        let all_selected = total_files > 0 && selected_count == total_files;

        let selected_additions: u32 = self
            .review_files
            .iter()
            .filter(|f| self.selected_files.contains(&f.path))
            .map(|f| f.additions)
            .sum();
        let selected_deletions: u32 = self
            .review_files
            .iter()
            .filter(|f| self.selected_files.contains(&f.path))
            .map(|f| f.deletions)
            .sum();

        let branch = self
            .git_status
            .as_ref()
            .and_then(|s| s.branch.as_deref())
            .unwrap_or("No branch");

        let can_push = !self.git_busy
            && !self.git_message_pending
            && self
                .git_status
                .as_ref()
                .is_some_and(|status| status.ahead > 0);

        let last_fetched_str = if let Some(instant) = self.last_fetched_time {
            let secs = instant.elapsed().as_secs();
            if secs < 60 {
                "Last fetched just now".to_string()
            } else if secs < 3600 {
                format!("Last fetched {} minutes ago", secs / 60)
            } else {
                format!("Last fetched {} hours ago", secs / 3600)
            }
        } else {
            "Fetch latest changes".to_string()
        };

        let status = self.git_status.as_ref();
        let behind = status.map_or(0, |s| s.behind);
        let ahead = status.map_or(0, |s| s.ahead);
        let can_publish = can_publish_branch(self.project.is_some(), status);
        let can_create_pr =
            can_create_pull_request(self.project.is_some() && !self.worktree_unavailable, status);

        let sync_button = if can_publish {
            Button::new("git-sync-action-btn")
                .icon(IconName::ArrowUp)
                .label("Publish branch")
                .accessibility_label("Publish this branch to origin")
                .small()
                .tooltip("Publish this branch to origin")
                .on_click(cx.listener(|this, _event, window, cx| {
                    this.run_git_action(GitAction::Push, window, cx);
                }))
        } else if behind > 0 {
            Button::new("git-sync-action-btn")
                .icon(IconName::ArrowDown)
                .label(format!("Pull ({behind})"))
                .accessibility_label("Pull latest changes from origin")
                .small()
                .tooltip("Pull latest changes from origin")
                .on_click(cx.listener(|this, _event, window, cx| {
                    this.run_git_action(GitAction::Pull, window, cx);
                }))
        } else if ahead > 0 {
            Button::new("git-sync-action-btn")
                .icon(IconName::ArrowUp)
                .label(format!("Push ({ahead})"))
                .accessibility_label("Push local commits to origin")
                .small()
                .tooltip("Push local commits to origin")
                .on_click(cx.listener(|this, _event, window, cx| {
                    this.run_git_action(GitAction::Push, window, cx);
                }))
        } else {
            Button::new("git-sync-action-btn")
                .icon(Icon::default().path("icons/download.svg"))
                .label("Fetch")
                .accessibility_label(&last_fetched_str)
                .ghost()
                .small()
                .tooltip(last_fetched_str)
                .on_click(cx.listener(|this, _event, window, cx| {
                    this.run_git_action(GitAction::Fetch, window, cx);
                }))
        };

        let sync_actions = div()
            .flex()
            .items_center()
            .gap_1()
            .child(sync_button.disabled(self.git_busy))
            .when(total_files > 0, |row| {
                row.child(
                    Button::new("git-stash-changes")
                        .label("Stash…")
                        .outline()
                        .small()
                        .tooltip("Stash changes…")
                        .disabled(self.git_busy)
                        .on_click(cx.listener(|this, _event, window, cx| {
                            this.close_all_git_dialogs();
                            this.stash_dialog_open = true;
                            this.stash_message_input
                                .update(cx, |input, cx| input.focus(window, cx));
                            cx.notify();
                        })),
                )
            })
            .when(can_create_pr, |row| {
                row.child(
                    Button::new("git-create-pull-request")
                        .icon(IconName::Github)
                        .label("Create draft PR…")
                        .accessibility_label("Review and create a draft pull request on GitHub")
                        .outline()
                        .small()
                        .tooltip("Review and create a draft pull request on GitHub")
                        .disabled(self.git_busy)
                        .on_click(cx.listener(|this, _event, window, cx| {
                            this.open_draft_pr_dialog(window, cx);
                        })),
                )
            });

        let branch_header = div()
            .flex()
            .items_center()
            .justify_between()
            .px_3()
            .py_2()
            .gap_2()
            .border_b_1()
            .border_color(theme.border)
            .bg(theme.list_head)
            .child(
                Button::new("git-branch-selector-btn")
                    .accessibility_label(format!("Manage branches, current branch {branch}"))
                    .ghost()
                    .small()
                    .selected(self.branch_popover_open)
                    .flex()
                    .items_center()
                    .gap_2()
                    .min_w_0()
                    .flex_1()
                    .px_2()
                    .py_1()
                    .rounded_md()
                    .on_click(cx.listener(|this, _event, _window, cx| {
                        this.branch_popover_open = !this.branch_popover_open;
                        cx.notify();
                    }))
                    .child(
                        div()
                            .size_4()
                            .flex()
                            .items_center()
                            .justify_center()
                            .text_color(theme.muted_foreground)
                            .child(Icon::default().path("icons/git/branch.svg")),
                    )
                    .child(
                        div()
                            .flex_1()
                            .truncate()
                            .text_xs()
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(theme.foreground)
                            .child(branch.to_string()),
                    )
                    .child(
                        div()
                            .size(rems(0.875))
                            .flex()
                            .items_center()
                            .justify_center()
                            .text_color(theme.muted_foreground)
                            .child(if self.branch_popover_open {
                                IconName::ChevronUp
                            } else {
                                IconName::ChevronDown
                            }),
                    ),
            )
            .child(sync_actions);

        let pr_expanded = self.pr_expanded;
        let pr_card = self.git_status.as_ref().and_then(|s| s.pr.as_ref()).map(|pr| {
            let comments_pr = pr.clone();
            let pr_url = pr.url.clone();
            let pr_num = pr.number;
            let pr_title = pr.title.clone();
            let pr_title_display = if pr.title.is_empty() {
                format!("PR #{pr_num}")
            } else {
                format!("#{pr_num} {}", pr.title)
            };

            let failing_checks = pr.failing_checks;
            let pending_checks = pr.pending_checks;
            let total_checks = pr.total_checks;
            let comments_count = threadlane_git::collect_actionable_pr_feedback(pr).len();

            let failing_check_names: Vec<String> = pr
                .checks
                .iter()
                .filter(|c| {
                    let concl = c.conclusion.as_deref().unwrap_or("").to_uppercase();
                    matches!(
                        concl.as_str(),
                        "FAILURE" | "TIMED_OUT" | "ACTION_REQUIRED" | "CANCELLED" | "ERROR"
                    )
                })
                .map(|c| c.name.clone())
                .collect();
            let failed_summary = failing_check_names.join(", ");

            div()
                .flex()
                .flex_col()
                .gap_1p5()
                .mx_3()
                .my_2()
                .p_2p5()
                .rounded_lg()
                .border_1()
                .border_color(theme.border)
                .bg(theme.group_box)
                .child(
                    Button::new("pr-card-toggle")
                        .accessibility_label(if pr_expanded {
                            "Collapse pull request details"
                        } else {
                            "Expand pull request details"
                        })
                        .ghost()
                        .h_auto()
                        .w_full()
                        .p_0()
                        .tooltip(if pr_expanded { "Collapse" } else { "Expand" })
                        .on_click(cx.listener(|this, _event, _window, cx| {
                            this.pr_expanded = !this.pr_expanded;
                            cx.notify();
                        }))
                        .child(
                    div()
                        .flex()
                        .items_center()
                        .justify_between()
                        .gap_2()
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .gap_1p5()
                                .min_w_0()
                                .flex_1()
                                .child(
                                    div()
                                        .size(rems(0.875))
                                        .flex()
                                        .items_center()
                                        .justify_center()
                                        .text_color(theme.muted_foreground)
                                        .child(if pr_expanded {
                                            IconName::ChevronDown
                                        } else {
                                            IconName::ChevronRight
                                        }),
                                )
                                .child(
                                    div()
                                        .size_4()
                                        .flex()
                                        .items_center()
                                        .justify_center()
                                        .text_color(theme.muted_foreground)
                                        .child(Icon::default().path("icons/git/actions.svg")),
                                )
                                .child(
                                    div()
                                        .truncate()
                                        .text_xs()
                                        .font_weight(FontWeight::SEMIBOLD)
                                        .text_color(theme.foreground)
                                        .child(pr_title_display),
                                ),
                        )
                        .when(!pr_url.is_empty(), |row| {
                            let target_url = pr_url.clone();
                            row.child(
                                Button::new("pr-link-btn")
                                    .accessibility_label("Open pull request in browser")
                                    .icon(IconName::ExternalLink)
                                    .ghost()
                                    .xsmall()
                                    .tooltip("Open pull request in browser")
                                    .on_click(move |_event, _window, cx| {
                                        cx.open_url(&target_url);
                                    }),
                            )
                        }),
                        ),
                )
                .when(pr_expanded, |card| {
                    card.child(
                    div()
                        .flex()
                        .items_center()
                        .justify_between()
                        .gap_2()
                        .pt_0p5()
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .gap_1p5()
                                .min_w_0()
                                .child(
                                    div()
                                        .size(rems(0.875))
                                        .flex()
                                        .items_center()
                                        .justify_center()
                                        .text_color(if failing_checks > 0 {
                                            theme.danger
                                        } else if pending_checks > 0 {
                                            theme.warning
                                        } else {
                                            theme.success
                                        })
                                        .child(if failing_checks > 0 {
                                            IconName::Close
                                        } else if pending_checks > 0 {
                                            IconName::Asterisk
                                        } else {
                                            IconName::Check
                                        }),
                                )
                                .child(
                                    div()
                                        .truncate()
                                        .text_xs()
                                        .text_color(if failing_checks > 0 {
                                            theme.danger
                                        } else {
                                            theme.muted_foreground
                                        })
                                        .child(if failing_checks > 0 {
                                            format!(
                                                "{failing_checks} failing check{}",
                                                if failing_checks == 1 { "" } else { "s" }
                                            )
                                        } else if pending_checks > 0 {
                                            format!("{pending_checks} in progress")
                                        } else {
                                            format!("All {} checks passed", total_checks.max(1))
                                        }),
                                ),
                        )
                        .child(if failing_checks > 0 {
                            let fix_pr_num = pr_num;
                            let fix_pr_title = pr_title.clone();
                            let fix_failed_summary = failed_summary.clone();
                            Button::new("fix-ci-btn")
                                .label("Fix CI")
                                .accessibility_label("Ask AI to fix failing CI checks")
                                .outline()
                                .xsmall()
                                .tooltip("Ask AI to fix failing CI checks")
                                .on_click(cx.listener(move |this, _event, _window, cx| {
                                    let prompt = format!(
                                        "Please inspect and fix the failing CI check on PR #{fix_pr_num} ({fix_pr_title}): {fix_failed_summary}"
                                    );
                                    this.model.update(cx, |state, _cx| {
                                        state.request_composer_prompt(prompt);
                                    });
                                    cx.notify();
                                }))
                                .into_any_element()
                        } else {
                            div()
                                .text_xs()
                                .text_color(theme.muted_foreground)
                                .child(format!("{}/{}", pr.passing_checks, pr.total_checks))
                                .into_any_element()
                        }),
                )
                .when(comments_count > 0, |card| {
                    let comments_pr = comments_pr.clone();
                    let comments_project = self.project.clone();
                    card.child(
                        div()
                            .flex()
                            .items_center()
                            .justify_between()
                            .gap_2()
                            .pt_0p5()
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap_1p5()
                                    .min_w_0()
                                    .child(
                                        div()
                                            .size(rems(0.875))
                                            .flex()
                                            .items_center()
                                            .justify_center()
                                            .text_color(theme.muted_foreground)
                                            .child(IconName::File),
                                    )
                                    .child(
                                        div()
                                            .truncate()
                                            .text_xs()
                                            .text_color(theme.muted_foreground)
                                            .child(format!(
                                                "{comments_count} review comment{}",
                                                if comments_count == 1 { "" } else { "s" }
                                            )),
                                    ),
                            )
                            .child(
                                Button::new("address-comments-btn")
                                    .label("Address")
                                    .accessibility_label("Ask AI to address PR comments")
                                    .ghost()
                                    .xsmall()
                                    .tooltip("Ask AI to address PR comments")
                                    .on_click(cx.listener(move |this, _event, _window, cx| {
                                        let Some(work_dir) = comments_project.clone() else {
                                            return;
                                        };
                                        this.model.update(cx, |state, cx| {
                                            match state.address_pr_reviews_manual(
                                                work_dir,
                                                comments_pr.head_ref.clone(),
                                                &comments_pr,
                                            ) {
                                                Ok(_) => state.session_status = Some(
                                                    "Addressing PR review feedback…".into(),
                                                ),
                                                Err(error) => state.session_status = Some(error),
                                            }
                                            cx.notify();
                                        });
                                    })),
                            ),
                    )
                })
                })
        });

        let staged_count = self.review_files.iter().filter(|f| f.staged).count();
        let unstaged_count = self.review_files.iter().filter(|f| f.unstaged).count();
        let has_staged = staged_count > 0;

        let total_additions_all: u32 = self.review_files.iter().map(|f| f.additions).sum();
        let total_deletions_all: u32 = self.review_files.iter().map(|f| f.deletions).sum();
        let total_delta = total_additions_all + total_deletions_all;
        let diff_ratio_bar = (total_delta > 0).then(|| {
            let add_pct = Self::diff_addition_percent(total_additions_all, total_deletions_all);
            let del_pct = 100.0 - add_pct;
            div().px_3().py_0p5().child(
                div()
                    .w_full()
                    .h(rems(0.25))
                    .rounded_full()
                    .overflow_hidden()
                    .bg(theme.muted)
                    .flex()
                    .child(
                        div()
                            .h_full()
                            .w(relative(add_pct / 100.0))
                            .bg(theme.success),
                    )
                    .child(div().h_full().w(relative(del_pct / 100.0)).bg(theme.danger)),
            )
        });

        let review_toolbar = (total_files > 0).then(|| {
            let panel_sb = panel_entity.clone();
            div()
                .flex()
                .flex_col()
                .border_b_1()
                .border_color(theme.border)
                .bg(theme.title_bar)
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap_2()
                        .px_3()
                        .py_1p5()
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .px_2()
                                .py_1()
                                .rounded_md()
                                .bg(theme.input)
                                .border_1()
                                .border_color(theme.border)
                                .flex()
                                .items_center()
                                .gap_1p5()
                                .child(
                                    div()
                                        .size(rems(0.875))
                                        .text_color(theme.muted_foreground)
                                        .child(IconName::Search),
                                )
                                .child(
                                    div().flex_1().min_w_0().child(
                                        Input::new(&self.review_filter_input)
                                            .aria_label("Filter changes")
                                            .appearance(false)
                                            .bordered(false),
                                    ),
                                )
                                .children(
                                    (!self.review_filter_input.read(cx).value().is_empty()).then(
                                        || {
                                            Button::new("clear-review-filter-btn")
                                                .icon(IconName::Close)
                                                .accessibility_label("Clear filter")
                                                .ghost()
                                                .xsmall()
                                                .tooltip("Clear filter")
                                                .on_click(cx.listener(|this, _, window, cx| {
                                                    this.review_filter_input
                                                        .update(cx, |input, cx| {
                                                            input.set_value("", window, cx)
                                                        });
                                                    cx.notify();
                                                }))
                                        },
                                    ),
                                ),
                        )
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .gap_0p5()
                                .rounded_md()
                                .bg(theme.tab_bar_segmented)
                                .p_0p5()
                                .child(
                                    Button::new("review-view-list")
                                        .icon(IconName::Menu)
                                        .accessibility_label("Flat list view")
                                        .ghost()
                                        .xsmall()
                                        .selected(self.review_view_mode == ReviewViewMode::List)
                                        .tooltip("Flat list view")
                                        .on_click(cx.listener(|this, _, _, cx| {
                                            this.review_view_mode = ReviewViewMode::List;
                                            cx.notify();
                                        })),
                                )
                                .child(
                                    Button::new("review-view-tree")
                                        .icon(IconName::FolderOpen)
                                        .accessibility_label("Tree view")
                                        .ghost()
                                        .xsmall()
                                        .selected(self.review_view_mode == ReviewViewMode::Tree)
                                        .tooltip("Tree view")
                                        .on_click(cx.listener(|this, _, _, cx| {
                                            this.review_view_mode = ReviewViewMode::Tree;
                                            cx.notify();
                                        })),
                                ),
                        )
                        .child(
                            div()
                                .child(
                                    Button::new("selection-bar-discard-btn")
                                        .icon(IconName::Undo2)
                                        .accessibility_label("Discard changes")
                                        .ghost()
                                        .xsmall()
                                        .tooltip("Discard changes (right-click for more options)")
                                        .disabled(self.git_busy)
                                        .on_click(cx.listener(|this, _event, window, cx| {
                                            let paths: Vec<String> =
                                                this.selected_files.iter().cloned().collect();
                                            let total = this.review_files.len();
                                            Self::handle_discard_option(
                                                cx.entity().clone(),
                                                if paths.is_empty() {
                                                    DiscardOption::All(total)
                                                } else {
                                                    DiscardOption::Selected(paths)
                                                },
                                                window,
                                                cx,
                                            );
                                        })),
                                )
                                .context_menu({
                                    let panel = panel_sb.clone();
                                    move |menu, _window, cx| {
                                        let (selected_paths, total_files) = {
                                            let panel_ref = panel.read(cx);
                                            let selected_paths: Vec<String> =
                                                panel_ref.selected_files.iter().cloned().collect();
                                            (selected_paths, panel_ref.review_files.len())
                                        };
                                        let mut menu = menu;
                                        for opt in selection_bar_discard_options(
                                            &selected_paths,
                                            total_files,
                                        ) {
                                            let panel_action = panel.clone();
                                            let label = opt.label();
                                            let opt_action = opt.clone();
                                            menu = menu.item(PopupMenuItem::new(label).on_click(
                                                move |_event, window, cx| {
                                                    Self::handle_discard_option(
                                                        panel_action.clone(),
                                                        opt_action.clone(),
                                                        window,
                                                        cx,
                                                    );
                                                },
                                            ));
                                        }
                                        menu
                                    }
                                }),
                        ),
                )
                .child(
                    div()
                        .flex()
                        .items_center()
                        .justify_between()
                        .px_3()
                        .py_1()
                        .border_t_1()
                        .border_color(theme.border)
                        .bg(theme.list_head)
                        .text_xs()
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .gap_2()
                                .child(
                                    Checkbox::new("select-all-files")
                                        .checked(all_selected)
                                        .small()
                                        .disabled(self.git_busy)
                                        .on_click(cx.listener(
                                            move |this, checked, _window, cx| {
                                                if *checked {
                                                    this.selected_files = this
                                                        .review_files
                                                        .iter()
                                                        .map(|f| f.path.clone())
                                                        .collect();
                                                } else {
                                                    this.selected_files.clear();
                                                }
                                                cx.notify();
                                            },
                                        )),
                                )
                                .child(
                                    div()
                                        .font_weight(FontWeight::MEDIUM)
                                        .text_color(theme.foreground)
                                        .child(format!("{selected_count}/{total_files} files")),
                                )
                                .when(selected_additions > 0 || selected_deletions > 0, |stats| {
                                    stats
                                        .child(
                                            div()
                                                .text_color(theme.success)
                                                .child(format!("+{selected_additions}")),
                                        )
                                        .child(
                                            div()
                                                .text_color(theme.danger)
                                                .child(format!("\u{2212}{selected_deletions}")),
                                        )
                                })
                                .child(
                                    Button::new("view-combined-diff-btn")
                                        .label("View Diff")
                                        .accessibility_label(
                                            "Open combined diff of all changes",
                                        )
                                        .ghost()
                                        .xsmall()
                                        .tooltip("Open combined diff of all changes")
                                        .on_click(cx.listener(|this, _event, _window, cx| {
                                            this.open_combined_diff(cx);
                                        })),
                                ),
                        )
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .gap_1()
                                .child(
                                    Button::new("git-stage-all-btn")
                                        .label("Stage all")
                                        .accessibility_label("Stage all changes (git add -A)")
                                        .ghost()
                                        .xsmall()
                                        .disabled(self.git_busy || unstaged_count == 0)
                                        .tooltip("Stage all changes (git add -A)")
                                        .on_click(cx.listener(|this, _event, window, cx| {
                                            this.run_git_action(GitAction::StageAll, window, cx);
                                        })),
                                )
                                .when(has_staged, |row| {
                                    row.child(
                                        Button::new("git-unstage-all-btn")
                                            .label("Unstage all")
                                            .accessibility_label(
                                                "Unstage all changes (git restore --staged .)",
                                            )
                                            .ghost()
                                            .xsmall()
                                            .disabled(self.git_busy)
                                            .tooltip("Unstage all changes (git restore --staged .)")
                                            .on_click(cx.listener(|this, _event, window, cx| {
                                                this.run_git_action(
                                                    GitAction::UnstageAll,
                                                    window,
                                                    cx,
                                                );
                                            })),
                                    )
                                }),
                        ),
                )
                .children(diff_ratio_bar)
        });
        let file_list_content = if self.review_files.is_empty() {
            div()
                .flex_1()
                .flex()
                .flex_col()
                .items_center()
                .justify_center()
                .p_4()
                .text_center()
                .child(
                    div()
                        .mb_2()
                        .size_8()
                        .flex()
                        .items_center()
                        .justify_center()
                        .rounded_full()
                        .bg(theme.success.opacity(0.12))
                        .text_color(theme.success)
                        .child(Icon::new(IconName::Check)),
                )
                .child(
                    div()
                        .text_sm()
                        .font_weight(FontWeight::MEDIUM)
                        .text_color(theme.foreground)
                        .child("No changes"),
                )
                .child(
                    div()
                        .mt_1()
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .child("Working tree is clean"),
                )
                .child(div().mt_3().child(
                    Button::new("refresh-clean-review")
                        .label("Refresh review")
                        .ghost()
                        .small()
                        .tooltip("Refresh the working tree review")
                        .disabled(self.git_busy)
                        .on_click(cx.listener(|this, _event, _window, cx| {
                            this.refresh_active_surface(cx);
                        })),
                ))
                .into_any_element()
        } else if self.review_view_mode == ReviewViewMode::Tree {
            let filtered = self.filtered_review_files(cx);
            let mut dir_map: std::collections::BTreeMap<String, Vec<GitFile>> =
                std::collections::BTreeMap::new();
            for f in filtered {
                let (dir, _) = f.path.rsplit_once('/').unwrap_or(("", &f.path));
                dir_map.entry(dir.to_string()).or_default().push(f);
            }
            div()
                .flex_1()
                .min_h_0()
                .overflow_y_scrollbar()
                .py_1()
                .children(dir_map.into_iter().map(|(dir, dir_files)| {
                    let is_collapsed = self.collapsed_tree_folders.contains(&dir);
                    let dir_key = dir.clone();
                    let folder_label = if dir.is_empty() {
                        "(root)".to_string()
                    } else {
                        dir.clone()
                    };
                    let count = dir_files.len();
                    div()
                        .flex()
                        .flex_col()
                        .child(
                            Button::new(SharedString::from(format!("folder-btn-{dir}")))
                                .ghost()
                                .xsmall()
                                .w_full()
                                .justify_start()
                                .px_2()
                                .py_1()
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    if this.collapsed_tree_folders.contains(&dir_key) {
                                        this.collapsed_tree_folders.remove(&dir_key);
                                    } else {
                                        this.collapsed_tree_folders.insert(dir_key.clone());
                                    }
                                    cx.notify();
                                }))
                                .child(
                                    div()
                                        .flex()
                                        .items_center()
                                        .gap_1p5()
                                        .child(
                                            Icon::new(if is_collapsed {
                                                IconName::ChevronRight
                                            } else {
                                                IconName::ChevronDown
                                            })
                                            .size_3()
                                            .text_color(theme.muted_foreground),
                                        )
                                        .child(
                                            Icon::new(if is_collapsed {
                                                IconName::Folder
                                            } else {
                                                IconName::FolderOpen
                                            })
                                            .size_3p5()
                                            .text_color(theme.muted_foreground),
                                        )
                                        .child(
                                            div()
                                                .text_xs()
                                                .font_weight(FontWeight::MEDIUM)
                                                .text_color(theme.foreground)
                                                .child(folder_label),
                                        )
                                        .child(
                                            Tag::new()
                                                .child(count.to_string())
                                                .with_variant(TagVariant::Secondary)
                                                .small(),
                                        ),
                                ),
                        )
                        .children((!is_collapsed).then(|| {
                            div()
                                .pl_3()
                                .border_l_1()
                                .border_color(theme.border.opacity(0.4))
                                .ml_3()
                                .flex()
                                .flex_col()
                                .children(
                                    dir_files
                                        .into_iter()
                                        .map(|f| self.render_file_item(&f, true, cx)),
                                )
                        }))
                }))
                .into_any_element()
        } else {
            div()
                .relative()
                .flex_1()
                .min_h_0()
                .child(
                    list(
                        self.review_files_list_state.clone(),
                        cx.processor(Self::render_review_file_row),
                    )
                    .size_full()
                    .py_1()
                    .with_sizing_behavior(ListSizingBehavior::Auto),
                )
                .child(div().absolute().inset_0().child(
                    gpui_component::scroll::Scrollbar::vertical(&self.review_files_list_state),
                ))
                .into_any_element()
        };

        let commit_label = if selected_count > 0 && selected_count < total_files {
            format!("Commit {selected_count}")
        } else {
            "Commit".to_string()
        };
        let commit_push_label = if selected_count > 0 && selected_count < total_files {
            format!("Commit {selected_count} & push")
        } else {
            "Commit & push".to_string()
        };

        let commit_val = self.commit_message_input.read(cx).value();
        let first_line = commit_val.lines().next().unwrap_or("");
        let subject_len = first_line.chars().count();
        let counter_color = if subject_len > 72 {
            theme.danger
        } else if subject_len > 50 {
            theme.warning
        } else {
            theme.muted_foreground
        };

        let is_empty = commit_val.trim().is_empty();
        let can_commit = !is_empty && selected_count > 0 && !self.git_busy;

        let commit_footer = div()
            .flex_none()
            .flex()
            .flex_col()
            .gap_2p5()
            .p_3()
            .border_t_1()
            .border_color(theme.border)
            .bg(theme.title_bar)
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .child(
                                div()
                                    .text_xs()
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .text_color(theme.muted_foreground)
                                    .child("COMMIT"),
                            )
                            .when(subject_len > 0, |header| {
                                header.child(
                                    div()
                                        .px_1p5()
                                        .py_0p5()
                                        .rounded_sm()
                                        .bg(counter_color.opacity(0.12))
                                        .text_xs()
                                        .font_weight(FontWeight::MEDIUM)
                                        .text_color(counter_color)
                                        .child(if subject_len > 72 {
                                            format!("{subject_len}/72 (too long)")
                                        } else if subject_len > 50 {
                                            format!("{subject_len}/50")
                                        } else {
                                            format!("{subject_len}")
                                        }),
                                )
                            }),
                    )
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_1()
                            .children((!commit_val.is_empty()).then(|| {
                                Button::new("clear-commit-input")
                                    .icon(IconName::Close)
                                    .accessibility_label("Clear message")
                                    .ghost()
                                    .xsmall()
                                    .tooltip("Clear message")
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.commit_message_input.update(cx, |input, cx| {
                                            input.set_value("", window, cx)
                                        });
                                        cx.notify();
                                    }))
                            }))
                            .child(if self.git_message_pending {
                                Button::new("git-generate-commit-msg")
                                    .child(Spinner::new().xsmall())
                                    .accessibility_label("Generating commit message with AI…")
                                    .ghost()
                                    .xsmall()
                                    .disabled(true)
                                    .tooltip("Generating commit message with AI…")
                            } else {
                                Button::new("git-generate-commit-msg")
                                    .icon(IconName::Bot)
                                    .accessibility_label("Generate commit message with AI")
                                    .ghost()
                                    .xsmall()
                                    .tooltip("Generate commit message with AI")
                                    .disabled(self.git_busy || total_files == 0)
                                    .on_click(cx.listener(|this, _event, _window, cx| {
                                        this.generate_commit_message(cx);
                                    }))
                            }),
                    ),
            )
            .child(
                div()
                    .px_2p5()
                    .py_2()
                    .rounded_md()
                    .bg(theme.input)
                    .border_1()
                    .border_color(theme.border)
                    .focus(|d| d.border_color(theme.ring))
                    .child(
                        Input::new(&self.commit_message_input)
                            .aria_label("Commit summary")
                            .disabled(self.git_busy),
                    ),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(
                        Button::new("git-commit-and-push")
                            .icon(Icon::default().path("icons/git/commit.svg"))
                            .label(commit_push_label)
                            .primary()
                            .small()
                            .flex_1()
                            .tooltip(if can_commit {
                                "Commit the selected changes and push"
                            } else {
                                "Select files and write a message to commit"
                            })
                            .disabled(!can_commit)
                            .on_click(cx.listener(|this, _event, window, cx| {
                                this.run_git_action(GitAction::CommitAndPush, window, cx);
                            })),
                    )
                    .child(
                        Button::new("git-commit-only")
                            .label(commit_label)
                            .outline()
                            .small()
                            .tooltip(if can_commit {
                                "Commit the selected changes locally"
                            } else {
                                "Select files and write a message to commit"
                            })
                            .disabled(!can_commit)
                            .on_click(cx.listener(|this, _event, window, cx| {
                                this.run_git_action(GitAction::Commit, window, cx);
                            })),
                    )
                    .when(can_push, |row| {
                        row.child(
                            Button::new("git-push-only")
                                .accessibility_label("Push commits")
                                .icon(Icon::default().path("icons/git/actions.svg"))
                                .tooltip("Push commits")
                                .ghost()
                                .small()
                                .on_click(cx.listener(|this, _event, window, cx| {
                                    this.run_git_action(GitAction::Push, window, cx);
                                })),
                        )
                    }),
            );
        let stash_banner = self
            .git_status
            .as_ref()
            .and_then(|s| s.current_stash.as_ref())
            .map(|stash| {
                let stash_msg = if stash.message.is_empty() {
                    "Stashed changes on this branch".to_string()
                } else {
                    stash.message.clone()
                };
                let time_str = if stash.relative_time.is_empty() {
                    String::new()
                } else {
                    format!(" • {}", stash.relative_time)
                };
                let idx = stash.index;
                let is_expanded = self.stash_expanded;
                let files_clone = self
                    .stash_files
                    .as_ref()
                    .filter(|(index, _)| *index == idx)
                    .map(|(_, files)| files.clone())
                    .unwrap_or_default();
                let is_loading = self.loading_stash_index == Some(idx);
                let count_label = if is_loading {
                    "Loading files…".to_string()
                } else if self
                    .stash_files
                    .as_ref()
                    .is_some_and(|(index, _)| *index == idx)
                {
                    if files_clone.len() == 1 {
                        "1 file".to_string()
                    } else {
                        format!("{} files", files_clone.len())
                    }
                } else {
                    "Stashed changes".to_string()
                };
                let project = self.project.clone();
                let model = self.model.clone();

                div()
                    .id("stash-banner")
                    .mx_3()
                    .my_2()
                    .p_2p5()
                    .rounded_lg()
                    .border_1()
                    .border_color(theme.border)
                    .bg(theme.group_box)
                    .flex()
                    .flex_col()
                    .gap_1p5()
                    .child(
                        Button::new("stash-header-toggle")
                            .accessibility_label(if is_expanded {
                                "Hide stashed changes"
                            } else {
                                "Show stashed changes"
                            })
                            .tooltip(if is_expanded { "Collapse" } else { "Expand" })
                            .ghost()
                            .h_auto()
                            .w_full()
                            .p_0()
                            .on_click(cx.listener(move |this, _event, _window, cx| {
                                this.stash_expanded = !this.stash_expanded;
                                if this.stash_expanded
                                    && this.loading_stash_index != Some(idx)
                                    && this
                                        .stash_files
                                        .as_ref()
                                        .is_none_or(|(index, _)| *index != idx)
                                {
                                    if let Some(project) = this.project.clone() {
                                        this.loading_stash_index = Some(idx);
                                        let tx = this.event_tx.clone();
                                        let client =
                                            this.model.read(cx).daemon_client.clone();
                                        if let Ok(executor) =
                                            threadlane_ui_state::chat::executor()
                                        {
                                            executor.spawn(async move {
                                                let files =
                                                    threadlane_ui_state::project_io::stash_files(
                                                        &client, &project, idx,
                                                    )
                                                    .await
                                                    .unwrap_or_default();
                                                let _ = tx.send(PanelEvent::StashFilesLoaded {
                                                    project,
                                                    index: idx,
                                                    files,
                                                });
                                            });
                                        }
                                    }
                                }
                                cx.notify();
                            }))
                            .child(
                                div()
                                    .w_full()
                                    .whitespace_normal()
                                    .flex()
                                    .items_center()
                                    .justify_between()
                                    .gap_2()
                                    .child(
                                        div()
                                            .flex()
                                            .items_center()
                                            .gap_1p5()
                                            .min_w_0()
                                            .flex_1()
                                            .child(
                                                div()
                                                    .size(rems(0.875))
                                                    .text_color(theme.primary)
                                                    .child(if is_expanded {
                                                        IconName::ChevronDown
                                                    } else {
                                                        IconName::ChevronRight
                                                    }),
                                            )
                                            .child(
                                                div()
                                                    .text_xs()
                                                    .font_weight(FontWeight::BOLD)
                                                    .text_color(theme.foreground)
                                                    .child("Stashed changes"),
                                            )
                                            .child(
                                                div()
                                                    .text_xs()
                                                    .text_color(theme.muted_foreground)
                                                    .child(format!("({count_label}{time_str})")),
                                            ),
                                    ),
                            ),
                    )
                    .child(
                        div()
                            .truncate()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child(stash_msg),
                    )
                    .children(is_expanded.then(|| {
                        div()
                            .flex()
                            .flex_col()
                            .gap_1()
                            .my_1()
                            .p_1p5()
                            .rounded_md()
                            .bg(theme.background)
                            .border_1()
                            .border_color(theme.border)
                            .children(files_clone.into_iter().map(|file| {
                                let path = file.path.clone();
                                let status = file.status_char().to_string();
                                let status_color = match file.status_char() {
                                    'A' | '?' => theme.success,
                                    'D' => theme.danger,
                                    _ => theme.warning,
                                };
                                let adds = file.additions;
                                let dels = file.deletions;
                                let file_path_for_click = path.clone();
                                let project_for_click = project.clone();
                                let model_for_click = model.clone();

                                Button::new(SharedString::from(format!("stash-file-{path}")))
                                    .accessibility_label(format!("Review stashed file {path}"))
                                    .ghost()
                                    .h_auto()
                                    .w_full()
                                    .p_0()
                                    .on_click(cx.listener(move |_this, _event, _window, cx| {
                                        let Some(proj) = project_for_click.clone() else {
                                            return;
                                        };
                                        let target_path = file_path_for_click.clone();
                                        let diff_project = proj.clone();
                                        let m = model_for_click.clone();
                                        let client = m.read(cx).daemon_client.clone();
                                        cx.spawn(async move |_this, cx| {
                                            let diff_target = target_path.clone();
                                            let content = cx
                                                .background_executor()
                                                .spawn(async move {
                                                    threadlane_ui_state::project_io::diff_stash_file(
                                                        &client,
                                                        &diff_project,
                                                        idx,
                                                        diff_target,
                                                    )
                                                    .await
                                                    .unwrap_or_else(|err| err)
                                                })
                                                .await;
                                            let _ = m.update(cx, |state, cx| {
                                                state.request_open_diff(proj, target_path, content);
                                                cx.notify();
                                            });
                                        })
                                        .detach();
                                    }))
                                    .child(
                                        div()
                                            .w_full()
                                            .whitespace_normal()
                                            .h(rems(1.625))
                                            .px_2()
                                            .rounded_sm()
                                            .flex()
                                            .items_center()
                                            .justify_between()
                                            .gap_2()
                                            .child(
                                                div()
                                                    .flex()
                                                    .items_center()
                                                    .gap_1p5()
                                                    .min_w_0()
                                                    .flex_1()
                                                    .child(
                                                        div()
                                                            .text_xs()
                                                            .font_weight(FontWeight::BOLD)
                                                            .text_color(status_color)
                                                            .child(status),
                                                    )
                                                    .child(
                                                        div()
                                                            .truncate()
                                                            .text_xs()
                                                            .text_color(theme.foreground)
                                                            .child(path),
                                                    ),
                                            )
                                            .child(
                                                div()
                                                    .flex()
                                                    .items_center()
                                                    .gap_1()
                                                    .text_xs()
                                                    .child(
                                                        div()
                                                            .text_color(theme.success)
                                                            .child(format!("+{adds}")),
                                                    )
                                                    .child(
                                                        div()
                                                            .text_color(theme.danger)
                                                            .child(format!("-{dels}")),
                                                    ),
                                            ),
                                    )
                            }))
                            .when(is_loading, |container| {
                                container.child(
                                    div()
                                        .text_xs()
                                        .text_color(theme.muted_foreground)
                                        .child("Loading stashed files…"),
                                )
                            })
                    }))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .justify_end()
                            .gap_2()
                            .pt_1()
                            .child(
                                Button::new("discard-stash-btn")
                                    .label("Discard")
                                    .danger()
                                    .xsmall()
                                    .tooltip("Discard the stashed changes")
                                    .disabled(self.git_busy)
                                    .on_click(cx.listener(move |this, _event, window, cx| {
                                        this.run_git_action(
                                            GitAction::DropStash(Some(idx)),
                                            window,
                                            cx,
                                        );
                                    })),
                            )
                            .child(
                                Button::new("restore-stash-btn")
                                    .label("Restore stash")
                                    .outline()
                                    .xsmall()
                                    .tooltip("Restore the stashed changes")
                                    .disabled(self.git_busy)
                                    .on_click(cx.listener(move |this, _event, window, cx| {
                                        this.run_git_action(
                                            GitAction::PopStash(Some(idx)),
                                            window,
                                            cx,
                                        );
                                    })),
                            ),
                    )
            });

        let changes_active = self.review_tab == ReviewTab::Changes;
        let total_changes = self.review_files.len();

        let staged_in_tab = self.review_files.iter().filter(|f| f.staged).count();
        let changes_label = if total_changes > 0 {
            if staged_in_tab > 0 {
                format!("Changes ({total_changes}, {staged_in_tab} staged)")
            } else {
                format!("Changes ({total_changes})")
            }
        } else {
            "Changes".to_string()
        };
        let commit_count = self
            .git_status
            .as_ref()
            .map(|status| status.recent_commits.len())
            .unwrap_or(0);
        let history_label = if commit_count > 0 {
            format!("History ({commit_count})")
        } else {
            "History".to_string()
        };
        let review_sub_tabs = div()
            .flex_none()
            .border_b_1()
            .border_color(theme.title_bar_border)
            .bg(theme.title_bar)
            .px_3()
            .child(
                TabBar::new("review-sub-tabs")
                    .segmented()
                    .small()
                    .selected_index(if changes_active { 0 } else { 1 })
                    .children(vec![
                        Tab::new().label(changes_label.clone()).aria_label(format!(
                            "Changes, {} files, {} staged",
                            total_changes, staged_in_tab
                        )),
                        Tab::new()
                            .label(history_label.clone())
                            .aria_label(format!("History, {commit_count} recent commits")),
                    ])
                    .on_click(cx.listener(|this, ix, _window, cx| {
                        this.review_tab = if *ix == 0 {
                            ReviewTab::Changes
                        } else {
                            ReviewTab::History
                        };
                        cx.notify();
                    })),
            );

        let review_body = if self.branch_popover_open {
            self.render_branch_manager(cx).into_any_element()
        } else if self.review_tab == ReviewTab::History {
            self.render_history(cx).into_any_element()
        } else {
            div()
                .flex_1()
                .min_h_0()
                .flex()
                .flex_col()
                .children(stash_banner)
                .children(pr_card)
                .children(review_toolbar)
                .child(file_list_content)
                .child(commit_footer)
                .into_any_element()
        };

        div()
            .flex_1()
            .min_h_0()
            .relative()
            .flex()
            .flex_col()
            .child(branch_header)
            .children((!self.branch_popover_open).then(|| review_sub_tabs))
            .child(review_body)
            .into_any_element()
    }

    fn render_history(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().colors;
        let filter_text = self
            .history_filter_input
            .read(cx)
            .value()
            .trim()
            .to_lowercase();
        let commits = self.git_status.as_ref().map(|s| &s.recent_commits);

        let filtered_commits: Vec<&GitCommitInfo> = if let Some(commits) = commits {
            if filter_text.is_empty() {
                commits.iter().collect()
            } else {
                commits
                    .iter()
                    .filter(|c| {
                        c.summary.to_lowercase().contains(&filter_text)
                            || c.author_name.to_lowercase().contains(&filter_text)
                            || c.short_sha.to_lowercase().contains(&filter_text)
                            || c.sha.to_lowercase().contains(&filter_text)
                    })
                    .collect()
            }
        } else {
            Vec::new()
        };

        let commit_list = if filtered_commits.is_empty() {
            div()
                .flex_1()
                .flex()
                .flex_col()
                .items_center()
                .justify_center()
                .p_4()
                .text_center()
                .child(
                    div()
                        .text_sm()
                        .font_weight(FontWeight::MEDIUM)
                        .text_color(theme.foreground)
                        .child("No commits found"),
                )
                .child(
                    div()
                        .mt_1()
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .child(if filter_text.is_empty() {
                            "This branch has no recent commits."
                        } else {
                            "No commits match your filter."
                        }),
                )
                .children((!filter_text.is_empty()).then(|| {
                    Button::new("history-clear-filter")
                        .label("Clear filter")
                        .ghost()
                        .small()
                        .tooltip("Clear the commit filter")
                        .on_click(cx.listener(|this, _event, window, cx| {
                            this.history_filter_input.update(cx, |input, cx| {
                                input.set_value("", window, cx);
                            });
                            cx.notify();
                        }))
                }))
                .into_any_element()
        } else {
            let project = self.project.clone();
            let selected_sha = self.selected_commit_sha.clone();
            let selected_files = self.selected_commit_files.clone();
            let loading_sha = self.loading_commit_sha.clone();
            let model = self.model.clone();
            let event_tx = self.event_tx.clone();

            div()
                .flex_1()
                .min_h_0()
                .overflow_y_scrollbar()
                .py_1()
                .children(filtered_commits.into_iter().map(|commit| {
                    let sha = commit.sha.clone();
                    let short_sha = commit.short_sha.clone();
                    let summary = commit.summary.clone();
                    let author = commit.author_name.clone();
                    let rel_time = commit.relative_time.clone();
                    let is_expanded = selected_sha.as_deref() == Some(&sha);
                    let is_loading = loading_sha.as_deref() == Some(&sha);
                    let click_sha = sha.clone();
                    let click_tx = event_tx.clone();
                    let click_project = project.clone();
                    let click_model = model.clone();

                    div()
                        .id(SharedString::from(format!("commit-{sha}")))
                        .flex()
                        .flex_col()
                        .mx_3()
                        .my_0p5()
                        .rounded_lg()
                        .border_1()
                        .border_color(if is_expanded {
                            theme.primary.opacity(0.6)
                        } else {
                            theme.border
                        })
                        .bg(theme.group_box)
                        .child(
                            Button::new(SharedString::from(format!("commit-header-{sha}")))
                                .accessibility_label(format!(
                                    "Inspect commit {short_sha}: {summary}"
                                ))
                                .ghost()
                                .h_auto()
                                .w_full()
                                .p_0()
                                .on_click(cx.listener(move |this, _event, _window, cx| {
                                    if this.selected_commit_sha.as_deref() == Some(&click_sha) {
                                        this.selected_commit_sha = None;
                                        this.loading_commit_sha = None;
                                        this.selected_commit_files.clear();
                                    } else {
                                        this.selected_commit_sha = Some(click_sha.clone());
                                        this.loading_commit_sha = Some(click_sha.clone());
                                        this.selected_commit_files.clear();
                                        if let Some(proj) = click_project.clone() {
                                            let tx = click_tx.clone();
                                            let fetch_sha = click_sha.clone();
                                            let click_client = click_model
                                                .read(cx)
                                                .daemon_client
                                                .clone();
                                            if let Ok(executor) =
                                                threadlane_ui_state::chat::executor()
                                            {
                                                executor.spawn(async move {
                                                    let files =
                                                        threadlane_ui_state::project_io::commit_files(
                                                            &click_client,
                                                            &proj,
                                                            fetch_sha.clone(),
                                                        )
                                                        .await
                                                        .unwrap_or_default();
                                                    let _ = tx.send(
                                                        PanelEvent::CommitFilesLoaded {
                                                            sha: fetch_sha,
                                                            files,
                                                        },
                                                    );
                                                });
                                            }
                                        }
                                    }
                                    cx.notify();
                                }))
                                .child(
                                    div()
                                        .w_full()
                                        .whitespace_normal()
                                        .p_2p5()
                                        .flex()
                                        .flex_col()
                                        .gap_1()
                                        .child(
                                            div()
                                                .flex()
                                                .items_start()
                                                .justify_between()
                                                .gap_2()
                                                .child(
                                                    div()
                                                        .flex_1()
                                                        .min_w_0()
                                                        .truncate()
                                                        .text_xs()
                                                        .font_weight(FontWeight::SEMIBOLD)
                                                        .text_color(theme.foreground)
                                                        .child(summary),
                                                )
                                                .child(
                                                    div()
                                                        .flex_none()
                                                        .flex_shrink_0()
                                                        .px_1p5()
                                                        .py_0p5()
                                                        .rounded_sm()
                                                        .bg(theme.muted)
                                                        .text_xs()
                                                        .text_color(theme.muted_foreground)
                                                        .child(short_sha.clone()),
                                                ),
                                        )
                                        .child(
                                            div()
                                                .flex()
                                                .items_center()
                                                .gap_1p5()
                                                .text_xs()
                                                .text_color(theme.muted_foreground)
                                                .child(
                                                    div()
                                                        .size_3()
                                                        .flex()
                                                        .items_center()
                                                        .justify_center()
                                                        .child(IconName::User),
                                                )
                                                .child(format!("{author} • {rel_time}")),
                                        ),
                                ),
                        )
                        .children(is_expanded.then(|| {
                            let commit_files = selected_files.clone();
                            let commit_sha = sha.clone();
                            let short_sha_disp = short_sha.clone();
                            let proj_for_diff = project.clone();
                            let model_ref = model.clone();

                            div()
                                .border_t_1()
                                .border_color(theme.border)
                                .bg(theme.background)
                                .p_2()
                                .flex()
                                .flex_col()
                                .gap_1()
                                .children(is_loading.then(|| {
                                    div()
                                        .p_2()
                                        .flex()
                                        .items_center()
                                        .gap_2()
                                        .text_xs()
                                        .text_color(theme.muted_foreground)
                                        .child(Spinner::new().xsmall())
                                        .child("Loading changed files…")
                                }))
                                .children((!is_loading && commit_files.is_empty()).then(|| {
                                    div()
                                        .p_2()
                                        .text_xs()
                                        .text_color(theme.muted_foreground)
                                        .child("No files changed in this commit.")
                                }))
                                .children(commit_files.into_iter().map(|file| {
                                    let path = file.path.clone();
                                    let status = file.status_char().to_string();
                                    let status_color = match file.status_char() {
                                        'A' | '?' => theme.success,
                                        'D' => theme.danger,
                                        _ => theme.warning,
                                    };
                                    let target_path = path.clone();
                                    let diff_sha = commit_sha.clone();
                                    let disp_sha = short_sha_disp.clone();
                                    let diff_proj = proj_for_diff.clone();
                                    let m = model_ref.clone();

                                    Button::new(SharedString::from(format!(
                                        "commit-file-{commit_sha}-{path}"
                                    )))
                                    .accessibility_label(format!(
                                        "Review {path} in commit {commit_sha}"
                                    ))
                                    .ghost()
                                    .h_auto()
                                    .w_full()
                                    .p_0()
                                    .on_click(cx.listener(move |_this, _event, _window, cx| {
                                        let Some(proj) = diff_proj.clone() else {
                                            return;
                                        };
                                        let p = proj.clone();
                                        let target = target_path.clone();
                                        let sha_str = diff_sha.clone();
                                        let label = format!("{target} @ {disp_sha}");
                                        let state_model = m.clone();
                                        let client = m.read(cx).daemon_client.clone();
                                        cx.spawn(async move |_this, cx| {
                                            let content = cx
                                                .background_executor()
                                                .spawn(async move {
                                                    threadlane_ui_state::project_io::diff_commit_file(
                                                        &client, &p, sha_str, target,
                                                    )
                                                    .await
                                                    .unwrap_or_else(|e| e)
                                                })
                                                .await;
                                            let _ = state_model.update(cx, |state, cx| {
                                                state.request_open_diff(proj, label, content);
                                                cx.notify();
                                            });
                                        })
                                        .detach();
                                    }))
                                    .child(
                                        div()
                                            .w_full()
                                            .whitespace_normal()
                                            .h(rems(1.625))
                                            .px_2()
                                            .rounded_md()
                                            .flex()
                                            .items_center()
                                            .justify_between()
                                            .gap_2()
                                            .child(
                                                div()
                                                    .flex_1()
                                                    .min_w_0()
                                                    .flex()
                                                    .items_center()
                                                    .gap_1p5()
                                                    .child(
                                                        div()
                                                            .size_3()
                                                            .text_color(theme.muted_foreground)
                                                            .child(IconName::File),
                                                    )
                                                    .child(
                                                        div()
                                                            .truncate()
                                                            .text_xs()
                                                            .text_color(theme.foreground)
                                                            .child(path),
                                                    ),
                                            )
                                            .child(
                                                div()
                                                    .flex()
                                                    .items_center()
                                                    .gap_1p5()
                                                    .when(file.additions > 0, |r| {
                                                        r.child(
                                                            div()
                                                                .text_xs()
                                                                .text_color(theme.success)
                                                                .child(format!(
                                                                    "+{}",
                                                                    file.additions
                                                                )),
                                                        )
                                                    })
                                                    .when(file.deletions > 0, |r| {
                                                        r.child(
                                                            div()
                                                                .text_xs()
                                                                .text_color(theme.danger)
                                                                .child(format!(
                                                                    "-{}",
                                                                    file.deletions
                                                                )),
                                                        )
                                                    })
                                                    .child(
                                                        div()
                                                            .size(rems(0.875))
                                                            .rounded_sm()
                                                            .flex()
                                                            .items_center()
                                                            .justify_center()
                                                            .text_xs()
                                                            .font_weight(FontWeight::BOLD)
                                                            .text_color(status_color)
                                                            .child(status),
                                                    ),
                                            ),
                                    )
                                }))
                        }))
                }))
                .into_any_element()
        };

        div()
            .flex_1()
            .min_h_0()
            .flex()
            .flex_col()
            .child(
                div()
                    .px_3()
                    .py_2()
                    .border_b_1()
                    .border_color(theme.border)
                    .bg(theme.title_bar)
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .px_2()
                            .py_1()
                            .rounded_md()
                            .border_1()
                            .border_color(theme.border)
                            .bg(theme.input)
                            .child(
                                div()
                                    .size(rems(0.875))
                                    .text_color(theme.muted_foreground)
                                    .child(IconName::Search),
                            )
                            .child(
                                div().flex_1().child(
                                    Input::new(&self.history_filter_input)
                                        .aria_label("Filter commits")
                                        .appearance(false)
                                        .bordered(false),
                                ),
                            ),
                    ),
            )
            .child(commit_list)
    }

    fn render_branch_manager(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().colors;
        let filter_text = self
            .branch_filter_input
            .read(cx)
            .value()
            .trim()
            .to_lowercase();
        let current_branch = self
            .git_status
            .as_ref()
            .and_then(|s| s.branch.as_deref())
            .unwrap_or("main");

        let branch_details = self.git_status.as_ref().map(|s| &s.branch_details);
        let default_branch_name = self
            .git_status
            .as_ref()
            .and_then(|s| s.default_branch.as_deref())
            .unwrap_or("main");

        let all_branches: Vec<GitBranchInfo> = if let Some(details) = branch_details {
            details.clone()
        } else if let Some(status) = &self.git_status {
            status
                .branches
                .iter()
                .filter(|b| b.as_str() != "origin" && !b.ends_with("/HEAD"))
                .map(|b| GitBranchInfo {
                    name: b.clone(),
                    is_current: b == current_branch,
                    is_default: b == default_branch_name,
                    is_remote: b.starts_with("origin/"),
                    relative_time: String::new(),
                    committer_date_unix: 0,
                    upstream: None,
                })
                .collect()
        } else {
            Vec::new()
        };

        let filtered_branches: Vec<GitBranchInfo> = all_branches
            .into_iter()
            .filter(|b| {
                b.name != "origin"
                    && !b.name.ends_with("/HEAD")
                    && (filter_text.is_empty() || b.name.to_lowercase().contains(&filter_text))
            })
            .collect();

        let default_branches: Vec<GitBranchInfo> = filtered_branches
            .iter()
            .filter(|b| b.is_default && !b.is_remote)
            .cloned()
            .collect();

        let recent_branches: Vec<GitBranchInfo> = filtered_branches
            .iter()
            .filter(|b| !b.is_default && !b.is_remote)
            .cloned()
            .collect();

        let other_branches: Vec<GitBranchInfo> = filtered_branches
            .iter()
            .filter(|b| b.is_remote)
            .cloned()
            .collect();

        let current_branch_str = current_branch.to_string();

        div()
            .id("git-branch-manager")
            .flex_1()
            .min_h_0()
            .flex()
            .flex_col()
            .bg(theme.title_bar)
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .p_3()
                    .border_b_1()
                    .border_color(theme.border)
                    .child(
                        div()
                            .flex_1()
                            .flex()
                            .items_center()
                            .gap_1p5()
                            .px_2()
                            .h_8()
                            .rounded_md()
                            .border_1()
                            .border_color(theme.border)
                            .bg(theme.background)
                            .child(
                                div()
                                    .size(rems(0.875))
                                    .text_color(theme.muted_foreground)
                                    .child(IconName::Search),
                            )
                            .child(
                                div().flex_1().child(
                                    Input::new(&self.branch_filter_input).aria_label("Filter branches")
                                        .appearance(false)
                                        .bordered(false),
                                ),
                            ),
                    )
                    .child(
                        Button::new("open-new-branch-modal-btn")
                            .icon(IconName::Plus)
                            .label("New branch…")
                            .outline()
                            .small()
                            .tooltip("Create a new branch…")
                            .on_click(cx.listener(|this, _event, window, cx| {
                                this.open_new_branch_dialog(window, cx);
                            })),
                    )
                    .child(
                        Button::new("close-branch-manager-btn")
                                    .accessibility_label("Back to review")
                            .icon(IconName::Close)
                            .ghost()
                            .small()
                            .tooltip("Back to review")
                            .on_click(cx.listener(|this, _event, _window, cx| {
                                this.branch_popover_open = false;
                                cx.notify();
                            })),
                    ),
            )
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scrollbar()
                    .p_3()
                    .flex()
                    .flex_col()
                    .gap_3()
                    .child(
                        Button::new("quick-merge-banner")
                            .accessibility_label("Merge a branch…")
                            .ghost().h_auto().w_full().p_0()
                            .on_click(cx.listener(|this, _event, window, cx| {
                                this.open_merge_dialog(window, cx);
                            }))
                            .child(div().w_full().whitespace_normal()
                            .flex()
                            .items_center()
                            .justify_between()
                            .px_2p5()
                            .py_2()
                            .rounded_md()
                            .border_1()
                            .border_color(theme.border)
                            .bg(theme.muted.opacity(0.35))
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap_2()
                                    .child(
                                        div()
                                            .size_4()
                                            .text_color(theme.primary)
                                            .child(Icon::default().path("icons/git/branch.svg")),
                                    )
                                    .child(
                                        div()
                                            .text_xs()
                                            .font_weight(FontWeight::MEDIUM)
                                            .text_color(theme.foreground)
                                            .child(format!("Choose a branch to merge into {current_branch_str}…")),
                                    ),
                            )
                            .child(
                                div()
                                    .size(rems(0.875))
                                    .text_color(theme.muted_foreground)
                                    .child(IconName::ChevronRight),
                            )),
                    )
                    .when(!default_branches.is_empty(), |el| {
                        el.child(self.render_branch_section("DEFAULT BRANCH", default_branches, cx))
                    })
                    .when(!recent_branches.is_empty(), |el| {
                        el.child(self.render_branch_section("RECENT BRANCHES", recent_branches, cx))
                    })
                    .when(!other_branches.is_empty(), |el| {
                        el.child(self.render_branch_section("OTHER BRANCHES", other_branches, cx))
                    }),
            )
    }

    fn can_delete_branch(&self, project: &Path, branch: &str) -> bool {
        !self.git_busy
            && self.project.as_deref() == Some(project)
            && self.git_status.as_ref().is_some_and(|status| {
                status.branch.as_deref() != Some(branch)
                    && status.default_branch.as_deref() != Some(branch)
                    && status.branch_details.iter().any(|info| {
                        info.name == branch && !info.is_current && !info.is_default && !info.is_remote
                    })
            })
    }

    fn confirm_delete_branch(
        &mut self,
        project: PathBuf,
        branch: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.can_delete_branch(&project, &branch) {
            return;
        }
        let panel = cx.entity().downgrade();
        window.open_alert_dialog(cx, move |alert, _, _| {
            let panel = panel.clone();
            let project = project.clone();
            let branch = branch.clone();
            alert
                .title(format!("Delete branch “{branch}”?"))
                .description(format!(
                    "Delete the local branch in {}. Remote branches and worktrees will not be removed. Unmerged branches and branches checked out in a worktree cannot be deleted.",
                    project.display()
                ))
                .button_props(DialogButtonProps::default()
                    .ok_text("Delete")
                    .ok_variant(ButtonVariant::Danger)
                    .show_cancel(true))
                .on_ok(move |_, window, cx| {
                    let _ = panel.update(cx, |panel, cx| {
                        // Recheck after confirmation: the panel may now show another project.
                        if panel.can_delete_branch(&project, &branch) {
                            panel.run_git_action(GitAction::DeleteBranch(branch.clone()), window, cx);
                        }
                    });
                    true
                })
        });
    }

    fn render_branch_section(
        &self,
        title: &'static str,
        branches: Vec<GitBranchInfo>,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let theme = cx.theme().colors;
        div()
            .flex()
            .flex_col()
            .gap_1()
            .child(
                div()
                    .text_xs()
                    .font_weight(FontWeight::BOLD)
                    .text_color(theme.muted_foreground)
                    .px_1()
                    .pb_0p5()
                    .child(title),
            )
            .children(branches.into_iter().map(|branch| {
                let name = branch.name.clone();
                let is_current = branch.is_current;
                let rel_time = branch.relative_time.clone();
                let branch_name_for_click = name.clone();
                let menu_name = name.clone();
                let panel = cx.entity().downgrade();
                let project = self.project.clone();
                Button::new(SharedString::from(format!("branch-row-{}", name)))
                    .debug_selector({
                        let name = name.clone();
                        move || format!("branch-row-{name}")
                    })
                    .accessibility_label(if is_current {
                        format!("Current branch {name}, already checked out")
                    } else {
                        format!("Switch to branch {name}")
                    })
                    .ghost()
                    .h_auto()
                    .w_full()
                    .p_0()
                    .on_click(cx.listener(move |this, _event, window, cx| {
                        if !is_current {
                            let has_dirty = this
                                .git_status
                                .as_ref()
                                .map_or(false, |s| !s.files.is_empty());
                            if has_dirty {
                                this.switch_target_branch = Some(branch_name_for_click.clone());
                                this.switch_dialog_open = true;
                                this.switch_stash_mode = true;
                                cx.notify();
                            } else {
                                this.run_git_action(
                                    GitAction::Checkout(branch_name_for_click.clone()),
                                    window,
                                    cx,
                                );
                                this.branch_popover_open = false;
                            }
                        }
                    }))
                    .child(
                        div()
                            .w_full()
                            .whitespace_normal()
                            .flex()
                            .items_center()
                            .justify_between()
                            .gap_2()
                            .px_2p5()
                            .py_2()
                            .rounded_md()
                            .bg(if is_current {
                                theme.muted.opacity(0.7)
                            } else {
                                gpui::transparent_black()
                            })
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap_2()
                                    .min_w_0()
                                    .flex_1()
                                    .child(
                                        div()
                                            .size_4()
                                            .flex()
                                            .items_center()
                                            .justify_center()
                                            .text_color(if is_current {
                                                theme.primary
                                            } else {
                                                theme.muted_foreground
                                            })
                                            .child(if is_current {
                                                Icon::new(IconName::Check)
                                            } else {
                                                Icon::default().path("icons/git/branch.svg")
                                            }),
                                    )
                                    .child(
                                        div()
                                            .truncate()
                                            .text_xs()
                                            .font_weight(if is_current {
                                                FontWeight::BOLD
                                            } else {
                                                FontWeight::MEDIUM
                                            })
                                            .text_color(if is_current {
                                                theme.foreground
                                            } else {
                                                theme.foreground.opacity(0.9)
                                            })
                                            .child(name),
                                    )
                                    .children(is_current.then(|| {
                                        Tag::new()
                                            .child("current")
                                            .with_variant(TagVariant::Info)
                                            .small()
                                    })),
                            )
                            .children((!rel_time.is_empty()).then(|| {
                                div()
                                    .text_xs()
                                    .text_color(theme.muted_foreground)
                                    .child(rel_time)
                            })),
                    )
                    .context_menu(move |menu, _, cx| {
                        let copy_name = menu_name.clone();
                        let delete_name = menu_name.clone();
                        let delete_panel = panel.clone();
                        let delete_project = project.clone();
                        let can_delete = panel.upgrade().is_some_and(|panel| {
                            project.as_deref().is_some_and(|project| {
                                panel.read(cx).can_delete_branch(project, &menu_name)
                            })
                        });
                        menu.item(PopupMenuItem::new("Copy branch name").on_click(move |_, _, cx| {
                            cx.write_to_clipboard(ClipboardItem::new_string(copy_name.clone()));
                        }))
                        .separator()
                        .item(PopupMenuItem::new("Delete branch…")
                            .disabled(!can_delete)
                            .on_click(move |_, window, cx| {
                                if let Some(project) = delete_project.clone() {
                                    let _ = delete_panel.update(cx, |panel, cx| {
                                        panel.confirm_delete_branch(project, delete_name.clone(), window, cx);
                                    });
                                }
                            }))
                    })
            }))
    }

    fn close_all_git_dialogs(&mut self) {
        self.new_branch_dialog_open = false;
        self.merge_dialog_open = false;
        self.merge_selected_branch = None;
        self.switch_dialog_open = false;
        self.switch_target_branch = None;
        self.stash_dialog_open = false;
    }

    pub fn sync_git_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let open = self.new_branch_dialog_open
            || self.merge_dialog_open
            || self.switch_dialog_open
            || self.stash_dialog_open;
        if open == self.git_dialog_presented {
            return;
        }
        self.git_dialog_presented = open;
        if !open {
            window.close_dialog(cx);
            return;
        }
        let panel = cx.entity().downgrade();
        let width = if self.new_branch_dialog_open || self.stash_dialog_open {
            26.25
        } else {
            28.75
        };
        let title = if self.new_branch_dialog_open {
            "Create a branch".to_string()
        } else if self.merge_dialog_open {
            "Merge branches".to_string()
        } else if self.stash_dialog_open {
            "Stash changes".to_string()
        } else {
            format!(
                "Switch to {}",
                self.switch_target_branch.as_deref().unwrap_or("main")
            )
        };
        window.open_dialog(cx, move |dialog, window, cx| {
            let close_panel = panel.clone();
            let content = panel
                .update(cx, |panel, cx| panel.render_git_dialog_layer(cx))
                .ok()
                .flatten();
            dialog
                .w(window.rem_size() * width)
                .title(title.clone())
                .children(content)
                .on_ok(|_, _, _| false)
                .on_close(move |_, _, cx| {
                    let _ = close_panel.update(cx, |panel, cx| {
                        panel.git_dialog_presented = false;
                        panel.close_all_git_dialogs();
                        cx.notify();
                    });
                })
        });
    }

    fn render_git_dialog_layer(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        if self.new_branch_dialog_open {
            Some(self.render_new_branch_dialog(cx).into_any_element())
        } else if self.merge_dialog_open {
            Some(self.render_merge_dialog(cx).into_any_element())
        } else if self.switch_dialog_open {
            Some(self.render_switch_branch_dialog(cx).into_any_element())
        } else if self.stash_dialog_open {
            Some(self.render_stash_dialog(cx).into_any_element())
        } else {
            None
        }
    }

    fn render_stash_dialog(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().colors;
        let include_untracked = self.stash_include_untracked;

        div()
            .id("stash-dialog")
            .w_full()
            .flex()
            .flex_col()
            .gap_3()
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(
                        div()
                            .text_xs()
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(theme.foreground)
                            .child("Stash message (optional)"),
                    )
                    .child(
                        div()
                            .px_2()
                            .py_1p5()
                            .rounded_md()
                            .bg(theme.input)
                            .border_1()
                            .border_color(theme.border)
                            .child(
                                Input::new(&self.stash_message_input).aria_label("Stash message"),
                            ),
                    ),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(
                        Checkbox::new("stash-include-untracked-chk")
                            .checked(include_untracked)
                            .on_click(cx.listener(|this, checked, _window, cx| {
                                this.stash_include_untracked = *checked;
                                cx.notify();
                            })),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(theme.foreground)
                            .child("Include untracked files"),
                    ),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_end()
                    .gap_2()
                    .pt_2()
                    .child(
                        Button::new("cancel-stash-btn")
                            .label("Cancel")
                            .ghost()
                            .small()
                            .on_click(cx.listener(|this, _event, _window, cx| {
                                this.close_all_git_dialogs();
                                cx.notify();
                            })),
                    )
                    .child(
                        Button::new("confirm-stash-btn")
                            .label("Stash changes")
                            .primary()
                            .small()
                            .disabled(self.git_busy)
                            .on_click(cx.listener(move |this, _event, window, cx| {
                                let message =
                                    this.stash_message_input.read(cx).value().trim().to_string();
                                let msg_opt = (!message.is_empty()).then_some(message);
                                let include_untracked = this.stash_include_untracked;
                                this.run_git_action(
                                    GitAction::StashPush {
                                        message: msg_opt,
                                        include_untracked,
                                    },
                                    window,
                                    cx,
                                );
                                this.close_all_git_dialogs();
                            })),
                    ),
            )
    }

    fn render_new_branch_dialog(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().colors;
        let current_branch = self
            .git_status
            .as_ref()
            .and_then(|s| s.branch.as_deref())
            .unwrap_or("main")
            .to_string();
        let name = self
            .new_branch_name_input
            .read(cx)
            .value()
            .trim()
            .to_string();
        let can_create = !name.is_empty() && !self.git_busy;

        div()
            .id("new-branch-dialog")
            .w_full()
            .flex()
            .flex_col()
            .gap_3()
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_1p5()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child("Based on")
                    .child(
                        Tag::new()
                            .child(current_branch)
                            .with_variant(TagVariant::Secondary)
                            .small(),
                    ),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(
                        div()
                            .text_xs()
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(theme.foreground)
                            .child("Branch name"),
                    )
                    .child(
                        div()
                            .px_2()
                            .py_1()
                            .rounded_md()
                            .border_1()
                            .border_color(theme.border)
                            .bg(theme.background)
                            .child(
                                Input::new(&self.new_branch_name_input)
                                    .aria_label("New branch name")
                                    .bordered(false),
                            ),
                    ),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_end()
                    .gap_2()
                    .pt_2()
                    .child(
                        Button::new("cancel-new-branch-btn")
                            .label("Cancel")
                            .ghost()
                            .small()
                            // Synara busy guard: block dismiss while Git is running.
                            .disabled(self.git_busy)
                            .on_click(cx.listener(|this, _event, _window, cx| {
                                if this.git_busy {
                                    return;
                                }
                                this.close_all_git_dialogs();
                                cx.notify();
                            })),
                    )
                    .child(
                        Button::new("submit-new-branch-btn")
                            .label(if name.is_empty() {
                                "Create branch".to_string()
                            } else {
                                format!("Create {name}")
                            })
                            .accessibility_label(if name.is_empty() {
                                "Create branch".to_string()
                            } else {
                                format!("Create branch {name}")
                            })
                            .tooltip(if name.is_empty() {
                                "Enter a branch name".to_string()
                            } else {
                                format!("Create branch {name}")
                            })
                            .primary()
                            .small()
                            .disabled(!can_create)
                            .on_click(cx.listener(move |this, _event, window, cx| {
                                let name = this
                                    .new_branch_name_input
                                    .read(cx)
                                    .value()
                                    .trim()
                                    .to_string();
                                if !name.is_empty() {
                                    this.run_git_action(GitAction::CreateBranch(name), window, cx);
                                    this.close_all_git_dialogs();
                                }
                            })),
                    ),
            )
    }

    fn render_merge_dialog(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().colors;
        let current_branch = self
            .git_status
            .as_ref()
            .and_then(|s| s.branch.as_deref())
            .unwrap_or("main")
            .to_string();
        let filter = self
            .merge_filter_input
            .read(cx)
            .value()
            .trim()
            .to_lowercase();

        let branch_details = self.git_status.as_ref().map(|s| &s.branch_details);
        let branches: Vec<GitBranchInfo> = if let Some(details) = branch_details {
            details
                .iter()
                .filter(|b| {
                    b.name != "origin"
                        && !b.name.ends_with("/HEAD")
                        && b.name != current_branch
                        && (filter.is_empty() || b.name.to_lowercase().contains(&filter))
                })
                .cloned()
                .collect()
        } else if let Some(status) = &self.git_status {
            status
                .branches
                .iter()
                .filter(|b| {
                    b.as_str() != "origin"
                        && !b.ends_with("/HEAD")
                        && b.as_str() != current_branch.as_str()
                        && (filter.is_empty() || b.to_lowercase().contains(&filter))
                })
                .map(|b| GitBranchInfo {
                    name: b.clone(),
                    is_current: false,
                    is_default: false,
                    is_remote: b.starts_with("origin/"),
                    relative_time: String::new(),
                    committer_date_unix: 0,
                    upstream: None,
                })
                .collect()
        } else {
            Vec::new()
        };

        let selected = self.merge_selected_branch.clone();
        let can_merge = selected.is_some() && !self.git_busy;

        div()
            .id("merge-branch-dialog")
            .w_full()
            .max_h(rems(32.5))
            .flex()
            .flex_col()
            .gap_3()
            .child(
                div()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child("Select a branch to merge into your current working tree:"),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_1p5()
                    .px_2()
                    .h_8()
                    .rounded_md()
                    .border_1()
                    .border_color(theme.border)
                    .bg(theme.background)
                    .child(
                        div()
                            .size(rems(0.875))
                            .text_color(theme.muted_foreground)
                            .child(IconName::Search),
                    )
                    .child(
                        div().flex_1().child(
                            Input::new(&self.merge_filter_input)
                                .aria_label("Filter branches to merge")
                                .appearance(false)
                                .bordered(false),
                        ),
                    ),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .max_h(rems(15.0))
                    .overflow_y_scrollbar()
                    .gap_1()
                    .children(branches.into_iter().map(|b| {
                        let name = b.name.clone();
                        let is_selected = selected.as_deref() == Some(&name);
                        let name_for_click = name.clone();
                        Button::new(SharedString::from(format!("merge-select-{}", name)))
                            .accessibility_label(format!("Select branch {name} to merge"))
                            .toggled(is_selected)
                            .ghost()
                            .h_auto()
                            .w_full()
                            .p_0()
                            .on_click(cx.listener(move |this, _event, _window, cx| {
                                this.merge_selected_branch = Some(name_for_click.clone());
                                cx.notify();
                            }))
                            .child(
                                div()
                                    .w_full()
                                    .whitespace_normal()
                                    .flex()
                                    .items_center()
                                    .justify_between()
                                    .gap_2()
                                    .px_2p5()
                                    .py_2()
                                    .rounded_md()
                                    .border_1()
                                    .border_color(if is_selected {
                                        theme.primary
                                    } else {
                                        gpui::transparent_black()
                                    })
                                    .bg(if is_selected {
                                        theme.muted.opacity(0.8)
                                    } else {
                                        gpui::transparent_black()
                                    })
                                    .child(
                                        div()
                                            .flex()
                                            .items_center()
                                            .gap_2()
                                            .child(
                                                div()
                                                    .size_4()
                                                    .flex()
                                                    .items_center()
                                                    .justify_center()
                                                    .text_color(if is_selected {
                                                        theme.primary
                                                    } else {
                                                        theme.muted_foreground
                                                    })
                                                    .child(
                                                        Icon::default()
                                                            .path("icons/git/branch.svg"),
                                                    ),
                                            )
                                            .child(
                                                div()
                                                    .text_xs()
                                                    .font_weight(if is_selected {
                                                        FontWeight::BOLD
                                                    } else {
                                                        FontWeight::NORMAL
                                                    })
                                                    .text_color(theme.foreground)
                                                    .child(name),
                                            ),
                                    )
                                    .children((!b.relative_time.is_empty()).then(|| {
                                        div()
                                            .text_xs()
                                            .text_color(theme.muted_foreground)
                                            .child(b.relative_time)
                                    })),
                            )
                    })),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_end()
                    .gap_2()
                    .pt_2()
                    .border_t_1()
                    .border_color(theme.border)
                    .child(
                        Button::new("cancel-merge-btn")
                            .label("Cancel")
                            .ghost()
                            .small()
                            .disabled(self.git_busy)
                            .on_click(cx.listener(|this, _event, _window, cx| {
                                if this.git_busy {
                                    return;
                                }
                                this.close_all_git_dialogs();
                                cx.notify();
                            })),
                    )
                    .child(
                        Button::new("submit-merge-btn")
                            .label(if let Some(target) = &selected {
                                format!("Merge {target} into {current_branch}")
                            } else {
                                format!("Merge into {current_branch}")
                            })
                            .primary()
                            .small()
                            .disabled(!can_merge)
                            .on_click(cx.listener(move |this, _event, window, cx| {
                                if let Some(branch_to_merge) = this.merge_selected_branch.clone() {
                                    this.run_git_action(
                                        GitAction::Merge(branch_to_merge),
                                        window,
                                        cx,
                                    );
                                    this.close_all_git_dialogs();
                                }
                            })),
                    ),
            )
    }

    fn render_switch_branch_dialog(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().colors;
        let current_branch = self
            .git_status
            .as_ref()
            .and_then(|s| s.branch.as_deref())
            .unwrap_or("main")
            .to_string();
        let target_branch = self
            .switch_target_branch
            .clone()
            .unwrap_or_else(|| "main".to_string());
        let is_stash = self.switch_stash_mode;

        div()
                    .id("switch-branch-dialog")
                    .w_full()






                    .flex()
                    .flex_col()
                    .gap_3p5()


                    .child(
                        div()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child(format!("You have uncommitted changes on {current_branch}. What would you like to do with them?")),
                    )
                    .child(
                        RadioGroup::vertical("switch-stash-mode")
                            .selected_index(Some(if is_stash { 0 } else { 1 }))
                            .child(
                                Radio::new("switch-opt-stash")
                                    .label(format!("Leave my changes on {current_branch} (Stash)"))
                                    .accessibility_label("Leave changes on this branch using a stash")
                                    .child(
                                        div()
                                            .text_xs()
                                            .text_color(theme.muted_foreground)
                                            .child("Your in-progress changes will be stashed and restored when you switch back."),
                                    ),
                            )
                            .child(
                                Radio::new("switch-opt-carry")
                                    .label(format!("Bring my changes to {target_branch}"))
                                    .accessibility_label("Carry changes to the selected branch")
                                    .child(
                                        div()
                                            .text_xs()
                                            .text_color(theme.muted_foreground)
                                            .child(format!("Your in-progress changes will be carried over to {target_branch}.")),
                                    ),
                            )
                            .on_click(cx.listener(|this, selected: &usize, _window, cx| {
                                this.switch_stash_mode = *selected == 0;
                                cx.notify();
                            })),
                    )
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .justify_end()
                            .gap_2()
                            .pt_2()
                            .child(
                                Button::new("cancel-switch-dialog-btn")
                                    .label("Cancel")
                                    .ghost()
                                    .small()
                                    .disabled(self.git_busy)
                                    .on_click(cx.listener(|this, _event, _window, cx| {
                                        if this.git_busy {
                                            return;
                                        }
                                        this.close_all_git_dialogs();
                                        cx.notify();
                                    })),
                            )
                            .child(
                                Button::new("submit-switch-dialog-btn")
                                    .label(format!("Switch to {target_branch}"))
                                    .accessibility_label(format!(
                                        "Switch to branch {target_branch}"
                                    ))
                                    .tooltip(format!("Check out {target_branch}"))
                                    .primary()
                                    .small()
                                    .disabled(self.git_busy)
                                    .on_click(cx.listener(move |this, _event, window, cx| {
                                        let target = this.switch_target_branch.clone().unwrap_or_else(|| "main".to_string());
                                        if this.switch_stash_mode {
                                            this.run_git_action(GitAction::CheckoutStash(target), window, cx);
                                        } else {
                                            this.run_git_action(GitAction::CheckoutCarry(target), window, cx);
                                        }
                                        this.close_all_git_dialogs();
                                        this.branch_popover_open = false;
                                    })),
                            ),
                    )
    }

    fn render_review_error(&self, error: &str, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme().colors;
        let details = error.to_owned();
        div()
            .flex_1()
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .gap_2()
            .p_6()
            .child(
                div()
                    .text_sm()
                    .font_weight(FontWeight::MEDIUM)
                    .text_color(theme.foreground)
                    .child("Couldn't load Git status."),
            )
            .child(
                div()
                    .max_w(rems(24.0))
                    .text_center()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child(error.to_owned()),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(
                        Button::new("review-error-retry")
                            .label("Retry")
                            .small()
                            .tooltip("Reload Git status")
                            .on_click(cx.listener(|this, _event, _window, cx| {
                                this.refresh_active_surface(cx);
                                cx.notify();
                            })),
                    )
                    .child(
                        Button::new("review-error-copy")
                            .label("Copy details")
                            .ghost()
                            .small()
                            .tooltip("Copy full error to clipboard")
                            .on_click(move |_event, window, cx| {
                                cx.write_to_clipboard(ClipboardItem::new_string(details.clone()));
                                window.push_notification(
                                    Notification::info("Copied error details"),
                                    cx,
                                );
                            }),
                    ),
            )
            .into_any_element()
    }

    fn render_empty(&self, title: &str, description: &str, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme().colors;
        div()
            .flex_1()
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .gap_3()
            .child(
                div()
                    .text_sm()
                    .font_weight(FontWeight::MEDIUM)
                    .child(title.to_string()),
            )
            .child(
                div()
                    .mt_1()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child(description.to_string()),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(
                        Button::new("right-panel-recreate-worktree")
                            .label("Recreate worktree")
                            .small()
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.model.update(cx, |state, cx| {
                                    threadlane_ui_state::controller::dispatch(
                                        state,
                                        threadlane_ui_state::actions::AppAction::RecreateActiveWorktree,
                                    );
                                    cx.notify();
                                });
                            })),
                    )
                    .child(
                        Button::new("right-panel-use-local")
                            .label("Use project folder")
                            .small()
                            .ghost()
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.model.update(cx, |state, cx| {
                                    if let Some(work_dir) = state.active_work_dir.clone() {
                                        threadlane_ui_state::controller::dispatch(
                                            state,
                                            threadlane_ui_state::actions::AppAction::SelectDraftProject(work_dir),
                                        );
                                    }
                                    cx.notify();
                                });
                            })),
                    ),
            )
            .into_any_element()
    }
}

impl Render for RightPanelView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.sync_project(cx);
        for notification in self.pending_git_notifications.drain(..) {
            window.push_notification(notification, cx);
        }
        if let Some(message) = self.generated_commit_message.take() {
            self.commit_message_input
                .update(cx, |input, cx| input.set_value(message, window, cx));
        }
        if self.should_clear_commit_message {
            self.should_clear_commit_message = false;
            self.commit_message_input
                .update(cx, |input, cx| input.set_value("", window, cx));
        }
        self.sync_pending_document(window, cx);
        let theme = cx.theme().colors;
        let unavailable_non_browser = self.worktree_unavailable
            && !matches!(
                self.active_surface,
                Some(Surface::Browser | Surface::Agents | Surface::Trajectory)
            );
        let body = if unavailable_non_browser {
            self.render_empty(
                "Worktree unavailable",
                "This worktree is not checked out",
                cx,
            )
        } else {
            match self.active_surface {
                None => self.render_chooser(cx).into_any_element(),
                Some(Surface::Trajectory) => self
                    .trajectory_view
                    .clone()
                    .map(|view| view.into_any_element())
                    .unwrap_or_else(|| {
                        self.render_empty("Trajectory", "Trajectory is unavailable", cx)
                    }),
                Some(Surface::Agents) => self.agents.clone().into_any_element(),
                Some(Surface::Review) if self.document_title.is_some() => self.render_files(cx),
                Some(Surface::Review) => self.render_review(window, cx),
                Some(Surface::Files) => self.render_files(cx),
                Some(Surface::Browser) => self.render_browser(window, cx),
            }
        };
        div()
            .w_full()
            .h_full()
            .min_w_0()
            .flex()
            .flex_col()
            .bg(theme.background)
            .child(self.render_header(cx))
            .when(self.active_surface == Some(Surface::Review), |panel| {
                panel.child(self.render_workspace_context(cx))
            })
            .child(body)
    }
}

/// One bridge-pump step: either a finished reply or a pending script
/// evaluation/snapshot/wait whose channel the pump awaits without blocking the UI.
enum BrowserReply {
    Ready(Result<String, String>),
    PendingEval(tokio::sync::oneshot::Receiver<String>),
    PendingSnapshot(tokio::sync::oneshot::Receiver<Result<(Vec<u8>, u32, u32), String>>),
    PendingWait {
        selector: Option<String>,
        text: Option<String>,
        deadline: std::time::Instant,
    },
}

/// Cap for evaluated script results. Snapshot JSON keeps url/title/count up
/// front so a cut tail still orients the model.
const MAX_BROWSER_EVAL_CHARS: usize = 8_000;

/// Whether a browser command should force the panel open on the Browser
/// surface (commands that visibly change the page).
// Keep this aligned with the surface switches in the browser command handlers.
fn browser_command_reveals_surface(command: &threadlane_protocol::browser::BrowserCommand) -> bool {
    use threadlane_protocol::browser::{BrowserCommand, BrowserTabAction};
    matches!(command,
        BrowserCommand::Tabs { action: BrowserTabAction::Open { .. } | BrowserTabAction::Select { .. } | BrowserTabAction::Close { .. } }
        | BrowserCommand::Navigate { .. }
        | BrowserCommand::Back
        | BrowserCommand::Screenshot
        | BrowserCommand::Wait { .. }
    )
}

/// Dispatches a `BrowserCommand` from the agent bridge: immediate replies,
/// or a pending eval/snapshot/wait the pump resolves later.
fn start_browser_request(
    panel: &mut RightPanelView,
    command: threadlane_protocol::browser::BrowserCommand,
    window: &mut Window,
    cx: &mut Context<RightPanelView>,
) -> BrowserReply {
    use threadlane_protocol::browser::BrowserCommand;
    // open_surface closes the document. Refuse before mutating any tabs or pages,
    // rather than silently dropping an unsaved editor buffer to reveal the browser.
    if panel.is_dirty && browser_command_reveals_surface(&command) {
        return BrowserReply::Ready(Err(
            "The editor has unsaved changes. Ask the user to save or discard them before switching to the browser.".into(),
        ));
    }
    panel.ensure_browser(window, cx);
    match command {
        BrowserCommand::Screenshot => {
            #[cfg(any(target_os = "macos", target_os = "linux", target_os = "windows"))]
            {
                panel.open_surface(Surface::Browser, cx);
                let Some(browser) = panel.browser.clone() else {
                    return BrowserReply::Ready(Err("The browser panel is not ready.".to_string()));
                };
                match browser.update(cx, |browser, cx| browser.take_snapshot(None, cx)) {
                    Ok(rx) => BrowserReply::PendingSnapshot(rx),
                    Err(err) => BrowserReply::Ready(Err(err)),
                }
            }
            #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
            {
                BrowserReply::Ready(Err(
                    "The embedded browser is not supported on this platform.".to_string()
                ))
            }
        }
        BrowserCommand::Wait {
            selector,
            text,
            timeout_ms,
        } => {
            panel.open_surface(Surface::Browser, cx);
            let deadline =
                std::time::Instant::now() + std::time::Duration::from_millis(timeout_ms.max(100));
            BrowserReply::PendingWait {
                selector,
                text,
                deadline,
            }
        }
        BrowserCommand::ConsoleLogs { clear, level } => {
            let script = super::browser::drain_console_logs_js(clear, &level);
            match panel.start_browser_eval(&script, cx) {
                Ok(rx) => BrowserReply::PendingEval(rx),
                Err(error) => BrowserReply::Ready(Err(error)),
            }
        }
        _ => {
            let script = match &command {
                BrowserCommand::Snapshot => Some(super::browser::snapshot_js()),
                BrowserCommand::Act {
                    action,
                    target,
                    text,
                    key,
                } => {
                    let target_json = match target {
                        threadlane_protocol::browser::ActTarget::Ref(number) => {
                            serde_json::json!({"ref": number, "selector": serde_json::Value::Null})
                        }
                        threadlane_protocol::browser::ActTarget::Selector(selector) => {
                            serde_json::json!({"ref": serde_json::Value::Null, "selector": selector})
                        }
                    }
                    .to_string();
                    let text_json = serde_json::to_string(text).unwrap_or_else(|_| "null".into());
                    let key_json = serde_json::to_string(key).unwrap_or_else(|_| "null".into());
                    Some(super::browser::act_script(
                        action,
                        &target_json,
                        &text_json,
                        &key_json,
                    ))
                }
                BrowserCommand::Evaluate { script } => {
                    Some(super::browser::evaluate_script_wrap(script))
                }
                _ => None,
            };
            match script {
                Some(script) => match panel.start_browser_eval(&script, cx) {
                    Ok(rx) => BrowserReply::PendingEval(rx),
                    Err(error) => BrowserReply::Ready(Err(error)),
                },
                None => BrowserReply::Ready(panel.apply_browser_command(command, window, cx)),
            }
        }
    }
}

/// Shapes a script-eval reply for the agent: pretty-prints the captured
/// console-log ring buffer and truncates oversized payloads.
fn finalize_browser_eval(payload: &str) -> String {
    let inner = super::browser::unwrap_callback_payload(payload);
    // Format console logs if this payload is from drain_console_logs_js
    if inner.contains("\"logs\":[") && inner.contains("\"count\":") {
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(&inner) {
            if let Some(logs) = v.get("logs").and_then(|a| a.as_array()) {
                if logs.is_empty() {
                    return "No console errors or warnings recorded on the current page."
                        .to_string();
                }
                let mut out = format!("Recorded console messages ({}):\n", logs.len());
                for log in logs {
                    let level = log
                        .get("level")
                        .and_then(|s| s.as_str())
                        .unwrap_or("log")
                        .to_uppercase();
                    let msg = log.get("message").and_then(|s| s.as_str()).unwrap_or("");
                    let line_info = match (
                        log.get("source").and_then(|s| s.as_str()),
                        log.get("line").and_then(|l| l.as_i64()),
                    ) {
                        (Some(src), Some(l)) => format!(" ({src}:{l})"),
                        (Some(src), None) => format!(" ({src})"),
                        _ => String::new(),
                    };
                    out.push_str(&format!("- [{level}]{line_info} {msg}\n"));
                }
                return out.trim_end().to_string();
            }
        }
    }
    if inner.chars().count() <= MAX_BROWSER_EVAL_CHARS {
        return inner;
    }
    let head: String = inner.chars().take(MAX_BROWSER_EVAL_CHARS).collect();
    format!("{head}\n[... browser result truncated to {MAX_BROWSER_EVAL_CHARS} characters ...]")
}

/// WebKitGTK snapshots are PNG, WKWebView's are JPEG — name files and data
/// URLs after the actual bytes rather than the producing platform.
fn snapshot_file_ext(bytes: &[u8]) -> &'static str {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        "png"
    } else {
        "jpg"
    }
}

/// A `data:` URL for snapshot bytes, with the mime sniffed from the
/// image magic bytes rather than assumed per platform.
fn base64_data_url(bytes: &[u8]) -> String {
    use base64::Engine as _;
    let mime = if snapshot_file_ext(bytes) == "png" {
        "image/png"
    } else {
        "image/jpeg"
    };
    format!(
        "data:{mime};base64,{}",
        base64::engine::general_purpose::STANDARD.encode(bytes)
    )
}

fn convert_node_to_tree_item(node: FileNode, expanded_paths: &HashSet<String>) -> TreeItem {
    let is_expanded = expanded_paths.contains(&node.relative_path);
    if node.is_dir {
        let children = node
            .children
            .into_iter()
            .map(|child| convert_node_to_tree_item(child, expanded_paths))
            .collect::<Vec<_>>();
        TreeItem::new(node.relative_path, node.name)
            .expanded(is_expanded)
            .children(children)
    } else {
        TreeItem::new(node.relative_path, node.name)
    }
}

/// Reconciles the review file selection against a fresh scan.
///
/// The first refresh selects everything so an unreviewed tree starts fully
/// checked; later refreshes only drop selected paths that disappeared, so an
/// explicit empty selection is preserved rather than re-defaulted.
pub(crate) fn retain_review_selection(
    selected: &mut HashSet<String>,
    available: HashSet<String>,
    initialized: &mut bool,
) {
    if !*initialized {
        *selected = available;
        *initialized = true;
    } else {
        selected.retain(|path| available.contains(path));
    }
}

/// Maps a panel `GitAction` onto the wire-level `GitOperation` the daemon
/// executes (protocol v3). The daemon re-inspects afterwards, so the UI
/// only needs the operation — status and messages come back in
/// `GitActionOutcome`.
fn git_action_to_operation(
    action: &GitAction,
    message: String,
    selected_paths: Vec<String>,
) -> threadlane_protocol::repo::GitOperation {
    use threadlane_protocol::repo::{CheckoutMode, GitOperation};
    match action {
        GitAction::Commit | GitAction::CommitAndPush => GitOperation::Commit {
            message,
            selected_paths,
            push: matches!(action, GitAction::CommitAndPush),
        },
        GitAction::Push => GitOperation::Push,
        GitAction::Pull => GitOperation::Pull,
        GitAction::Fetch => GitOperation::Fetch,
        GitAction::StageAll => GitOperation::StageAll,
        GitAction::UnstageAll => GitOperation::UnstageAll,
        GitAction::CreatePullRequest => GitOperation::CreatePullRequest,
        GitAction::Checkout(branch) => GitOperation::Checkout {
            branch: branch.clone(),
            mode: CheckoutMode::Clean,
        },
        GitAction::CheckoutStash(branch) => GitOperation::Checkout {
            branch: branch.clone(),
            mode: CheckoutMode::Stash,
        },
        GitAction::CheckoutCarry(branch) => GitOperation::Checkout {
            branch: branch.clone(),
            mode: CheckoutMode::Carry,
        },
        GitAction::CreateBranch(branch) => GitOperation::CreateBranch {
            name: branch.clone(),
        },
        GitAction::DeleteBranch(branch) => GitOperation::DeleteBranch {
            branch: branch.clone(),
            force: false,
        },
        GitAction::Merge(branch) => GitOperation::Merge {
            branch: branch.clone(),
        },
        GitAction::PopStash(index) => GitOperation::PopStash { index: *index },
        GitAction::DropStash(index) => GitOperation::DropStash { index: *index },
        GitAction::DiscardFile(path) => GitOperation::Discard {
            paths: vec![path.clone()],
        },
        GitAction::DiscardFiles(paths) => GitOperation::Discard {
            paths: paths.clone(),
        },
        GitAction::DiscardAll => GitOperation::DiscardAll,
        GitAction::IgnoreFile(path) => GitOperation::IgnoreFile {
            path: path.clone(),
        },
        GitAction::IgnoreExtension(ext) => GitOperation::IgnoreExtension {
            extension: ext.clone(),
        },
        GitAction::StageFile(path) => GitOperation::Stage {
            paths: vec![path.clone()],
        },
        GitAction::UnstageFile(path) => GitOperation::Unstage {
            paths: vec![path.clone()],
        },
        GitAction::StageFiles(paths) => GitOperation::Stage {
            paths: paths.clone(),
        },
        GitAction::UnstageFiles(paths) => GitOperation::Unstage {
            paths: paths.clone(),
        },
        GitAction::StashPush {
            message,
            include_untracked,
        } => GitOperation::StashPush {
            message: message.clone(),
            include_untracked: *include_untracked,
        },
    }
}

#[cfg(test)]
mod dialog_keyboard_tests {
    use super::RightPanelView;
    use gpui::{
        AppContext, Context, Entity, FocusHandle, InteractiveElement, IntoElement, Render, Role,
        ParentElement, StatefulInteractiveElement, Styled, TestAppContext, Window, div,
    };
    use gpui_component::{Root, WindowExt};
    use threadlane_ui_state::AppState;

    struct Host {
        panel: Entity<RightPanelView>,
        trigger: FocusHandle,
    }
    impl Render for Host {
        fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            self.panel
                .update(cx, |panel, cx| panel.sync_git_dialog(window, cx));
            div()
                .id("dialog-host")
                .track_focus(&self.trigger)
                .role(Role::Application)
                .tab_group()
                .size_full()
                .child(self.panel.update(cx, |panel, cx| {
                    let branches = panel.git_status.as_ref()
                        .map(|s| s.branch_details.clone()).unwrap_or_default();
                    panel.render_branch_section("BRANCHES", branches, cx)
                        .into_any_element()
                }))
        }
    }

    #[gpui::test]
    fn branch_deletion_requires_confirmation_and_keeps_project_scope(cx: &mut TestAppContext) {
        cx.update(gpui_component::init);
        let model = cx.new(|_| AppState::default());
        let captured = std::rc::Rc::new(std::cell::RefCell::new(None));
        let capture = captured.clone();
        let (_, cx) = cx.add_window_view(move |window, cx| {
            let panel = cx.new(|cx| RightPanelView::new(model, window, cx));
            let trigger = cx.focus_handle();
            trigger.focus(window, cx);
            *capture.borrow_mut() = Some(panel.clone());
            Root::new(cx.new(|_| Host { panel, trigger }), window, cx)
        });
        let panel = captured.borrow_mut().take().unwrap();
        let project = std::path::PathBuf::from("/test/project");
        cx.update(|window, cx| {
            panel.update(cx, |panel, cx| {
                panel.project = Some(project.clone());
                panel.git_status = Some(threadlane_git::GitStatus {
                    branch: Some("current".into()),
                    branch_details: vec![
                        threadlane_git::GitBranchInfo { name: "main".into(), is_default: true, ..Default::default() },
                        threadlane_git::GitBranchInfo { name: "current".into(), is_current: true, ..Default::default() },
                        threadlane_git::GitBranchInfo { name: "feature".into(), ..Default::default() },
                        threadlane_git::GitBranchInfo {
                            name: "origin/feature".into(),
                            is_remote: true,
                            ..Default::default()
                        },
                    ],
                    ..Default::default()
                });
                for branch in ["main", "current", "missing", "origin/feature"] {
                    assert!(!panel.can_delete_branch(&project, branch));
                    panel.confirm_delete_branch(project.clone(), branch.into(), window, cx);
                    assert!(!window.has_active_dialog(cx));
                }
                assert!(!panel.can_delete_branch(std::path::Path::new("/other"), "feature"));
                panel.git_busy = true;
                assert!(!panel.can_delete_branch(&project, "feature"));
                panel.git_busy = false;
                assert!(panel.can_delete_branch(&project, "feature"));
                cx.notify();
            });
        });
        cx.run_until_parked();
        cx.update(|window, cx| window.draw(cx).clear(cx));
        let row = cx.debug_bounds("branch-row-feature").unwrap();
        for keys in ["down enter", "down down enter"] {
            cx.simulate_event(gpui::MouseDownEvent {
                button: gpui::MouseButton::Right,
                position: row.center(),
                modifiers: Default::default(),
                click_count: 1,
                first_mouse: false,
            });
            cx.run_until_parked();
            cx.update(|window, cx| window.draw(cx).clear(cx));
            cx.simulate_keystrokes(keys);
            cx.run_until_parked();
            cx.update(|window, cx| {
                window.draw(cx).clear(cx);
                assert!(
                    !panel.read(cx).git_busy,
                    "right-click must not checkout or delete"
                );
                if keys == "down enter" {
                    assert_eq!(
                        cx.read_from_clipboard().unwrap().text().as_deref(),
                        Some("feature")
                    );
                    assert!(!window.has_active_dialog(cx));
                } else {
                    assert!(window.has_active_dialog(cx));
                }
            });
        }
        cx.simulate_keystrokes("escape");
        cx.run_until_parked();
        cx.update(|window, cx| {
            assert!(!window.has_active_dialog(cx));
            assert!(!panel.read(cx).git_busy);
            panel.update(cx, |panel, cx| {
                panel.confirm_delete_branch(project.clone(), "feature".into(), window, cx);
                panel.project = Some("/test/other-project".into());
            });
            window.draw(cx).clear(cx);
        });
        cx.simulate_keystrokes("enter");
        cx.run_until_parked();
        cx.update(|window, cx| {
            assert!(!window.has_active_dialog(cx));
            assert!(
                !panel.read(cx).git_busy,
                "stale confirmation must not run Git in another project"
            );
        });
    }
    #[gpui::test]
    fn git_dialogs_dismiss_with_escape_and_restore_focus(cx: &mut TestAppContext) {
        cx.update(gpui_component::init);
        let model = cx.new(|_| AppState::default());
        let captured = std::rc::Rc::new(std::cell::RefCell::new(None));
        let capture = captured.clone();
        let (_, cx) = cx.add_window_view(move |window, cx| {
            let panel = cx.new(|cx| RightPanelView::new(model, window, cx));
            let trigger = cx.focus_handle();
            trigger.focus(window, cx);
            *capture.borrow_mut() = Some((panel.clone(), trigger.clone()));
            Root::new(cx.new(|_| Host { panel, trigger }), window, cx)
        });
        let (panel, trigger) = captured.borrow_mut().take().unwrap();
        for kind in 0..3 {
            cx.update(|window, cx| {
                panel.update(cx, |panel, cx| {
                    panel.new_branch_dialog_open = kind == 0;
                    panel.merge_dialog_open = kind == 1;
                    panel.switch_dialog_open = kind == 2;
                    panel.switch_target_branch = Some("test-target".into());
                    panel.sync_git_dialog(window, cx);
                })
            });
            cx.run_until_parked();
            cx.update(|window, cx| {
                window.draw(cx).clear(cx);
                assert!(window.has_active_dialog(cx));
            });
            if kind == 2 {
                cx.update(|window, cx| {
                    window.focus_next(cx); // Stash
                    window.focus_next(cx); // Carry
                    window.draw(cx).clear(cx);
                });
                let keystroke = gpui::Keystroke::parse("space").unwrap();
                cx.simulate_event(gpui::KeyDownEvent {
                    keystroke: keystroke.clone(),
                    is_held: false,
                    prefer_character_input: false,
                });
                cx.simulate_event(gpui::KeyUpEvent { keystroke });
                panel.read_with(cx, |panel, _| {
                    assert!(!panel.switch_stash_mode, "Carry is keyboard selectable");
                    assert!(!panel.git_busy, "choosing a mode does not switch branches");
                });
            }
            cx.simulate_keystrokes("escape");
            cx.run_until_parked();
            cx.update(|window, cx| {
                assert!(!window.has_active_dialog(cx));
                assert!(trigger.is_focused(window));
                let panel = panel.read(cx);
                assert!(
                    !panel.new_branch_dialog_open
                        && !panel.merge_dialog_open
                        && !panel.switch_dialog_open
                );
                assert!(panel.switch_target_branch.is_none());
                assert!(!panel.git_busy, "dismissing a dialog must not run Git");
            });
        }
    }
}

#[cfg(test)]
mod review_layout_tests {
    use super::{RightPanelView, Surface};
    use gpui::{
        AppContext, Context, Entity, IntoElement, ListSizingBehavior, ParentElement, Render, Styled,
        TestAppContext, Window, div, list, px,
    };
    use threadlane_git::GitFile;
    use threadlane_ui_state::AppState;

    struct RowHost {
        panel: Entity<RightPanelView>,
        width: f32,
    }

    struct SurfaceHost {
        panel: Entity<RightPanelView>,
        width: f32,
    }

    impl Render for SurfaceHost {
        fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            self.panel.update(cx, |panel, cx| {
                div()
                    .w(px(self.width))
                    .h(px(600.0))
                    .flex()
                    .flex_col()
                    .child(panel.render_header(cx))
                    .child(panel.render_chooser(cx))
            })
        }
    }

    #[gpui::test]
    fn surface_controls_fit_narrow_panels(cx: &mut TestAppContext) {
        cx.update(gpui_component::init);
        let model = cx.new(|_| AppState::default());
        let (host, cx) = cx.add_window_view(move |window, cx| SurfaceHost {
            panel: cx.new(|cx| RightPanelView::new(model, window, cx)),
            width: 280.0,
        });
        for rem_size in [16.0, 20.0] {
            for width in [280.0, 320.0, 480.0] {
                host.update(cx, |host, cx| {
                    host.width = width;
                    cx.notify();
                });
                cx.update(|window, cx| {
                    window.set_rem_size(px(rem_size));
                    window.draw(cx).clear(cx);
                });
                let mut previous_choice = None;
                for (tab, choice) in [
                    ("right-panel-tab-Trajectory", "right-panel-choice-Trajectory"),
                    ("right-panel-tab-Agents", "right-panel-choice-Agents"),
                    ("right-panel-tab-Review", "right-panel-choice-Review"),
                    ("right-panel-tab-Files", "right-panel-choice-Files"),
                    ("right-panel-tab-Browser", "right-panel-choice-Browser"),
                ].into_iter().take(Surface::all().len()) {
                    for selector in [tab, choice] {
                        let bounds = cx.debug_bounds(selector).expect("surface control rendered");
                        assert!(bounds.left() >= px(0.0) && bounds.right() <= px(width),
                            "{selector} overflows at width {width}, rem {rem_size}: {bounds:?}");
                        if selector == choice {
                            if let Some(bottom) = previous_choice {
                                assert!(bounds.top() >= bottom, "surface choices overlap");
                            }
                            previous_choice = Some(bounds.bottom());
                        }
                    }
                }
            }
        }
    }

    impl Render for RowHost {
        fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            div().w(px(self.width)).h(px(200.0)).child(
                self.panel.update(cx, |panel, cx| {
                    list(
                        panel.review_files_list_state.clone(),
                        cx.processor(RightPanelView::render_review_file_row),
                    )
                    .size_full()
                    .with_sizing_behavior(ListSizingBehavior::Auto)
                }),
            )
        }
    }

    #[gpui::test]
    fn long_review_paths_keep_filename_and_stats_visible(cx: &mut TestAppContext) {
        cx.update(gpui_component::init);
        let model = cx.new(|_| AppState::default());
        let (host, cx) = cx.add_window_view(move |window, cx| {
            let panel = cx.new(|cx| RightPanelView::new(model, window, cx));
            panel.update(cx, |panel, _| {
                let mut file = GitFile::default();
                file.path = format!("crates/{}/tests.rs", "long-directory-name/".repeat(12));
                file.additions = 1234;
                file.deletions = 567;
                panel.review_files = vec![file];
                panel
                    .review_files_list_state
                    .reset_with_uniform_height(1, px(32.0));
            });
            RowHost {
                panel,
                width: 320.0,
            }
        });
        for path in [
            format!("crates/{}tests.rs", "long-directory-name/".repeat(12)),
            format!("src/{}.rs", "long-filename".repeat(12)),
            format!("{}.rs", "root-filename".repeat(12)),
            "Cargo.toml".to_owned(),
        ] {
            for width in [320.0, 480.0, 640.0] {
                host.update(cx, |host, cx| {
                    host.width = width;
                    host.panel.update(cx, |panel, cx| {
                        panel.review_files[0].path = path.clone();
                        cx.notify();
                    });
                    cx.notify();
                });
                cx.update(|window, cx| window.draw(cx).clear(cx));
                let row = cx.debug_bounds("review-file-row").expect("row rendered");
                assert_eq!(row.size.width, px(width - 16.0), "rows must share the panel inset");
                assert_eq!(row.size.height, px(32.0), "row height changed at {width}");
                let filename = cx
                    .debug_bounds("review-filename")
                    .expect("filename rendered");
                let status = cx
                    .debug_bounds("review-file-status")
                    .expect("status rendered");
                let stats = cx
                    .debug_bounds("review-file-stats")
                    .expect("stats rendered");
                assert!(
                    filename.size.height <= row.size.height,
                    "filename wraps beyond row height at {width}: {filename:?}"
                );
                assert!(
                    filename.size.width >= px(40.0),
                    "filename collapsed at {width}: {filename:?}"
                );
                assert!(
                    filename.right() <= status.left(),
                    "filename overlaps status at {width}"
                );
                assert!(
                    status.right() <= stats.left(),
                    "status overlaps stats at {width}"
                );
                assert!(
                    stats.right() <= px(width),
                    "stats overflow at {width}: {stats:?}"
                );
            }
        }
    }
}

#[cfg(test)]
mod browser_editor_safety_tests {
    use super::{browser_command_reveals_surface, start_browser_request, BrowserReply, RightPanelView, Surface};
    use gpui::{AppContext, TestAppContext};
    use threadlane_protocol::browser::{BrowserCommand, BrowserTabAction};
    use threadlane_ui_state::AppState;

    #[test]
    fn listing_tabs_does_not_reveal_browser() {
        assert!(!browser_command_reveals_surface(&BrowserCommand::Tabs { action: BrowserTabAction::List }));
    }

    #[gpui::test]
    fn browser_commands_preserve_dirty_document(cx: &mut TestAppContext) {
        cx.update(gpui_component::init);
        let model = cx.new(|_| AppState::default());
        let (panel, cx) = cx.add_window_view(|window, cx| RightPanelView::new(model, window, cx));
        panel.update_in(cx, |panel, window, cx| {
            for surface in [Surface::Review, Surface::Files] {
                panel.active_surface = Some(surface);
                panel.pending_document = Some(("draft.rs".into(), "unsaved buffer".into()));
                panel.sync_pending_document(window, cx);
                panel.is_dirty = true;
                let editor = panel.editor_state.clone().expect("editor");
                let error = panel.open_terminal_url("http://localhost:3000/", window, cx).unwrap_err();
                assert!(error.contains("Save or discard"));
                assert_eq!(panel.active_surface, Some(surface));
                assert!(panel.is_dirty);
                assert_eq!(panel.editor_state.as_ref(), Some(&editor));
                assert!(panel.browser.is_none());
                for command in [
                    BrowserCommand::Tabs { action: BrowserTabAction::Open { url: "example.com".into() } },
                    BrowserCommand::Tabs { action: BrowserTabAction::Select { tab_id: 1 } },
                    BrowserCommand::Tabs { action: BrowserTabAction::Close { tab_id: 1 } },
                    BrowserCommand::Navigate { url: "example.com".into() },
                    BrowserCommand::Back,
                    BrowserCommand::Screenshot,
                    BrowserCommand::Wait { selector: None, text: None, timeout_ms: 100 },
                ] {
                    let result = start_browser_request(panel, command, window, cx);
                    assert!(matches!(result, BrowserReply::Ready(Err(error)) if error.contains("unsaved changes")));
                    assert_eq!(panel.active_surface, Some(surface));
                    assert!(panel.is_dirty);
                    assert_eq!(panel.editor_state.as_ref(), Some(&editor));
                    assert_eq!(panel.saved_content, "unsaved buffer");
                    assert!(panel.browser.is_none());
                }
            }
        });
    }
}

#[cfg(test)]
mod review_diff_tests {
    use super::{GitAction, PanelEvent, ReviewDiffState, ReviewDiffTarget, RightPanelView, Surface};
    use gpui::{
        AppContext, Context, Entity, IntoElement, ParentElement, Render, Styled, TestAppContext,
        Window, div, px,
    };
    use gpui_component::Root;
    use std::path::PathBuf;
    use threadlane_ui_state::AppState;

    struct DiffHost {
        panel: Entity<RightPanelView>,
        width: f32,
    }

    impl Render for DiffHost {
        fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            div()
                .flex()
                .flex_col()
                .w(px(self.width))
                .h_96()
                .child(self.panel.update(cx, |panel, cx| panel.render_files(cx)))
        }
    }

    #[gpui::test]
    fn review_refresh_invalidates_then_starts_one_diff_after_status(cx: &mut TestAppContext) {
        cx.update(gpui_component::init);
        let model = cx.new(|_| {
            let mut state = AppState::default();
            state.active_work_dir = Some(PathBuf::from("/workspace"));
            state
        });
        let captured = std::rc::Rc::new(std::cell::RefCell::new(None));
        let capture = captured.clone();
        let (_, cx) = cx.add_window_view(move |window, cx| {
            let panel = cx.new(|cx| RightPanelView::new(model, window, cx));
            *capture.borrow_mut() = Some(panel.clone());
            Root::new(panel, window, cx)
        });
        let panel = captured.borrow_mut().take().unwrap();
        panel.update(cx, |panel, cx| {
            let (event_tx, _event_rx) = tokio::sync::mpsc::unbounded_channel();
            panel.event_tx = event_tx;
            panel.open_combined_diff(cx);
            panel.set_ignore_whitespace(true, cx);
            let old = panel.review_diff_request.clone().unwrap();
            panel.apply_review_diff_result(old.clone(), Ok("old patch".into()), cx);
            let loads = panel.review_diff_load_count;
            panel.refresh_surface(Surface::Review, cx);
            assert_eq!(panel.review_diff_load_count, loads);
            assert!(panel.pending_document.is_none());
            assert!(matches!(
                panel.review_diff_state,
                Some(ReviewDiffState::Loading)
            ));
            panel.apply_review_diff_result(old, Ok("stale patch".into()), cx);
            assert!(matches!(
                panel.review_diff_state,
                Some(ReviewDiffState::Loading)
            ));
            panel.apply_event(
                PanelEvent::ReviewLoaded {
                    project: PathBuf::from("/workspace"),
                    status: Some(threadlane_git::GitStatus::default()),
                    files: Vec::new(),
                    error: None,
                },
                cx,
            );
            assert_eq!(panel.review_diff_load_count, loads + 1);
            let request = panel.review_diff_request.as_ref().unwrap();
            assert_eq!(request.target, ReviewDiffTarget::AllChanges);
            assert!(request.options.ignore_whitespace);
        });
    }

    #[gpui::test]
    fn review_checkout_failure_retains_target_and_filter(cx: &mut TestAppContext) {
        cx.update(gpui_component::init);
        let model = cx.new(|_| {
            let mut state = AppState::default();
            state.active_work_dir = Some(PathBuf::from("/workspace"));
            state
        });
        let captured = std::rc::Rc::new(std::cell::RefCell::new(None));
        let capture = captured.clone();
        let (_, cx) = cx.add_window_view(move |window, cx| {
            let panel = cx.new(|cx| RightPanelView::new(model, window, cx));
            *capture.borrow_mut() = Some(panel.clone());
            Root::new(panel, window, cx)
        });
        let panel = captured.borrow_mut().take().unwrap();
        panel.update(cx, |panel, cx| {
            let (event_tx, _event_rx) = tokio::sync::mpsc::unbounded_channel();
            panel.event_tx = event_tx;
            let status = threadlane_git::GitStatus {
                branch: Some("main".into()),
                ..threadlane_git::GitStatus::default()
            };
            panel.replace_git_status(Some(status.clone()), cx);
            for target in [
                ReviewDiffTarget::AllChanges,
                ReviewDiffTarget::File("selected.txt".into()),
            ] {
                for action in [
                    GitAction::Checkout("other".into()),
                    GitAction::CheckoutStash("other".into()),
                    GitAction::CheckoutCarry("other".into()),
                    GitAction::CreateBranch("other".into()),
                ] {
                    panel.open_review_diff(target.clone(), cx);
                    panel.set_ignore_whitespace(true, cx);
                    let old = panel.review_diff_request.clone().unwrap();
                    let loads = panel.review_diff_load_count;
                    panel.run_git_action_without_window(action, cx);
                    assert!(panel.review_diff_options.ignore_whitespace);
                    assert_eq!(panel.review_diff_request.as_ref().unwrap().target, target);
                    panel.apply_review_diff_result(old, Ok("old checkout patch".into()), cx);
                    assert!(matches!(
                        panel.review_diff_state,
                        Some(ReviewDiffState::Loading)
                    ));
                    panel.set_ignore_whitespace(false, cx);
                    panel.set_ignore_whitespace(true, cx);
                    assert_eq!(panel.review_diff_load_count, loads);
                    let pending = panel.review_diff_request.clone().unwrap();
                    panel.apply_review_diff_result(
                        pending,
                        Ok("pending checkout patch".into()),
                        cx,
                    );
                    assert!(matches!(
                        panel.review_diff_state,
                        Some(ReviewDiffState::Loading)
                    ));
                    panel.apply_event(
                        PanelEvent::ActionFinished {
                            project: PathBuf::from("/workspace"),
                            status: if target == ReviewDiffTarget::AllChanges {
                                Ok(status.clone())
                            } else {
                                Err("status unavailable".into())
                            },
                            action_error: Some("checkout rejected".into()),
                            action_message: None,
                            checkout_succeeded: false,
                        },
                        cx,
                    );
                    assert!(!panel.git_busy);
                    assert_eq!(panel.review_diff_load_count, loads + 1);
                    let retry = panel.review_diff_request.clone().unwrap();
                    assert_eq!(retry.target, target);
                    assert!(retry.options.ignore_whitespace);
                    panel.apply_review_diff_result(
                        retry,
                        Ok("retained checkout patch".into()),
                        cx,
                    );
                    assert!(matches!(
                        panel.review_diff_state,
                        Some(ReviewDiffState::Ready { empty: false })
                    ));
                }
            }
            panel.run_git_action_without_window(GitAction::Checkout("main".into()), cx);
            panel.apply_event(
                PanelEvent::ActionFinished {
                    project: PathBuf::from("/workspace"),
                    status: Err("status unavailable".into()),
                    action_error: None,
                    action_message: None,
                    checkout_succeeded: true,
                },
                cx,
            );
            assert!(!panel.review_diff_options.ignore_whitespace);
            assert!(panel.review_diff_request.is_none());
            assert!(panel.document_title.is_none());
        });
    }

    #[gpui::test]
    fn review_diff_rejects_results_after_toggles_refresh_navigation_and_checkout_change(
        cx: &mut TestAppContext,
    ) {
        cx.update(gpui_component::init);
        let model = cx.new(|_| {
            let mut state = AppState::default();
            state.active_work_dir = Some(PathBuf::from("/workspace"));
            state
        });
        let (panel, cx) =
            cx.add_window_view(|window, cx| RightPanelView::new(model.clone(), window, cx));
        panel.update_in(cx, |panel, window, cx| {
            panel.open_file_diff("first.txt".into(), cx);
            let first = panel.review_diff_request.clone().unwrap();
            panel.set_ignore_whitespace(true, cx);
            let filtered = panel.review_diff_request.clone().unwrap();
            panel.apply_review_diff_result(first, Ok("old unfiltered patch".into()), cx);
            assert!(matches!(
                panel.review_diff_state,
                Some(ReviewDiffState::Loading)
            ));
            panel.reload_review_diff(cx);
            panel.apply_review_diff_result(filtered, Ok(String::new()), cx);
            assert!(matches!(
                panel.review_diff_state,
                Some(ReviewDiffState::Loading)
            ));
            let refreshed = panel.review_diff_request.clone().unwrap();
            panel.open_combined_diff(cx);
            panel.apply_review_diff_result(refreshed, Ok(String::new()), cx);
            assert_eq!(
                panel.review_diff_request.as_ref().unwrap().target,
                ReviewDiffTarget::AllChanges
            );
            assert!(matches!(
                panel.review_diff_state,
                Some(ReviewDiffState::Loading)
            ));
            let combined = panel.review_diff_request.clone().unwrap();
            panel.pending_document = Some(("editable.rs".into(), "editable".into()));
            panel.sync_pending_document(window, cx);
            panel.apply_review_diff_result(combined, Ok("stale patch".into()), cx);
            assert!(panel.review_diff_request.is_none());
            assert!(panel.editor_state.is_some());
            panel.open_combined_diff(cx);
            let closing = panel.review_diff_request.clone().unwrap();
            panel.close_document(cx);
            panel.apply_review_diff_result(closing, Ok("stale patch".into()), cx);
            assert!(panel.document_title.is_none());
            assert!(panel.review_diff_state.is_none());
            panel.open_combined_diff(cx);
            let previous_checkout = panel.review_diff_request.clone().unwrap();
            model.update(cx, |state, _| {
                state.active_work_dir = Some(PathBuf::from("/other-worktree"))
            });
            panel.sync_project(cx);
            panel.apply_review_diff_result(previous_checkout, Ok("stale patch".into()), cx);
            assert!(panel.review_diff_request.is_none());
            assert!(!panel.review_diff_options.ignore_whitespace);
            assert!(panel.document_title.is_none());
        });
    }

    #[gpui::test]
    fn review_diff_empty_failure_retry_and_target_retention(cx: &mut TestAppContext) {
        cx.update(gpui_component::init);
        let model = cx.new(|_| {
            let mut state = AppState::default();
            state.active_work_dir = Some(PathBuf::from("/workspace"));
            state
        });
        let (panel, cx) = cx.add_window_view(|window, cx| RightPanelView::new(model, window, cx));
        panel.update(cx, |panel, cx| {
            panel.selected_files.insert("selected.txt".into());
            panel.open_combined_diff(cx);
            panel.set_ignore_whitespace(true, cx);
            let request = panel.review_diff_request.clone().unwrap();
            panel.apply_review_diff_result(request, Ok(String::new()), cx);
            assert!(matches!(panel.review_diff_state, Some(ReviewDiffState::Ready { empty: true })));
            panel.reload_review_diff(cx);
            let request = panel.review_diff_request.clone().unwrap();
            let error = threadlane_git::diff_file_with_options(&request.project, "../outside", request.options).unwrap_err();
            panel.apply_review_diff_result(request.clone(), Err(error.to_string()), cx);
            assert!(matches!(&panel.review_diff_state, Some(ReviewDiffState::Failed(error)) if error.contains("outside")));
            panel.reload_review_diff(cx);
            let retry = panel.review_diff_request.clone().unwrap();
            assert!(retry.revision > request.revision);
            assert_eq!(retry.target, ReviewDiffTarget::AllChanges);
            assert!(retry.options.ignore_whitespace);
            assert!(matches!(panel.review_diff_state, Some(ReviewDiffState::Loading)));
            panel.apply_review_diff_result(retry, Ok("Binary files differ".into()), cx);
            assert!(matches!(panel.review_diff_state, Some(ReviewDiffState::Ready { empty: false })));
            panel.set_ignore_whitespace(false, cx);
            panel.open_file_diff("next.txt".into(), cx);
            assert!(!panel.review_diff_options.ignore_whitespace);
            panel.set_ignore_whitespace(true, cx);
            panel.open_file_diff("last.txt".into(), cx);
            assert!(panel.review_diff_request.as_ref().unwrap().options.ignore_whitespace);
            assert!(panel.selected_files.contains("selected.txt"));
            assert!(!panel.git_busy);
            let status = threadlane_git::GitStatus {
                branch: Some("main".into()),
                ..threadlane_git::GitStatus::default()
            };
            panel.replace_git_status(Some(status.clone()), cx);
            panel.replace_git_status(Some(status), cx);
            assert!(panel.review_diff_options.ignore_whitespace);
            let old_branch = panel.review_diff_request.clone().unwrap();
            panel.replace_git_status(Some(threadlane_git::GitStatus {
                branch: Some("other".into()),
                ..threadlane_git::GitStatus::default()
            }), cx);
            panel.apply_review_diff_result(old_branch, Ok("old branch patch".into()), cx);
            assert!(!panel.review_diff_options.ignore_whitespace);
            assert!(panel.review_diff_request.is_none());
            assert!(panel.document_title.is_none());
        });
    }

    #[gpui::test]
    fn review_checkbox_toggles_with_keyboard_and_retains_focus_at_narrow_width(
        cx: &mut TestAppContext,
    ) {
        cx.update(gpui_component::init);
        let model = cx.new(|_| {
            let mut state = AppState::default();
            state.active_work_dir = Some(PathBuf::from("/workspace"));
            state
        });
        let captured = std::rc::Rc::new(std::cell::RefCell::new(None));
        let capture = captured.clone();
        let (_, cx) = cx.add_window_view(move |window, cx| {
            let panel = cx.new(|cx| RightPanelView::new(model, window, cx));
            panel.update(cx, |panel, cx| {
                panel.active_surface = Some(Surface::Review);
                panel.selected_files.insert("keep.rs".into());
                panel.open_file_diff(format!("src/{}.rs", "long-file-name".repeat(12)), cx);
            });
            let host = cx.new(|_| DiffHost {
                panel: panel.clone(),
                width: 320.0,
            });
            *capture.borrow_mut() = Some((panel, host.clone()));
            Root::new(host, window, cx)
        });
        let (panel, host) = captured.borrow_mut().take().unwrap();
        cx.update(|window, cx| {
            window.draw(cx).clear(cx);
            window.focus_next(cx);
            window.focus_next(cx);
            window.focus_next(cx);
            window.draw(cx).clear(cx);
        });
        let checkbox = cx
            .debug_bounds("review-ignore-whitespace")
            .expect("checkbox rendered");
        assert!(checkbox.left() >= px(0.0));
        assert!(checkbox.right() <= px(320.0));
        let focused = cx.update(|window, cx| window.focused(cx).expect("checkbox focused"));
        for checked in [true, false] {
            let keystroke = gpui::Keystroke::parse("space").unwrap();
            cx.simulate_event(gpui::KeyDownEvent {
                keystroke: keystroke.clone(),
                is_held: false,
                prefer_character_input: false,
            });
            cx.simulate_event(gpui::KeyUpEvent { keystroke });
            panel.read_with(cx, |panel, _| {
                assert_eq!(panel.review_diff_options.ignore_whitespace, checked);
                assert!(panel.selected_files.contains("keep.rs"));
                assert!(!panel.git_busy);
            });
            cx.update(|window, cx| {
                window.draw(cx).clear(cx);
                assert!(focused.is_focused(window));
            });
        }
        for theme in [
            gpui_component::ThemeMode::Light,
            gpui_component::ThemeMode::Dark,
        ] {
            for rem_size in [16.0, 20.0] {
                for width in [320.0, 480.0, 640.0] {
                    cx.update(|window, cx| {
                        gpui_component::Theme::change(theme, Some(window), cx);
                        window.set_rem_size(px(rem_size));
                        host.update(cx, |host, cx| {
                            host.width = width;
                            cx.notify();
                        });
                        window.draw(cx).clear(cx);
                    });
                    for selector in [
                        "review-ignore-whitespace",
                        "right-panel-document-back",
                        "close-document",
                    ] {
                        let bounds = cx.debug_bounds(selector).expect("control rendered");
                        assert!(bounds.left() >= px(0.0));
                        assert!(bounds.right() <= px(width));
                    }
                }
            }
        }
        panel.update(cx, |panel, cx| {
            panel.set_ignore_whitespace(true, cx);
        });
        cx.run_until_parked();
        panel.update(cx, |panel, cx| {
            let request = panel.review_diff_request.clone().unwrap();
            panel.apply_review_diff_result(request, Ok(String::new()), cx);
        });
        cx.update(|window, cx| window.draw(cx).clear(cx));
        let show = cx
            .debug_bounds("show-whitespace-changes")
            .expect("empty recovery rendered");
        cx.simulate_mouse_move(show.center(), None, Default::default());
        cx.simulate_click(show.center(), Default::default());
        panel.read_with(cx, |panel, _| {
            assert!(!panel.review_diff_options.ignore_whitespace);
            assert!(panel.selected_files.contains("keep.rs"));
        });
        panel.update(cx, |panel, cx| {
            let request = panel.review_diff_request.clone().unwrap();
            let error = threadlane_git::diff_file_with_options(
                &request.project,
                "../outside",
                request.options,
            )
            .unwrap_err();
            panel.apply_review_diff_result(request, Err(error.to_string()), cx);
        });
        cx.update(|window, cx| window.draw(cx).clear(cx));
        let retry = cx
            .debug_bounds("retry-review-diff")
            .expect("error recovery rendered");
        let revision = panel.read_with(cx, |panel, _| {
            panel.review_diff_request.as_ref().unwrap().revision
        });
        cx.simulate_mouse_move(retry.center(), None, Default::default());
        cx.simulate_click(retry.center(), Default::default());
        panel.read_with(cx, |panel, _| {
            assert!(panel.review_diff_request.as_ref().unwrap().revision > revision);
            assert!(panel.selected_files.contains("keep.rs"));
        });
    }
}

#[cfg(test)]
mod environment_shortcut_tests {
    use super::RightPanelView;
    use crate::{ReviewTab, Surface};
    use gpui::{AppContext, Focusable, TestAppContext};
    use gpui_component::{Root, WindowExt};
    use threadlane_ui_state::AppState;

    #[gpui::test]
    fn environment_commit_opens_changes_and_focuses_summary_without_committing(
        cx: &mut TestAppContext,
    ) {
        cx.update(gpui_component::init);
        let model = cx.new(|_| AppState::default());
        let captured = std::rc::Rc::new(std::cell::RefCell::new(None));
        let capture = captured.clone();
        let (_, cx) = cx.add_window_view(move |window, cx| {
            let panel = cx.new(|cx| RightPanelView::new(model, window, cx));
            *capture.borrow_mut() = Some(panel.clone());
            Root::new(panel, window, cx)
        });
        let panel = captured.borrow_mut().take().unwrap();
        cx.update(|window, cx| {
            panel.update(cx, |panel, cx| {
                panel.active_surface = Some(Surface::Review);
                panel.review_tab = ReviewTab::History;
                panel.document_title = Some("Review · changed.rs".into());
                panel.open_commit(window, cx);
                assert_eq!(panel.active_surface, Some(Surface::Review));
                assert_eq!(panel.review_tab, ReviewTab::Changes);
                assert!(panel.document_title.is_none());
                assert!(!panel.git_busy, "Opening the commit UI must not run Git");
                assert!(panel
                    .commit_message_input
                    .read(cx)
                    .focus_handle(cx)
                    .is_focused(window));
            })
        });
    }

    #[gpui::test]
    fn environment_pr_uses_current_checkout_before_hidden_panel_renders(cx: &mut TestAppContext) {
        cx.update(gpui_component::init);
        let model = cx.new(|_| {
            let mut state = AppState::default();
            state.active_work_dir = None;
            state.active_session_id = None;
            state
        });
        let retained = model.clone();
        let captured = std::rc::Rc::new(std::cell::RefCell::new(None));
        let capture = captured.clone();
        let (_, cx) = cx.add_window_view(move |window, cx| {
            let panel = cx.new(|cx| RightPanelView::new(model, window, cx));
            *capture.borrow_mut() = Some(panel.clone());
            Root::new(panel, window, cx)
        });
        let panel = captured.borrow_mut().take().unwrap();
        cx.update(|window, cx| {
            retained.update(cx, |state, _| {
                state.active_work_dir = Some("/current-checkout".into());
                state.git_statuses.insert(
                    "/current-checkout".into(),
                    threadlane_git::GitStatus {
                        branch: Some("feature".into()),
                        remote: Some("git@github.com:owner/repo.git".into()),
                        has_upstream: true,
                        pr_ready: true,
                        pr_lookup_available: true,
                        ..Default::default()
                    },
                );
            });
            panel.update(cx, |panel, cx| {
                panel.project = Some("/previous-checkout".into());
                panel.open_draft_pr_dialog(window, cx);
                let key = panel.draft_pr_creation_key().unwrap();
                assert_eq!(key.project, std::path::PathBuf::from("/current-checkout"));
                assert_eq!(key.branch, "feature");
                assert!(!panel.git_busy);
            });
            assert!(
                window.has_active_dialog(cx),
                "The existing draft PR form opens without a second click"
            );
            window.close_dialog(cx);
            retained.update(cx, |state, _| {
                state
                    .git_statuses
                    .get_mut(std::path::Path::new("/current-checkout"))
                    .unwrap()
                    .branch = Some("new-feature".into());
            });
            panel.update(cx, |panel, cx| {
                panel.open_draft_pr_dialog(window, cx);
                assert_eq!(panel.draft_pr_creation_key().unwrap().branch, "new-feature");
            });
            assert!(window.has_active_dialog(cx));
            window.close_dialog(cx);
        });
    }
}
