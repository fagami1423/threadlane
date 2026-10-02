//! GPUI views for the Threadlane mobile client.
//!
//! A connect (pairing entry) screen in front of a tabbed shell: a chats
//! tab holding the session list and the live transcript with composer, a
//! Git tab for the selected project's repository, and a slide-over
//! sidebar for project picking and connection controls. All daemon
//! traffic flows through [`crate::client::MobileDaemon`]; the view pumps
//! its event stream on the GPUI executor and keeps a flat projection of
//! the wire types.

use std::collections::{HashMap, HashSet};
use std::sync::Mutex;
use std::time::Duration;

use gpui::{prelude::*, *};
use gpui_component::menu::{DropdownMenu, PopupMenuItem};
use gpui_component::scroll::ScrollableElement;
use gpui_component::tag::Tag;
use gpui_kit::component::StyledExt;
use gpui_kit::component::{
    button::{Button, ButtonVariants},
    input::{Input, InputEvent, InputState, Textarea},
    marker::{Marker, MarkerContent, MarkerLoadingStyle},
    ActiveTheme, Disableable, Icon, Sizable,
};
use gpui_kit_assets::__private as kit_icons;
use threadlane_protocol::automation::{
    AutomationCommand, AutomationResponse, Definition, Run, RunStatus, Schedule,
};
use threadlane_protocol::daemon::{CommandResponse, ComposerModel};
use threadlane_protocol::repo::{
    CheckoutMode, DiffOptions, GitBranchInfo, GitCommitInfo, GitFile,
    GitHubIssueDetail, GitHubIssueListState, GitHubIssueSummary, GitHubOperation,
    GitHubPrInfo, GitHubPrListState, GitHubPullRequestSummary, GitHubResponse,
    GitOperation, GitResponse, GitStashInfo, GitStatus,
};
use threadlane_protocol::{OrchestratorMode, ReasoningEffort};

use crate::client::{MobileDaemon, MobileEvent};
use crate::preferences;
use threadlane_client::ClientState;
use threadlane_protocol::daemon::{
    ChatMessageInfo, GitHubIssueRef, MessageRole, PermissionDecision, SessionCommand, SessionEvent,
    SessionHealth, SessionInfo,
};
use threadlane_protocol::interaction::{
    PermissionRequest, QuestionAnswer, QuestionItemAnswer, QuestionRequest,
};

/// Deep links delivered by `set_deep_link_handler` on the UIKit thread.
/// The view drains this on a short timer — the handler runs outside GPUI
/// contexts so it can only stash the URL.
static PENDING_LINKS: Mutex<Vec<String>> = Mutex::new(Vec::new());

/// Stash a `threadlane://` deep link for the view to consume.
pub fn push_deeplink(url: String) {
    PENDING_LINKS.lock().unwrap().push(url);
}

fn take_pending_links() -> Vec<String> {
    std::mem::take(&mut *PENDING_LINKS.lock().unwrap())
}

fn composer_command_session(command: &SessionCommand) -> Option<&str> {
    match command {
        SessionCommand::GetComposerOptions { session_id, .. } => session_id.as_deref(),
        SessionCommand::SetModel { session_id, .. }
        | SessionCommand::SetReasoningEffort { session_id, .. }
        | SessionCommand::SetOrchestratorMode { session_id, .. } => Some(session_id),
        _ => None,
    }
}

const PREF_HOST: &str = "threadlane.pair.host";
const PREF_PORT: &str = "threadlane.pair.port";
const PREF_TOKEN: &str = "threadlane.pair.token";

fn restore_pairing() -> Option<(String, String, String)> {
    match (
        preferences::get_string(PREF_HOST),
        preferences::get_string(PREF_PORT),
    ) {
        (Some(host), Some(port)) if !host.is_empty() && !port.is_empty() => Some((
            host,
            port,
            preferences::get_string(PREF_TOKEN).unwrap_or_default(),
        )),
        _ => None,
    }
}

fn store_pairing(host: &str, port: &str, token: &str) {
    preferences::set_string(PREF_HOST, host);
    preferences::set_string(PREF_PORT, port);
    preferences::set_string(PREF_TOKEN, token);
}

fn clear_pairing() {
    preferences::remove(PREF_HOST);
    preferences::remove(PREF_PORT);
    preferences::remove(PREF_TOKEN);
}

/// Kit icons on iOS: there is no installable `AssetSource`, so `Icon::path`
/// cannot resolve — embed the SVG bytes straight from the generated
/// `embedded` constants instead.
fn icon(bytes: &'static [u8]) -> Icon {
    Icon::default().data(bytes)
}

/// Selection/custom-answer map key: one entry per request + question item.
fn question_key(request_id: &str, item_id: &str) -> String {
    format!("{request_id}\0{item_id}")
}

#[derive(Clone, Copy, PartialEq)]
enum Screen {
    Connect,
    Main,
}

/// A destination in the main screen's bottom tab bar. Tabs only appear
/// once their surface works — the bar grows as panels land.
#[derive(Clone, Copy, PartialEq)]
enum Tab {
    Chats,
    Git,
    Issues,
    Pulls,
    Automations,
}

/// Schedule presets the automation form offers — the full calendar
/// editor is desktop-only for now.
#[derive(Clone, Copy, PartialEq)]
enum AutoSchedule {
    Manual,
    Interval,
    DailyUtc,
}

impl AutoSchedule {
    fn label(self) -> &'static str {
        match self {
            Self::Manual => "Manual",
            Self::Interval => "Every N minutes",
            Self::DailyUtc => "Daily 09:00 UTC",
        }
    }
}

/// The session whose transcript the view is watching.
struct ActiveSession {
    id: String,
    title: String,
    /// `SessionInfo::runtime_work_dir` — the execution dir `SubmitPrompt` wants.
    work_dir: std::path::PathBuf,
    /// Needed by `DeleteSession` to archive the transcript.
    session_file: std::path::PathBuf,
    /// Queued question requests; the front entry is rendered.
    /// Toggled options per question item id, for the front request.
    answers: HashMap<String, Vec<String>>,
    transcript: threadlane_ui_session::transcript::TranscriptState,
    confirm_delete: bool,
    /// Repo context mirrored from `SessionInfo` for the header meta row.
    git_branch: Option<String>,
    is_worktree: bool,
    /// Linked issue rendered as a chip that opens the Issues panel.
    github_issue: Option<GitHubIssueRef>,
}

impl ActiveSession {
    fn new(info: &SessionInfo, window: &Window) -> Self {
        Self {
            id: info.id.clone(),
            title: if info.title.trim().is_empty() {
                "Untitled session".to_string()
            } else {
                info.title.clone()
            },
            work_dir: info.runtime_work_dir.clone(),
            session_file: info.session_file.clone(),
            answers: HashMap::new(),
            transcript: threadlane_ui_session::transcript::TranscriptState::new(window),
            confirm_delete: false,
            git_branch: info.git_branch.clone(),
            is_worktree: info.is_worktree,
            github_issue: info.github_issue.clone(),
        }
    }
}

enum MobileSessionRow {
    Project(threadlane_protocol::daemon::ProjectInfo),
    Session(SessionInfo),
}

/// Root view. One `MobileDaemon` drives all traffic; `screen` picks the
/// layout and `projects`/`active` hold the rendered projection.
pub struct MobileApp {
    screen: Screen,
    host: Entity<InputState>,
    port: Entity<InputState>,
    token: Entity<InputState>,
    search: Entity<InputState>,
    composer: Entity<gpui_kit::component::input::TextareaState>,
    /// One custom-answer input per `allow_custom` question item, keyed by
    /// `question_key(request.id, item.id)` and created lazily on render.
    /// The keyboard subscription rides along so it drops with the input.
    question_inputs: HashMap<String, (Entity<InputState>, Subscription)>,
    connect_error: Option<String>,
    /// Whether a persisted pairing exists — drives the Forget button.
    saved_pairing: bool,
    sending: bool,
    pending_composer: HashMap<String, usize>,
    uncertain_prompts: HashSet<String>,
    daemon: Option<MobileDaemon>,
    /// Human-readable link state shown in the sessions header.
    link_state: String,
    /// Deep links already consumed, so reconnect flows don't re-apply them.
    seen_links: Vec<String>,
    client: ClientState,
    markdown_states:
        HashMap<(SharedString, String), threadlane_ui_session::markdown::MarkdownRenderState>,
    active: Option<ActiveSession>,
    sessions_list: ListState,
    session_rows: Vec<MobileSessionRow>,
    project_git: HashMap<std::path::PathBuf, GitStatus>,
    /// Which tab the main screen shows.
    tab: Tab,
    /// Slide-over drawer with the project list and connection controls.
    sidebar_open: bool,
    /// Commit-message composer on the Git tab.
    git_message: Entity<gpui_kit::component::input::TextareaState>,
    /// New-branch name input on the Git tab.
    git_branch: Entity<InputState>,
    /// Open diff overlay: `(title, unified diff text)`.
    git_diff: Option<(String, String)>,
    /// Diff request in flight — the next `GitResponse::Text` fills `git_diff`.
    git_diff_pending: Option<String>,
    /// Expanded commit/stash objects on the Git tab, keyed `commit-<sha>`
    /// or `stash-<index>`.
    git_expanded: HashSet<String>,
    /// Files loaded for an expanded object, same key space; presence of
    /// the key distinguishes loaded-empty from still-loading.
    git_object_files: HashMap<String, Vec<GitFile>>,
    /// Issue list for the selected project; `None` until the first
    /// `ListIssues` reply lands.
    issues: Option<Vec<GitHubIssueSummary>>,
    issue_filter: GitHubIssueListState,
    issue_search: Entity<InputState>,
    /// Open issue detail overlay (body + comments).
    issue_detail: Option<GitHubIssueDetail>,
    /// `InspectIssue` request in flight.
    issue_detail_pending: Option<u64>,
    /// Confirm strip inside the detail overlay before `DeleteIssue`.
    issue_confirm_delete: bool,
    issue_comment: Entity<gpui_kit::component::input::TextareaState>,
    /// New-issue form state.
    new_issue_open: bool,
    issue_title: Entity<InputState>,
    issue_body: Entity<gpui_kit::component::input::TextareaState>,
    /// Pull-request list + detail for the selected project.
    prs: Option<Vec<GitHubPullRequestSummary>>,
    pr_filter: GitHubPrListState,
    pr_search: Entity<InputState>,
    pr_detail: Option<GitHubPrInfo>,
    pr_detail_pending: Option<u64>,
    pr_comment: Entity<gpui_kit::component::input::TextareaState>,
    /// Automation editor overlay: `None` when closed, `Some(definition)`
    /// carries the definition being edited (or the draft skeleton for a
    /// new one — `revision == 0` marks it new).
    auto_editing: Option<Definition>,
    auto_name: Entity<InputState>,
    auto_model: Entity<InputState>,
    auto_effort: Entity<InputState>,
    auto_minutes: Entity<InputState>,
    auto_prompt: Entity<gpui_kit::component::input::TextareaState>,
    auto_schedule: AutoSchedule,
    auto_worktree: bool,
    auto_enabled: bool,
    /// Definition id awaiting its delete-confirm strip.
    auto_delete_confirm: Option<String>,
    /// A queued-message cancel awaiting its reply or the journaled
    /// `QueuedEntryCancelled`: the echo row stays until the daemon
    /// confirms the entry left the queue. `(session_id, entry_id)`.
    pending_queued_cancel: Option<(String, String)>,
    /// Run id awaiting its delete-confirm strip.
    auto_run_delete_confirm: Option<String>,
    models: Vec<ComposerModel>,
    session_drafts: Vec<SessionInfo>,
    selected_model: Option<String>,
    effort: Option<ReasoningEffort>,
    mode: OrchestratorMode,
    _link_task: Task<()>,
    _pump: Option<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

impl MobileApp {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let host = cx.new(|cx| InputState::new(window, cx).placeholder("192.168.x.x"));
        let port = cx.new(|cx| InputState::new(window, cx).placeholder("port"));
        let token = cx.new(|cx| InputState::new(window, cx).placeholder("pairing token"));
        let search = cx.new(|cx| InputState::new(window, cx).placeholder("Search chats"));
        let git_message = cx.new(|cx| {
            gpui_kit::component::input::TextareaState::new(window, cx)
                .placeholder("Commit message")
                .auto_grow(1, 4)
                .soft_wrap(true)
        });
        let git_branch =
            cx.new(|cx| InputState::new(window, cx).placeholder("New branch name"));
        let issue_search = cx.new(|cx| InputState::new(window, cx).placeholder("Search issues"));
        let issue_title = cx.new(|cx| InputState::new(window, cx).placeholder("Issue title"));
        let issue_body = cx.new(|cx| {
            gpui_kit::component::input::TextareaState::new(window, cx)
                .placeholder("Describe the issue")
                .auto_grow(2, 8)
                .soft_wrap(true)
        });
        let issue_comment = cx.new(|cx| {
            gpui_kit::component::input::TextareaState::new(window, cx)
                .placeholder("Add a comment")
                .auto_grow(1, 4)
                .soft_wrap(true)
        });
        let pr_search = cx.new(|cx| InputState::new(window, cx).placeholder("Search pull requests"));
        let pr_comment = cx.new(|cx| {
            gpui_kit::component::input::TextareaState::new(window, cx)
                .placeholder("Add a comment")
                .auto_grow(1, 4)
                .soft_wrap(true)
        });
        let auto_name =
            cx.new(|cx| InputState::new(window, cx).placeholder("Automation name"));
        let auto_model = cx.new(|cx| {
            InputState::new(window, cx).placeholder("Model, e.g. claude-sonnet-4-5")
        });
        let auto_effort =
            cx.new(|cx| InputState::new(window, cx).placeholder("Effort, e.g. medium"));
        let auto_minutes =
            cx.new(|cx| InputState::new(window, cx).placeholder("Minutes between runs"));
        let auto_prompt = cx.new(|cx| {
            gpui_kit::component::input::TextareaState::new(window, cx)
                .placeholder("Prompt the run executes")
                .auto_grow(3, 10)
                .soft_wrap(true)
        });
        let composer = cx.new(|cx| {
            gpui_kit::component::input::TextareaState::new(window, cx)
                .placeholder("Message")
                .auto_grow(1, 8)
                .submit_on_enter(true)
                .soft_wrap(true)
        });
        let mut subscriptions = [
            &host,
            &port,
            &token,
            &search,
            &git_branch,
            &issue_search,
            &issue_title,
            &pr_search,
            &auto_name,
            &auto_model,
            &auto_effort,
            &auto_minutes,
        ]
        .iter()
            .map(|input| {
                cx.subscribe_in(input, window, |_, _, event, _, _| match event {
                    InputEvent::Focus => gpui_mobile::show_keyboard(),
                    InputEvent::Blur => gpui_mobile::hide_keyboard(),
                    _ => {}
                })
            })
            .collect::<Vec<_>>();
        subscriptions.push(
            cx.subscribe_in(
                &composer,
                window,
                |this, _, event, window, cx| match event {
                    InputEvent::Focus => gpui_mobile::show_keyboard(),
                    InputEvent::Blur => gpui_mobile::hide_keyboard(),
                    InputEvent::PressEnter { .. } => this.submit_composer(window, cx),
                    _ => {}
                },
            ),
        );
        for entity in [&git_message, &issue_body, &issue_comment, &pr_comment, &auto_prompt] {
            subscriptions.push(
                cx.subscribe_in(entity, window, |_, _, event, _, _| {
                    match event {
                        InputEvent::Focus => gpui_mobile::show_keyboard(),
                        InputEvent::Blur => gpui_mobile::hide_keyboard(),
                        _ => {}
                    }
                }),
            );
        }

        subscriptions.push(
            cx.subscribe_in(&search, window, |this, input, event, _, cx| {
                if matches!(event, InputEvent::Change) {
                    this.client.search_query = input.read(cx).value().to_string();
                    this.refresh_session_rows();
                    cx.notify();
                }
            }),
        );

        // Poll for pairing deep links arriving while the app runs — the
        // UIKit handler runs outside GPUI and can only stash them.
        let link_task = cx.spawn_in(window, async move |this, cx| {
            // A link may have launched the app before this task started.
            if let Ok(Some(url)) = gpui_mobile::packages::deeplink::get_initial_link() {
                let _ = this.update_in(cx, |this, window, cx| {
                    this.apply_pairing_link(&url, window, cx);
                });
            }
            let mut tick = 0u64;
            loop {
                cx.background_executor()
                    .timer(Duration::from_millis(500))
                    .await;
                tick += 1;
                let links = take_pending_links();
                // The daemon only publishes `ProjectChanged` on attach/detach
                // or when asked, so new sessions and renames inside an
                // attached project never reach the client — re-ask on a
                // slow cadence to keep the list current.
                let refresh_projects = tick % 30 == 0;
                if links.is_empty() && !refresh_projects {
                    continue;
                }
                let _ = this.update_in(cx, |this, window, cx| {
                    for url in links {
                        this.apply_pairing_link(&url, window, cx);
                    }
                    if refresh_projects {
                        if let Some(daemon) = &this.daemon {
                            if daemon.is_connected() {
                                daemon.send(SessionCommand::GetProjects);
                                if let Some(work_dir) = &this.client.sidebar_project_filter {
                                    daemon.request(SessionCommand::GitRequest {
                                        work_dir: work_dir.clone(),
                                        operation: GitOperation::Inspect { sync_remote: false },
                                    });
                                }
                            }
                        }
                    }
                });
            }
        });

        let saved = restore_pairing();
        let saved_pairing = saved.is_some();
        let mut app = Self {
            screen: Screen::Connect,
            host,
            port,
            token,
            composer,
            search,
            question_inputs: HashMap::new(),
            connect_error: None,
            saved_pairing,
            sending: false,
            pending_composer: HashMap::new(),
            uncertain_prompts: HashSet::new(),
            daemon: None,
            link_state: "Disconnected".to_string(),
            seen_links: Vec::new(),
            client: ClientState::default(),
            markdown_states: HashMap::new(),
            active: None,
            sessions_list: ListState::new(0, ListAlignment::Top, window.rem_size() * 4.5),
            session_rows: Vec::new(),
            project_git: HashMap::new(),
            tab: Tab::Chats,
            sidebar_open: false,
            git_message,
            git_branch,
            git_diff: None,
            git_diff_pending: None,
            git_expanded: HashSet::new(),
            git_object_files: HashMap::new(),
            issues: None,
            issue_filter: GitHubIssueListState::Open,
            issue_search,
            issue_detail: None,
            issue_detail_pending: None,
            issue_confirm_delete: false,
            issue_comment,
            new_issue_open: false,
            issue_title,
            issue_body,
            prs: None,
            pr_filter: GitHubPrListState::Open,
            pr_search,
            pr_detail: None,
            pr_detail_pending: None,
            pr_comment,
            auto_editing: None,
            auto_name,
            auto_model,
            auto_effort,
            auto_minutes,
            auto_prompt,
            auto_schedule: AutoSchedule::Interval,
            auto_worktree: true,
            auto_enabled: true,
            auto_delete_confirm: None,
            pending_queued_cancel: None,
            auto_run_delete_confirm: None,
            models: Vec::new(),
            session_drafts: Vec::new(),
            selected_model: None,
            effort: None,
            mode: OrchestratorMode::Normal,
            _link_task: link_task,
            _pump: None,
            _subscriptions: subscriptions,
        };
        // Restore the saved pairing and reconnect — the fields stay visible
        // on the Connect screen so a failed attempt can be edited.
        if let Some((saved_host, saved_port, saved_token)) = saved {
            app.host
                .update(cx, |input, cx| input.set_value(saved_host, window, cx));
            app.port
                .update(cx, |input, cx| input.set_value(saved_port, window, cx));
            app.token
                .update(cx, |input, cx| input.set_value(saved_token, window, cx));
            app.connect_now(window, cx);
        }
        app
    }

    /// Parse `threadlane://pair?host=…&port=…&token=…` and connect.
    fn apply_pairing_link(&mut self, url: &str, window: &mut Window, cx: &mut Context<Self>) {
        if self.seen_links.iter().any(|seen| seen == url) {
            return;
        }
        let Ok(parsed) = url::Url::parse(url) else {
            return;
        };
        if parsed.scheme() != "threadlane" || parsed.host_str() != Some("pair") {
            return;
        }
        self.seen_links.push(url.to_string());
        let query = |key: &str| {
            parsed
                .query_pairs()
                .find(|(name, _)| name == key)
                .map(|(_, value)| value.to_string())
        };
        // A link that redirects to a different endpoint but carries no token
        // must not inherit the credential saved for another host — the Bearer
        // token is only valid for the endpoint it was paired with.
        let endpoint_changed = query("host")
            .is_some_and(|host| host != self.host.read(cx).value().trim().to_owned())
            || query("port")
                .is_some_and(|port| port != self.port.read(cx).value().trim().to_owned());
        if endpoint_changed && query("token").is_none() {
            self.token
                .update(cx, |input, cx| input.set_value("", window, cx));
        }
        if let Some(host) = query("host") {
            self.host
                .update(cx, |input, cx| input.set_value(host, window, cx));
        }
        if let Some(port) = query("port") {
            self.port
                .update(cx, |input, cx| input.set_value(port, window, cx));
        }
        if let Some(token) = query("token") {
            self.token
                .update(cx, |input, cx| input.set_value(token, window, cx));
        }
        self.connect_now(window, cx);
    }

    fn connect_now(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let host = self.host.read(cx).value().trim().to_owned();
        let port = self.port.read(cx).value().trim().to_owned();
        let token = self.token.read(cx).value().trim().to_owned();
        if host.is_empty() || port.is_empty() {
            self.connect_error =
                Some("Enter the host and port shown in the desktop QR dialog".into());
            cx.notify();
            return;
        }
        let url = format!("ws://{host}:{port}");
        let token = (!token.is_empty()).then_some(token);
        let daemon = match MobileDaemon::connect(url, token) {
            Ok(daemon) => daemon,
            Err(error) => {
                self.connect_error = Some(error);
                cx.notify();
                return;
            }
        };
        self.link_state = "Connecting…".to_string();
        self.connect_error = None;
        self.client.projects.clear();
        self.session_drafts.clear();
        self.project_git.clear();
        self.models.clear();
        self.selected_model = None;
        self.effort = None;
        self.active = None;
        self.git_diff = None;
        self.git_diff_pending = None;
        self.git_expanded.clear();
        self.git_object_files.clear();
        self.issues = None;
        self.issue_detail = None;
        self.issue_detail_pending = None;
        self.new_issue_open = false;
        self.prs = None;
        self.pr_detail = None;
        self.pr_detail_pending = None;
        self.daemon = Some(daemon);
        self.screen = Screen::Main;
        self.tab = Tab::Chats;
        self.sidebar_open = false;

        // Pump the wire onto the view until the daemon is dropped.
        let mut events = self.daemon.as_mut().and_then(|daemon| daemon.take_events());
        self._pump = Some(cx.spawn_in(window, async move |this, cx| {
            let Some(events) = events.as_mut() else {
                return;
            };
            while let Some(event) = events.recv().await {
                if this
                    .update_in(cx, |this, window, cx| this.apply_event(event, window, cx))
                    .is_err()
                {
                    return;
                }
            }
        }));
        cx.notify();
    }

    fn disconnect(&mut self, cx: &mut Context<Self>) {
        // Dropping the client closes its command channel and ends the
        // driver's reconnect loop.
        self.daemon = None;
        self._pump = None;
        self.sending = false;
        self.pending_composer.clear();
        self.client.projects.clear();
        self.active = None;
        self.client.pending_permissions.clear();
        self.client.pending_questions.clear();
        self.client.queued_questions.clear();
        self.link_state = "Disconnected".to_string();
        self.tab = Tab::Chats;
        self.sidebar_open = false;
        self.git_diff = None;
        self.git_diff_pending = None;
        self.git_expanded.clear();
        self.git_object_files.clear();
        self.issues = None;
        self.issue_detail = None;
        self.issue_detail_pending = None;
        self.new_issue_open = false;
        self.prs = None;
        self.pr_detail = None;
        self.pr_detail_pending = None;
        self.screen = Screen::Connect;
        cx.notify();
    }

    /// Clear the persisted pairing and the fields — the next launch lands
    /// on a blank Connect screen instead of auto-reconnecting.
    fn forget_pairing(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        clear_pairing();
        self.saved_pairing = false;
        for input in [&self.host, &self.port, &self.token] {
            input.update(cx, |input, cx| input.set_value("", window, cx));
        }
        cx.notify();
    }

    fn open_session(&mut self, info: &SessionInfo, window: &mut Window, cx: &mut Context<Self>) {
        if !self.session_drafts.iter().any(|draft| draft.id == info.id) {
            if let Some(daemon) = &self.daemon {
                daemon.send(SessionCommand::GetSessionSnapshot {
                    session_id: info.id.clone(),
                });
            }
        }
        self.request_composer(SessionCommand::GetComposerOptions {
            work_dir: info.work_dir.clone(),
            session_id: Some(info.id.clone()),
        });
        self.models.clear();
        self.selected_model = None;
        self.effort = None;
        self.markdown_states.clear();
        self.client.select_session(info);
        let key = (
            self.client.active_work_dir.clone(),
            self.client.active_session_id.clone(),
        );
        let draft = self.client.composer_drafts.remove(&key).unwrap_or_default();
        self.composer
            .update(cx, |input, cx| input.set_value(draft.text, window, cx));
        self.active = Some(ActiveSession::new(info, window));
        self.tab = Tab::Chats;
        cx.notify();
    }

    fn begin_session(&mut self, cx: &mut Context<Self>) {
        if self.sending {
            return;
        }
        if self.active.is_some() {
            self.client.composer_drafts.insert(
                (
                    self.client.active_work_dir.clone(),
                    self.client.active_session_id.clone(),
                ),
                threadlane_client::ComposerDraft {
                    text: self.composer.read(cx).value().to_string(),
                    images: Vec::new(),
                },
            );
        }
        let project = self
            .client
            .sidebar_project_filter
            .clone()
            .or_else(|| self.client.active_work_dir.clone());
        if let (Some(work_dir), Some(daemon)) = (project, &self.daemon) {
            daemon.request(SessionCommand::BeginSession { work_dir });
            self.sending = true;
        }
        cx.notify();
    }

    fn apply_response(
        &mut self,
        command: &SessionCommand,
        response: CommandResponse,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match response {
            CommandResponse::SessionDraft { session } => {
                self.sending = false;
                self.markdown_states.clear();
                self.models.clear();
                self.selected_model = None;
                self.effort = None;
                // A draft has no transcript until its first accepted prompt.
                self.session_drafts.push(session.clone());
                if let Some(project) = self
                    .client
                    .projects
                    .iter_mut()
                    .find(|project| project.work_dir == session.work_dir)
                {
                    project.sessions.push(session.clone());
                }
                self.refresh_session_rows();
                self.client.select_session(&session);
                self.client.messages = Default::default();
                self.composer
                    .update(cx, |input, cx| input.set_value("", window, cx));
                self.active = Some(ActiveSession::new(&session, window));
                self.tab = Tab::Chats;
                self.request_composer(SessionCommand::GetComposerOptions {
                    work_dir: session.work_dir,
                    session_id: Some(session.id),
                });
            }
            CommandResponse::ComposerOptions {
                models,
                model,
                effort,
                mode,
            } => {
                if let SessionCommand::GetComposerOptions {
                    work_dir,
                    session_id,
                } = command
                {
                    if self.client.active_work_dir.as_ref() != Some(work_dir)
                        || &self.client.active_session_id != session_id
                    {
                        return;
                    }
                }
                self.models = models;
                self.selected_model = Some(model);
                self.effort = Some(effort);
                self.mode = mode;
            }
            CommandResponse::Automation { response } => {
                let AutomationResponse::Projection { projection } = response;
                self.client.automation = projection;
                if matches!(
                    command,
                    SessionCommand::AutomationRequest {
                        command: AutomationCommand::Save { .. }
                    }
                ) {
                    self.auto_editing = None;
                }
            }
            CommandResponse::GitHub { response } => {
                let SessionCommand::GitHubRequest { operation, .. } = command else {
                    return;
                };
                match response {
                    GitHubResponse::Issues { issues } => {
                        self.issues = Some(issues);
                    }
                    GitHubResponse::Issue { detail } => {
                        self.issue_detail_pending = None;
                        self.issue_detail = Some(detail);
                    }
                    GitHubResponse::PullRequests { prs } => {
                        self.prs = Some(prs);
                    }
                    GitHubResponse::PullRequest { pr } => {
                        self.pr_detail_pending = None;
                        self.pr_detail = Some(pr);
                    }
                    GitHubResponse::Text { text } => {
                        if let Some(title) = self.git_diff_pending.take() {
                            self.git_diff = Some((title, text));
                        }
                    }
                    GitHubResponse::Number { number } => {
                        self.new_issue_open = false;
                        self.client.session_status = Some(format!("Created issue #{number}"));
                        self.refresh_issues();
                        self.send_github(GitHubOperation::InspectIssue { number });
                        self.issue_detail_pending = Some(number);
                    }
                    GitHubResponse::Action { message } => {
                        if let Some(message) = message {
                            self.client.session_status = Some(message);
                        }
                        // Refresh whatever a mutation may have touched: the
                        // list, and the open detail's comment/state.
                        match operation {
                            GitHubOperation::CommentIssue { .. }
                            | GitHubOperation::SetIssueState { .. } => {
                                if let Some(detail) = &self.issue_detail {
                                    let number = detail.summary.issue.number;
                                    self.issue_detail_pending = Some(number);
                                    self.send_github(GitHubOperation::InspectIssue { number });
                                }
                                self.refresh_issues();
                            }
                            GitHubOperation::DeleteIssue { .. } => {
                                self.issue_detail = None;
                                self.refresh_issues();
                            }
                            GitHubOperation::CommentPullRequest { .. } => {
                                if let Some(pr) = &self.pr_detail {
                                    let number = pr.number;
                                    self.pr_detail_pending = Some(number);
                                    self.send_github(GitHubOperation::InspectPullRequest {
                                        number,
                                    });
                                }
                            }
                            _ => {}
                        }
                    }
                }
            }
            CommandResponse::Git { response } => {
                let SessionCommand::GitRequest { work_dir, operation } = command else {
                    return;
                };
                match response {
                    GitResponse::Status { status } => {
                        self.project_git.insert(work_dir.clone(), *status);
                        self.refresh_session_rows();
                    }
                    GitResponse::Action { outcome } => {
                        if let Some(error) = &outcome.action_error {
                            self.client.session_status = Some(error.clone());
                        } else if let Some(message) = &outcome.message {
                            self.client.session_status = Some(message.clone());
                        }
                        if let Ok(status) = outcome.status {
                            self.project_git.insert(work_dir.clone(), status);
                            self.refresh_session_rows();
                        }
                        if outcome.action_error.is_none()
                            && matches!(operation, GitOperation::Commit { .. })
                        {
                            self.git_message.update(cx, |input, cx| {
                                input.set_value("", window, cx)
                            });
                        }
                    }
                    GitResponse::Files { files } => {
                        let key = match operation {
                            GitOperation::CommitFiles { sha } => Some(format!("commit-{sha}")),
                            GitOperation::StashFiles { index } => Some(format!("stash-{index}")),
                            _ => None,
                        };
                        if let Some(key) = key {
                            self.git_object_files.insert(key, files);
                        }
                    }
                    GitResponse::Text { text } => {
                        if let Some(title) = self.git_diff_pending.take() {
                            self.git_diff = Some((title, text));
                        }
                    }
                    _ => {}
                }
            }
            CommandResponse::CancelledQueuedMessage {
                session_id,
                entry_id,
                ..
            } => {
                let echo_id = format!("queued-user-{session_id}-{entry_id}");
                self.client.messages_mut().retain(|m| m.id != echo_id);
                self.pending_queued_cancel = None;
                self.client.session_status = Some("Queued message removed".to_string());
            }
            CommandResponse::Ack => {
                let session_id = match command {
                    SessionCommand::SetModel { session_id, .. }
                    | SessionCommand::SetReasoningEffort { session_id, .. }
                    | SessionCommand::SetOrchestratorMode { session_id, .. } => Some(session_id),
                    _ => None,
                };
                if session_id.is_some_and(|id| self.client.active_session_id.as_ref() != Some(id)) {
                    return;
                }
                match command {
                    SessionCommand::SetModel { model, .. } => {
                        self.selected_model = Some(model.clone());
                        if let Some(efforts) = self
                            .models
                            .iter()
                            .find(|m| m.id == *model)
                            .map(|m| &m.efforts)
                        {
                            if !self.effort.is_some_and(|effort| efforts.contains(&effort)) {
                                self.effort = efforts.first().copied();
                            }
                        }
                    }
                    SessionCommand::SetReasoningEffort { effort, .. } => {
                        self.effort = Some(*effort)
                    }
                    SessionCommand::SetOrchestratorMode { mode, .. } => self.mode = *mode,
                    _ => {}
                }
            }
            _ => {}
        }
    }

    fn request_composer(&mut self, command: SessionCommand) {
        if let Some(daemon) = &self.daemon {
            if let Some(id) = composer_command_session(&command) {
                *self.pending_composer.entry(id.to_owned()).or_default() += 1;
            }
            daemon.request(command);
        }
    }

    fn composer_pending(&self) -> bool {
        self.client
            .active_session_id
            .as_ref()
            .is_some_and(|id| self.pending_composer.get(id).copied().unwrap_or_default() > 0)
    }

    fn composer_options(&self, cx: &mut Context<Self>) -> AnyElement {
        let entity = cx.entity();
        let models = self.models.clone();
        let selected = self.selected_model.clone();
        let model_label = models
            .iter()
            .find(|m| Some(&m.id) == selected.as_ref())
            .map(|m| m.label.clone())
            .unwrap_or_else(|| {
                selected
                    .clone()
                    .filter(|model| !model.is_empty())
                    .unwrap_or_else(|| "Connect a provider on desktop".into())
            });
        let efforts = models
            .iter()
            .find(|m| Some(&m.id) == selected.as_ref())
            .map(|m| m.efforts.clone())
            .unwrap_or_default();
        let effort = self.effort;
        let mode = self.mode;
        let enabled =
            self.daemon.as_ref().is_some_and(|d| d.is_connected()) && !self.client.is_generating && !self.composer_pending();
        let model_entity = entity.clone();
        let effort_entity = entity.clone();
        div().flex().flex_col().w_full().gap_1()
            .child(Button::new("mobile-model").ghost().h_11().w_full().label(format!("{model_label} ▾"))
                .accessibility_label("Select model").dropdown_caret(true).disabled(!enabled || models.is_empty())
                .dropdown_menu_with_anchor(Anchor::BottomLeft, move |menu, _, _| {
                    models.iter().fold(menu.scrollable(true), |menu, model| {
                        let entity = model_entity.clone(); let id = model.id.clone();
                        menu.item(PopupMenuItem::new(format!("{}{}", if Some(&model.id) == selected.as_ref() { "✓ " } else { "" }, model.label)).checked(Some(&model.id) == selected.as_ref())
                            .on_click(move |_, _, cx| { entity.update(cx, |this, cx| {
                                if !this.composer_pending() {
                                    if let Some(active) = &this.active {
                                        let session_id = active.id.clone();
                                        this.request_composer(SessionCommand::SetModel { session_id, model: id.clone() });
                                    }
                                } cx.notify();
                            }); }))
                    })
                }))
            .child(div().flex().items_center().gap_1()
                .when(!efforts.is_empty(), |this| this.child(
                    Button::new("mobile-effort").ghost().h_11().label(format!("{} ▾", effort.map(|e| e.label()).unwrap_or("Effort")))
                        .accessibility_label("Reasoning effort").dropdown_caret(true).disabled(!enabled)
                        .dropdown_menu_with_anchor(Anchor::BottomLeft, move |menu, _, _| {
                            efforts.iter().fold(menu, |menu, value| {
                                let entity = effort_entity.clone(); let value = *value;
                                menu.item(PopupMenuItem::new(format!("{}{}", if Some(value) == effort { "✓ " } else { "" }, value.label())).checked(Some(value) == effort)
                                    .on_click(move |_, _, cx| { entity.update(cx, |this, cx| {
                                        if !this.composer_pending() {
                                            if let Some(active) = &this.active {
                                                let session_id = active.id.clone();
                                                this.request_composer(SessionCommand::SetReasoningEffort { session_id, effort: value });
                                            }
                                        } cx.notify();
                                    }); }))
                            })
                        })))
                .child(Button::new("mobile-mode").ghost().h_11().label(format!("{} ▾", mode.label())).accessibility_label("Agent mode")
                    .dropdown_caret(true).disabled(!enabled)
                    .dropdown_menu_with_anchor(Anchor::BottomLeft, move |menu, _, _| {
                        [OrchestratorMode::Normal, OrchestratorMode::Fusion].into_iter().fold(menu, |menu, value| {
                            let entity = entity.clone();
                            menu.item(PopupMenuItem::new(format!("{}{}", if value == mode { "✓ " } else { "" }, value.label())).checked(value == mode)
                                .on_click(move |_, _, cx| { entity.update(cx, |this, cx| {
                                    if !this.composer_pending() {
                                        if let Some(active) = &this.active {
                                            let session_id = active.id.clone();
                                            this.request_composer(SessionCommand::SetOrchestratorMode { session_id, mode: value });
                                        }
                                    } cx.notify();
                                }); }))
                        })
                    })))
            .into_any_element()
    }

    fn apply_event(&mut self, event: MobileEvent, window: &mut Window, cx: &mut Context<Self>) {
        if let MobileEvent::CommandResult { command, .. } = &event {
            if let Some(id) = composer_command_session(command) {
                if let Some(count) = self.pending_composer.get_mut(id) {
                    *count = count.saturating_sub(1);
                }
            }
        }
        match event {
            MobileEvent::Connected => {
                if let Some(daemon) = &self.daemon {
                    daemon.send(SessionCommand::GetProjects);
                    if let Some(active) = &self.active {
                        daemon.send(SessionCommand::GetSessionSnapshot {
                            session_id: active.id.clone(),
                        });
                    }
                }
                if let (Some(work_dir), Some(session_id)) = (
                    self.client.active_work_dir.clone(),
                    self.client.active_session_id.clone(),
                ) {
                    self.selected_model = None;
                    self.request_composer(SessionCommand::GetComposerOptions {
                        work_dir,
                        session_id: Some(session_id),
                    });
                }
                self.link_state = "Live".to_string();
                self.connect_error = None;
                // Remember the pairing so the next launch reconnects.
                let host = self.host.read(cx).value().trim().to_owned();
                let port = self.port.read(cx).value().trim().to_owned();
                let token = self.token.read(cx).value().trim().to_owned();
                store_pairing(&host, &port, &token);
                self.saved_pairing = true;
            }
            MobileEvent::Connecting => {
                self.link_state = "Connecting…".into();
            }
            MobileEvent::Reconnecting => {
                self.link_state = "Reconnecting…".to_string();
            }
            MobileEvent::Fatal(error) => {
                self.link_state = "Failed".to_string();
                self.connect_error = Some(error);
                self.screen = Screen::Connect;
            }
            MobileEvent::CommandResult { command, result } => match result {
                Ok(response) => {
                    self.apply_response(&command, response, window, cx);
                    match command {
                        SessionCommand::SubmitPrompt {
                            session_id, text, ..
                        } => {
                            self.sending = false;
                            if let Some(daemon) = &self.daemon {
                                daemon.send(SessionCommand::GetProjects);
                            }
                            let accepted_id =
                                format!("sent-user-{session_id}-{}", self.client.messages.len());
                            if let Some(message) = self
                                .client
                                .messages_mut()
                                .iter_mut()
                                .find(|m| m.id == format!("pending-user-{session_id}"))
                            {
                                message.id = accepted_id;
                            }
                            let key = (
                                self.client
                                    .projects
                                    .iter()
                                    .find(|p| p.sessions.iter().any(|s| s.id == session_id))
                                    .map(|p| p.work_dir.clone()),
                                Some(session_id.clone()),
                            );
                            if self.client.active_session_id.as_deref() == Some(&session_id)
                                && self.composer.read(cx).value().trim() == text
                            {
                                self.composer
                                    .update(cx, |input, cx| input.set_value("", window, cx));
                            }
                            if self
                                .client
                                .composer_drafts
                                .get(&key)
                                .is_some_and(|d| d.text.trim() == text)
                            {
                                self.client.composer_drafts.remove(&key);
                            }
                        }
                        SessionCommand::AnswerPermission {
                            session_id,
                            request_id,
                            ..
                        } => {
                            if self
                                .client
                                .pending_permissions
                                .get(&session_id)
                                .is_some_and(|r| r.id == request_id)
                            {
                                self.client.pending_permissions.remove(&session_id);
                            }
                        }
                        SessionCommand::AnswerQuestion { session_id, answer } => {
                            if self
                                .client
                                .pending_questions
                                .get(&session_id)
                                .is_some_and(|r| r.id == answer.request_id)
                            {
                                self.client.pop_question(&session_id);
                            }
                            let prefix = format!("{}\0", answer.request_id);
                            if let Some(active) = &mut self.active {
                                if active.id == session_id {
                                    active.answers.retain(|key, _| !key.starts_with(&prefix));
                                }
                            }
                            self.question_inputs
                                .retain(|key, _| !key.starts_with(&prefix));
                        }
                        _ => {}
                    }
                }
                Err(error) => {
                    if matches!(
                        command,
                        SessionCommand::SubmitPrompt { .. } | SessionCommand::BeginSession { .. }
                    ) {
                        self.sending = false;
                    }
                    if let SessionCommand::SubmitPrompt { session_id, .. } = &command {
                        if error
                            .starts_with(threadlane_client::RemoteDaemon::UNKNOWN_REQUEST_OUTCOME)
                        {
                            // Dispatch may still be running. Keep the echo and draft,
                            // and require the user to check before submitting again.
                            self.uncertain_prompts.insert(session_id.clone());
                            cx.notify();
                            return;
                        }
                        self.client
                            .messages_mut()
                            .retain(|m| m.id != format!("pending-user-{session_id}"));
                        if let Some(active) = &mut self.active {
                            active.transcript.sync(
                                self.client.messages.clone(),
                                self.client.is_generating,
                                false,
                                true,
                            );
                        }
                    }
                    if matches!(
                        command,
                        SessionCommand::SetModel { .. }
                            | SessionCommand::SetReasoningEffort { .. }
                            | SessionCommand::SetOrchestratorMode { .. }
                    ) {
                        if let (Some(work_dir), Some(session_id)) = (
                            self.client.active_work_dir.clone(),
                            self.client.active_session_id.clone(),
                        ) {
                            self.selected_model = None;
                            self.request_composer(SessionCommand::GetComposerOptions {
                                work_dir,
                                session_id: Some(session_id),
                            });
                        }
                    }
                    if let SessionCommand::GitRequest { operation, .. } = &command {
                        self.git_diff_pending = None;
                        // A background Inspect failing is not worth surfacing —
                        // the panel keeps showing its last status. Everything
                        // else the user asked for, so the error belongs near
                        // the surface they acted on.
                        if !matches!(operation, GitOperation::Inspect { .. }) {
                            self.client.session_status = Some(error.clone());
                        }
                        return;
                    }
                    if let SessionCommand::GitHubRequest { .. } = &command {
                        self.git_diff_pending = None;
                        self.issue_detail_pending = None;
                        self.pr_detail_pending = None;
                        self.client.session_status = Some(error.clone());
                        return;
                    }
                    if let SessionCommand::AutomationRequest { .. } = &command {
                        self.client.session_status = Some(error.clone());
                        return;
                    }
                    if let SessionCommand::CancelQueuedMessage { .. } = &command {
                        // The echo stays: the entry never left the queue.
                        self.pending_queued_cancel = None;
                        self.client.session_status =
                            Some(format!("Could not remove queued message: {error}"));
                        return;
                    }
                    self.connect_error = Some(error.clone());
                    self.client.session_status = Some(error);
                }
            },
            MobileEvent::Event(event) => self.apply_session_event(event, cx),
        }
        cx.notify();
    }

    fn apply_session_event(&mut self, event: SessionEvent, cx: &mut Context<Self>) {
        let projects_changed = matches!(
            event,
            SessionEvent::ProjectChanged { .. } | SessionEvent::SessionRemoved { .. }
        );
        let mut event = event;
        if let SessionEvent::SessionRemoved { session_id, .. } = &event {
            self.session_drafts.retain(|draft| draft.id != *session_id);
        }
        match &event {
            // Bind the optimistic `queued-user-{session}` echo to its durable
            // queue entry so the row's steer/edit/remove controls appear.
            SessionEvent::FollowUpQueued {
                session_id,
                entry_id,
            } => {
                let pending_id = format!("queued-user-{session_id}");
                if let Some(message) = self
                    .client
                    .messages_mut()
                    .iter_mut()
                    .find(|m| m.id == pending_id)
                {
                    message.id = format!("queued-user-{session_id}-{entry_id}");
                }
            }
            // The entry left the daemon's queue — ours or another client's
            // cancel: drop the retained echo and settle the parked intent.
            SessionEvent::QueuedEntryCancelled {
                session_id,
                entry_id,
                ..
            } => {
                let echo_id = format!("queued-user-{session_id}-{entry_id}");
                self.client.messages_mut().retain(|m| m.id != echo_id);
                if self
                    .pending_queued_cancel
                    .as_ref()
                    .is_some_and(|(sid, eid)| sid == session_id && eid == entry_id)
                {
                    self.pending_queued_cancel = None;
                    self.client.session_status = Some("Queued message removed".to_string());
                }
            }
            _ => {}
        }
        if let SessionEvent::ProjectChanged { project } = &mut event {
            self.session_drafts.retain(|draft| {
                !project
                    .sessions
                    .iter()
                    .any(|session| session.id == draft.id)
            });
            project.sessions.extend(
                self.session_drafts
                    .iter()
                    .filter(|draft| draft.work_dir == project.work_dir)
                    .cloned(),
            );
            if !self.project_git.contains_key(&project.work_dir) {
                if let Some(daemon) = &self.daemon {
                    daemon.request(SessionCommand::GitRequest {
                        work_dir: project.work_dir.clone(),
                        operation: GitOperation::Inspect { sync_remote: false },
                    });
                }
            }
        }
        let commands = self.client.apply_event(event);
        if projects_changed {
            self.refresh_session_rows();
        }
        if let Some(daemon) = &self.daemon {
            for command in commands {
                daemon.send(command);
            }
        }
        if self.client.active_session_id.is_none() && self.active.is_some() {
            self.active = None;
        }
        if let Some(active) = &mut self.active {
            if let Some(info) = self
                .client
                .projects
                .iter()
                .flat_map(|p| &p.sessions)
                .find(|s| s.id == active.id)
            {
                if !info.title.trim().is_empty() {
                    active.title = info.title.clone();
                }
                active.git_branch = info.git_branch.clone();
                active.is_worktree = info.is_worktree;
                active.github_issue = info.github_issue.clone();
            }
            active.transcript.sync(
                self.client.messages.clone(),
                self.client.is_generating,
                false,
                true,
            );
        }
        cx.notify();
    }

    /// Submit normally; the daemon queues a prompt while a turn is running.
    fn submit_composer(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        let text = self.composer.read(cx).value().trim().to_owned();
        if text.is_empty() {
            return;
        }
        if self.composer_pending()
            || self.selected_model.is_none()
            || self
                .client
                .active_session_id
                .as_ref()
                .is_some_and(|id| self.uncertain_prompts.contains(id))
        {
            return;
        }
        let Some(active) = &mut self.active else {
            return;
        };
        // Commands queued while the socket is down are drained without
        // delivery, so refuse unless a live socket can take it.
        let connected = self
            .daemon
            .as_ref()
            .is_some_and(|daemon| daemon.is_connected());
        if !connected {
            self.client.session_status = Some("Not connected — message was not sent".to_string());
            cx.notify();
            return;
        }
        let session_id = active.id.clone();
        if self.sending {
            return;
        }
        let command = SessionCommand::SubmitPrompt {
            session_id,
            work_dir: active.work_dir.clone(),
            text,
            images: Vec::new(),
            effort: self.effort,
            acp_config: Vec::new(),
            model: self.selected_model.clone(),
        };
        if let Some(daemon) = &self.daemon {
            // Mid-turn submissions queue as follow-ups; the unbound
            // `queued-user-{session}` echo is bound to its entry id by
            // `SessionEvent::FollowUpQueued`, which turns on the row's
            // steer/edit/remove controls.
            let echo_id = if self.client.is_generating {
                format!("queued-user-{}", active.id)
            } else {
                format!("pending-user-{}", active.id)
            };
            let draft = match &command {
                SessionCommand::SubmitPrompt { text, .. } => text.clone(),
                _ => unreachable!(),
            };
            self.client.messages_mut().push(ChatMessageInfo {
                id: echo_id,
                role: MessageRole::User,
                content: draft,
                tool_activities: Vec::new(),
                streaming: false,
                reasoning_content: None,
                reasoning_expanded: false,
            });
            active.transcript.sync(
                self.client.messages.clone(),
                self.client.is_generating,
                false,
                false,
            );
            active.transcript.list.scroll_to_end();
            daemon.request(command);
            self.sending = true;
            if self.client.is_generating {
                self.client.session_status = Some("Message queued…".to_string());
            }
        }
        cx.notify();
    }

    /// Interrupt the live turn with the composer text instead of queueing
    /// behind it.
    fn steer_composer(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let text = self.composer.read(cx).value().trim().to_owned();
        if text.is_empty() || !self.client.is_generating {
            return;
        }
        let Some(active) = &mut self.active else {
            return;
        };
        let connected = self
            .daemon
            .as_ref()
            .is_some_and(|daemon| daemon.is_connected());
        if !connected {
            self.client.session_status = Some("Not connected — message was not sent".to_string());
            cx.notify();
            return;
        }
        let session_id = active.id.clone();
        if let Some(daemon) = &self.daemon {
            daemon.send(SessionCommand::SteerMessage {
                session_id: session_id.clone(),
                text: text.clone(),
                images: Vec::new(),
            });
            let echo_id = format!(
                "steered-user-{session_id}-{}",
                self.client.messages.len()
            );
            self.client.messages_mut().push(ChatMessageInfo {
                id: echo_id,
                role: MessageRole::User,
                content: text,
                tool_activities: Vec::new(),
                streaming: false,
                reasoning_content: None,
                reasoning_expanded: false,
            });
            active.transcript.sync(
                self.client.messages.clone(),
                self.client.is_generating,
                false,
                false,
            );
            active.transcript.list.scroll_to_end();
            self.client.session_status = Some("Steering current turn…".to_string());
            self.composer
                .update(cx, |input, cx| input.set_value("", window, cx));
        }
        cx.notify();
    }

    /// Re-route a still-pending queued follow-up into the live turn.
    fn steer_queued(&mut self, entry_id: &str, cx: &mut Context<Self>) {
        let Some(session_id) = self.client.active_session_id.clone() else {
            return;
        };
        if let Some(daemon) = &self.daemon {
            daemon.send(SessionCommand::SteerQueuedMessage {
                session_id: session_id.clone(),
                entry_id: entry_id.to_string(),
            });
        }
        let queued_id = format!("queued-user-{session_id}-{entry_id}");
        if let Some(message) = self
            .client
            .messages_mut()
            .iter_mut()
            .find(|m| m.id == queued_id)
        {
            message.id = format!("steered-user-{session_id}-{entry_id}");
        }
        self.client.session_status = Some("Steering current turn…".to_string());
        cx.notify();
    }

    /// Drop a still-pending queued follow-up. `restore` puts the echo's
    /// staged text back into the composer immediately — the entry leaves
    /// the queue once the reply or the journaled
    /// `QueuedEntryCancelled` confirms it, so the row stays until then.
    fn cancel_queued_message(
        &mut self,
        entry_id: &str,
        restore: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(session_id) = self.client.active_session_id.clone() else {
            return;
        };
        let echo_id = format!("queued-user-{session_id}-{entry_id}");
        if restore {
            let staged = self
                .client
                .messages
                .iter()
                .find(|m| m.id == echo_id)
                .map(|m| m.content.clone())
                .unwrap_or_default();
            if !staged.is_empty() {
                self.composer
                    .update(cx, |input, cx| input.set_value(staged, window, cx));
            }
        }
        self.pending_queued_cancel = Some((session_id.clone(), entry_id.to_string()));
        if let Some(daemon) = &self.daemon {
            daemon.request(SessionCommand::CancelQueuedMessage {
                session_id,
                entry_id: entry_id.to_string(),
                work_dir: self.client.active_work_dir.clone(),
            });
        }
        self.client.session_status = Some("Removing queued message…".to_string());
        cx.notify();
    }

    /// Open the session's linked GitHub issue: jump to the Issues tab in
    /// the session's project scope and load the detail overlay.
    fn open_linked_issue(&mut self, number: u64, cx: &mut Context<Self>) {
        let Some(active) = &self.active else {
            return;
        };
        let Some(work_dir) = self
            .client
            .projects
            .iter()
            .find(|project| project.sessions.iter().any(|s| s.id == active.id))
            .map(|project| project.work_dir.clone())
        else {
            return;
        };
        self.select_project(work_dir, cx);
        self.select_tab(Tab::Issues, cx);
        self.issue_detail_pending = Some(number);
        self.send_github(GitHubOperation::InspectIssue { number });
        cx.notify();
    }

    fn answer_permission(&mut self, decision: PermissionDecision, cx: &mut Context<Self>) {
        let Some(active) = &mut self.active else {
            return;
        };
        let Some(request) = self.client.pending_permissions.get(&active.id).cloned() else {
            return;
        };
        // Commands queued while the socket is down are drained without
        // delivery, so keep the prompt unless a live socket can take it.
        let connected = self
            .daemon
            .as_ref()
            .is_some_and(|daemon| daemon.is_connected());
        if !connected {
            self.client.session_status = Some("Not connected — answer was not sent".to_string());
            cx.notify();
            return;
        }
        if let Some(daemon) = &self.daemon {
            daemon.request(SessionCommand::AnswerPermission {
                session_id: active.id.clone(),
                request_id: request.id,
                decision,
            });
        }
        cx.notify();
    }

    fn toggle_question_option(&mut self, key: &str, option: &str, cx: &mut Context<Self>) {
        let Some(active) = &mut self.active else {
            return;
        };
        let selected = active.answers.entry(key.to_string()).or_default();
        if let Some(position) = selected.iter().position(|picked| picked == option) {
            selected.remove(position);
        } else {
            selected.push(option.to_string());
        }
        cx.notify();
    }

    fn answer_question(&mut self, dismiss: bool, cx: &mut Context<Self>) {
        let Some(active) = &mut self.active else {
            return;
        };
        let Some(request) = self.client.pending_questions.get(&active.id).cloned() else {
            return;
        };
        let connected = self
            .daemon
            .as_ref()
            .is_some_and(|daemon| daemon.is_connected());
        if !connected {
            self.client.session_status = Some("Not connected — answer was not sent".to_string());
            cx.notify();
            return;
        }
        let answer = if dismiss {
            QuestionAnswer::dismissed(&request.id)
        } else {
            QuestionAnswer {
                request_id: request.id.clone(),
                dismissed: false,
                answers: request
                    .questions
                    .iter()
                    .map(|item| {
                        let key = question_key(&request.id, &item.id);
                        QuestionItemAnswer {
                            question_id: item.id.clone(),
                            selected: active.answers.get(&key).cloned().unwrap_or_default(),
                            custom_text: self
                                .question_inputs
                                .get(&key)
                                .map(|(input, _)| input.read(cx).value().trim().to_string())
                                .filter(|text| !text.is_empty()),
                        }
                    })
                    .collect(),
            }
        };
        // Submit stays disabled until something is answered, but never resolve
        // a totally unanswered card through any path — an empty answer is
        // indistinguishable from a real one downstream.
        if !dismiss
            && answer.answers.iter().all(|item| {
                item.selected.is_empty()
                    && item
                        .custom_text
                        .as_deref()
                        .is_none_or(|text| text.is_empty())
            })
        {
            return;
        }
        if let Some(daemon) = &self.daemon {
            daemon.request(SessionCommand::AnswerQuestion {
                session_id: active.id.clone(),
                answer,
            });
        }
        cx.notify();
    }

    fn cancel_run(&mut self, cx: &mut Context<Self>) {
        let Some(active) = &self.active else { return };
        if let Some(daemon) = &self.daemon {
            daemon.send(SessionCommand::CancelRun {
                session_id: active.id.clone(),
            });
        }
        cx.notify();
    }

    /// Archive the open session — the daemon applies its worktree/dirtiness
    /// guards and replies `SessionRemoved`, which navigates back to the
    /// session list. `delete_worktree` stays off: mobile has no worktree
    /// picker, so never drop a checkout from here.
    fn delete_active_session(&mut self, cx: &mut Context<Self>) {
        let Some(active) = &mut self.active else {
            return;
        };
        let connected = self
            .daemon
            .as_ref()
            .is_some_and(|daemon| daemon.is_connected());
        if !connected {
            self.client.session_status = Some("Not connected".to_string());
            cx.notify();
            return;
        }
        if let Some(daemon) = &self.daemon {
            daemon.send(SessionCommand::DeleteSession {
                session_id: active.id.clone(),
                session_file: active.session_file.clone(),
                delete_worktree: false,
            });
        }
        active.confirm_delete = false;
        self.client.session_status = Some("Archiving session…".to_string());
        cx.notify();
    }
}

impl MobileApp {
    /// Colored link-state dot for the sessions header.
    fn link_dot(&self, cx: &Context<Self>) -> impl IntoElement {
        let color = match self.link_state.as_str() {
            "Live" => cx.theme().success,
            "Connecting…" | "Reconnecting…" => cx.theme().warning,
            "Failed" => cx.theme().danger,
            _ => cx.theme().muted_foreground,
        };
        div()
            .flex_none()
            .size_2()
            .rounded_full()
            .bg(color)
            .mt(px(2.))
    }

    fn render_connect(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let mut brand = div().flex().items_center().gap_3();
        if let Some(logo) = threadlane_ui_theme::bundled_icon("icons/threadlane.svg") {
            brand = brand.child(logo.large().text_color(cx.theme().foreground));
        }
        let header = div()
            .flex_none()
            .flex()
            .flex_col()
            .gap_2()
            .pt_4()
            .child(brand.child(div().text_xl().font_bold().child("Threadlane")))
            .child(
                div()
                    .text_sm()
                    .text_color(cx.theme().muted_foreground)
                    .child(
                        "Run your desktop sessions from your phone. Scan the QR code \
                         in the desktop app (sidebar → Share with mobile) \
                         or enter the pairing details below.",
                    ),
            );
        div()
            .id("pairing")
            .size_full()
            .overflow_y_scroll()
            .flex()
            .flex_col()
            .bg(cx.theme().background)
            .text_color(cx.theme().foreground)
            .p_4()
            .gap_4()
            .child(header)
            .child(
                div()
                    .flex_none()
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
                                    .text_sm()
                                    .text_color(cx.theme().muted_foreground)
                                    .child("Host"),
                            )
                            .child(Input::new(&self.host).large().aria_label("Desktop host")),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap_1()
                            .child(
                                div()
                                    .text_sm()
                                    .text_color(cx.theme().muted_foreground)
                                    .child("Port"),
                            )
                            .child(Input::new(&self.port).large().aria_label("Desktop port")),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap_1()
                            .child(
                                div()
                                    .text_sm()
                                    .text_color(cx.theme().muted_foreground)
                                    .child("Token"),
                            )
                            .child(Input::new(&self.token).large().aria_label("Pairing token")),
                    ),
            )
            .when_some(self.connect_error.clone(), |this, error| {
                this.child(div().text_sm().text_color(cx.theme().danger).child(error))
            })
            .child(
                Button::new("connect")
                    .primary()
                    .h_11()
                    .label("Connect")
                    .w_full()
                    .disabled(self.link_state == "Connecting…")
                    .on_click(cx.listener(|this, _, window, cx| this.connect_now(window, cx))),
            )
            .when(self.saved_pairing, |this| {
                this.child(
                    Button::new("forget-pairing")
                        .ghost()
                        .small()
                        .h_11()
                        .label("Forget saved pairing")
                        .w_full()
                        .on_click(
                            cx.listener(|this, _, window, cx| this.forget_pairing(window, cx)),
                        ),
                )
            })
    }

    fn render_sessions(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .size_full()
            .flex()
            .flex_col()
            .bg(cx.theme().background)
            .text_color(cx.theme().foreground)
            .child(
                div()
                    .flex_none()
                    .flex()
                    .items_center()
                    .gap_2()
                    .px_3()
                    .py_3()
                    .border_b_1()
                    .border_color(cx.theme().border)
                    .child(
                        Button::new("open-sidebar")
                            .ghost()
                            .h_11()
                            .icon(icon(kit_icons::Menu.1))
                            .accessibility_label("Menu")
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.sidebar_open = true;
                                cx.notify();
                            })),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap_2()
                                    .child(div().font_bold().child("Projects & chats"))
                                    .child(self.link_dot(cx)),
                            )
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(cx.theme().muted_foreground)
                                    .child(self.link_state.clone()),
                            ),
                    ),
            )
            .child(
                div()
                    .flex()
                    .px_3()
                    .py_2()
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .flex_1()
                            .text_sm()
                            .text_color(cx.theme().muted_foreground)
                            .child("Choose a project"),
                    )
                    .child(
                        Button::new("new-session")
                            .ghost()
                            .h_11()
                            .label("New chat")
                            .disabled(
                                self.sending
                                    || self.client.sidebar_project_filter.is_none()
                                    || !self.daemon.as_ref().is_some_and(|d| d.is_connected()),
                            )
                            .on_click(cx.listener(|this, _, _, cx| this.begin_session(cx))),
                    ),
            )
            .child(
                div()
                    .px_3()
                    .pb_2()
                    .child(Input::new(&self.search).aria_label("Search chats").h_11()),
            )
            .when_some(self.connect_error.clone(), |this, error| {
                this.child(
                    div()
                        .px_3()
                        .text_sm()
                        .text_color(cx.theme().danger)
                        .child(error),
                )
            })
            .when(self.client.projects.is_empty(), |this| {
                this.child(
                    div()
                        .px_4()
                        .py_4()
                        .text_sm()
                        .text_color(cx.theme().muted_foreground)
                        .child("Attach a project on desktop to start a chat."),
                )
            })
            .child(div().id("sessions").flex_1().min_h_0().w_full().child(
                threadlane_ui_session::session_list(
                    self.sessions_list.clone(),
                    cx.processor(Self::render_session_row),
                ),
            ))
    }

    fn refresh_session_rows(&mut self) {
        self.session_rows = self
            .client
            .projects
            .iter()
            .flat_map(|project| {
                std::iter::once(MobileSessionRow::Project(project.clone())).chain(
                    project
                        .sessions
                        .iter()
                        .filter(|session| {
                            (self.client.sidebar_project_filter.as_ref() == Some(&project.work_dir)
                                || !self.client.search_query.is_empty())
                                && (self.client.search_query.is_empty()
                                    || session
                                        .title
                                        .to_lowercase()
                                        .contains(&self.client.search_query.to_lowercase()))
                        })
                        .cloned()
                        .map(MobileSessionRow::Session),
                )
            })
            .collect();
        self.sessions_list.reset(self.session_rows.len());
    }

    fn render_session_row(
        &mut self,
        ix: usize,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        match self.session_rows.get(ix) {
            Some(MobileSessionRow::Project(project)) => {
                let project = project.clone();
                let work_dir = project.work_dir.clone();
                let expanded = self.client.sidebar_project_filter.as_ref() == Some(&work_dir);
                let status = self
                    .project_git
                    .get(&work_dir)
                    .map(|git| {
                        format!(
                            "{} · {}{}",
                            git.branch.as_deref().unwrap_or("Detached"),
                            if git.has_changes { "Modified" } else { "Clean" },
                            if git.ahead > 0 || git.behind > 0 {
                                format!(" · ↑{} ↓{}", git.ahead, git.behind)
                            } else {
                                String::new()
                            }
                        )
                    })
                    .unwrap_or_else(|| "Git status unavailable".into());
                div()
                    .px_3()
                    .pt_3()
                    .child(
                        Button::new(format!("project-{}", work_dir.display()))
                            .ghost()
                            .w_full()
                            .h_auto()
                            .min_h_16()
                            .child(
                                div()
                                    .flex()
                                    .flex_col()
                                    .items_start()
                                    .w_full()
                                    .min_w_0()
                                    .gap_1()
                                    .child(div().font_bold().truncate().child(format!(
                                        "{} {}",
                                        if expanded { "▾" } else { "▸" },
                                        project.name
                                    )))
                                    .child(
                                        div()
                                            .text_xs()
                                            .text_color(cx.theme().muted_foreground)
                                            .w_full()
                                            .truncate()
                                            .child(format!(
                                                "{} {} · {}",
                                                project.sessions.len(),
                                                if project.sessions.len() == 1 {
                                                    "chat"
                                                } else {
                                                    "chats"
                                                },
                                                status
                                            )),
                                    ),
                            )
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.client.sidebar_project_filter = if expanded {
                                    None
                                } else {
                                    Some(work_dir.clone())
                                };
                                if let Some(daemon) = &this.daemon {
                                    daemon.request(SessionCommand::GitRequest {
                                        work_dir: work_dir.clone(),
                                        operation: GitOperation::Inspect { sync_remote: false },
                                    });
                                }
                                this.refresh_session_rows();
                                cx.notify();
                            })),
                    )
                    .into_any_element()
            }
            Some(MobileSessionRow::Session(session)) => {
                let info = session.clone();
                let needs_you = self.client.pending_permissions.contains_key(&session.id)
                    || self.client.pending_questions.contains_key(&session.id);
                let attention = if needs_you {
                    threadlane_protocol::daemon::SessionAttention::NeedsYou
                } else if matches!(session.health, SessionHealth::Working) {
                    threadlane_protocol::daemon::SessionAttention::Working
                } else {
                    threadlane_protocol::daemon::SessionAttention::Idle
                };
                let identity = threadlane_ui_session::session_identity(session);
                div()
                    .px_3()
                    .child(
                        threadlane_ui_session::session_card(&session.id, false, cx)
                            .aria_label(format!("{}, {}", identity.title, attention.label()))
                            .child(
                                Button::new(format!("session-{}", session.id))
                                    .ghost()
                                    .h_auto()
                                    .min_h_16()
                                    .py_3()
                                    .w_full()
                                    .child(
                                        div()
                                            .flex()
                                            .flex_col()
                                            .items_start()
                                            .gap_1()
                                            .w_full()
                                            .child(
                                                div()
                                                    .flex()
                                                    .items_center()
                                                    .gap_2()
                                                    .w_full()
                                                    .child(
                                                        div()
                                                            .flex_1()
                                                            .min_w_0()
                                                            .truncate()
                                                            .child(identity.title),
                                                    )
                                                    .children(
                                                        threadlane_ui_session::session_attention(
                                                            &session.id,
                                                            attention,
                                                            cx,
                                                        ),
                                                    ),
                                            )
                                            .child(
                                                div()
                                                    .text_xs()
                                                    .text_color(cx.theme().muted_foreground)
                                                    .truncate()
                                                    .child(format!(
                                                        "{}{}",
                                                        session
                                                            .git_branch
                                                            .as_deref()
                                                            .unwrap_or("Local"),
                                                        if session.is_worktree {
                                                            " · worktree"
                                                        } else {
                                                            ""
                                                        }
                                                    )),
                                            ),
                                    )
                                    .on_click(cx.listener(move |this, _, window, cx| {
                                        this.open_session(&info, window, cx)
                                    })),
                            ),
                    )
                    .into_any_element()
            }
            None => div().into_any_element(),
        }
    }

    fn render_session(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let uncertain_prompt = self.client.active_session_id.as_ref()
            .is_some_and(|id| self.uncertain_prompts.contains(id));
        let Some(active) = &self.active else {
            return div().size_full().bg(cx.theme().background);
        };
        let title = active.title.clone();
        let working = self.client.is_generating;
        let status = self.client.session_status.clone();
        let permission = self.client.pending_permissions.get(&active.id).cloned();
        let question = self.client.pending_questions.get(&active.id).cloned();
        let confirm_delete = active.confirm_delete;
        let composer_empty = self.composer.read(cx).value().trim().is_empty();
        let session_meta = {
            let mut parts: Vec<String> = Vec::new();
            if let Some(branch) = &active.git_branch {
                if !branch.is_empty() {
                    parts.push(branch.clone());
                }
            }
            if active.is_worktree {
                parts.push("worktree".to_string());
            }
            parts.join(" · ")
        };
        let linked_issue = active.github_issue.clone();
        let connected = self.daemon.as_ref().is_some_and(|d| d.is_connected());
        let send_disabled = composer_empty
            || self.sending
            || self.composer_pending()
            || self.selected_model.is_none()
            || uncertain_prompt
            || !connected;

        div()
            .size_full()
            .flex()
            .flex_col()
            .bg(cx.theme().background)
            .text_color(cx.theme().foreground)
            .child(
                div()
                    .flex_none()
                    .flex()
                    .items_center()
                    .gap_2()
                    .px_3()
                    .py_3()
                    .border_b_1()
                    .border_color(cx.theme().border)
                    .child(
                        Button::new("back")
                            .ghost()
                            .h_11()
                            .icon(icon(kit_icons::ArrowLeft.1))
                            .label("Back")
                            .on_click(cx.listener(|this, _, _, cx| {
                                let key = (
                                    this.client.active_work_dir.clone(),
                                    this.client.active_session_id.clone(),
                                );
                                this.client.composer_drafts.insert(
                                    key,
                                    threadlane_client::ComposerDraft {
                                        text: this.composer.read(cx).value().to_string(),
                                        images: Vec::new(),
                                    },
                                );
                                this.active = None;
                                cx.notify();
                            })),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .child(div().font_bold().truncate().child(title))
                            .when(!session_meta.is_empty(), |this| {
                                this.child(
                                    div()
                                        .text_xs()
                                        .text_color(cx.theme().muted_foreground)
                                        .truncate()
                                        .child(session_meta),
                                )
                            })
                            .when_some(status, |this, status| {
                                this.child(
                                    div()
                                        .text_xs()
                                        .text_color(cx.theme().muted_foreground)
                                        .child(status),
                                )
                            }),
                    )
                    .when_some(linked_issue, |this, issue| {
                        let number = issue.number;
                        this.child(
                            Button::new(format!("linked-issue-{number}"))
                                .ghost()
                                .small()
                                .h_10()
                                .icon(icon(kit_icons::CircleDot.1))
                                .label(format!("#{number}"))
                                .tooltip("Open linked issue")
                                .accessibility_label(format!(
                                    "Open linked issue number {number}"
                                ))
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.open_linked_issue(number, cx)
                                })),
                        )
                    })
                    .when(working, |this| {
                        this.child(
                            Button::new("cancel-run")
                                .danger()
                                .h_11()
                                .label("Stop")
                                .on_click(cx.listener(|this, _, _, cx| this.cancel_run(cx))),
                        )
                    })
                    .child(
                        Button::new("session-options")
                            .ghost()
                            .size_11()
                            .icon(icon(kit_icons::Ellipsis.1))
                            .accessibility_label("Session options")
                            .tooltip("Session options")
                            .dropdown_menu_with_anchor(Anchor::TopRight, {
                                let entity = cx.entity();
                                move |menu, _, _| {
                                    let refresh = entity.clone();
                                    let new = entity.clone();
                                    let archive = entity.clone();
                                    menu.item(PopupMenuItem::new("Refresh chat").on_click(
                                        move |_, _, cx| {
                                            refresh.update(cx, |this, _| {
                                                if let (Some(active), Some(daemon)) =
                                                    (&this.active, &this.daemon)
                                                {
                                                    daemon.send(
                                                        SessionCommand::GetSessionSnapshot {
                                                            session_id: active.id.clone(),
                                                        },
                                                    );
                                                }
                                            });
                                        },
                                    ))
                                    .item(PopupMenuItem::new("New chat in project").on_click(
                                        move |_, _, cx| {
                                            new.update(cx, |this, cx| this.begin_session(cx));
                                        },
                                    ))
                                    .item(
                                        PopupMenuItem::new("Archive chat…").on_click(
                                            move |_, _, cx| {
                                                archive.update(cx, |this, cx| {
                                                    if let Some(active) = &mut this.active {
                                                        active.confirm_delete = true;
                                                    }
                                                    cx.notify();
                                                });
                                            },
                                        ),
                                    )
                                }
                            }),
                    ),
            )
            .when(confirm_delete, |this| {
                this.child(
                    div()
                        .flex_none()
                        .flex()
                        .items_center()
                        .gap_2()
                        .px_3()
                        .py_2()
                        .border_b_1()
                        .border_color(cx.theme().border)
                        .bg(cx.theme().muted.opacity(0.3))
                        .child(div().flex_1().text_xs().child("Archive this session?"))
                        .child(
                            Button::new("confirm-delete")
                                .danger()
                                .small()
                                .label("Archive")
                                .on_click(
                                    cx.listener(|this, _, _, cx| this.delete_active_session(cx)),
                                ),
                        )
                        .child(
                            Button::new("cancel-delete")
                                .ghost()
                                .small()
                                .label("Cancel")
                                .on_click(cx.listener(|this, _, _, cx| {
                                    if let Some(active) = &mut this.active {
                                        active.confirm_delete = false;
                                    }
                                    cx.notify();
                                })),
                        ),
                )
            })
            .child(div().id("transcript").flex_1().min_h_0().w_full().child(
                threadlane_ui_session::transcript_list(
                    &self.active.as_ref().unwrap().transcript,
                    cx.processor(Self::render_transcript_row),
                ),
            ))
            .when_some(permission, |this, permission| {
                this.child(self.render_permission(&permission, cx))
            })
            .when_some(question, |this, question| {
                this.child(self.render_question(&question, window, cx))
            })
            .child(
                div().flex_none().w_full().px_3().mb_3().child(
                    threadlane_ui_session::composer_surface(cx)
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .child(threadlane_ui_session::composer_input(&self.composer)),
                        )
                        .when(uncertain_prompt, |this| this.child(
                            div().flex().flex_col().gap_1()
                                .child("Submission is unconfirmed. Check the chat before sending again; the original prompt may still run.")
                                .child(Button::new("acknowledge-uncertain-prompt").ghost().label("I checked — allow another send")
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        if let Some(active) = &this.active { this.uncertain_prompts.remove(&active.id); }
                                        cx.notify();
                                    })))
                        ))
                        .child(self.composer_options(cx))
                        .child(
                            div().flex().justify_end().gap_2()
                                .when(working, |this| {
                                    this.child(
                                        Button::new("steer")
                                            .ghost()
                                            .h_11()
                                            .icon(icon(kit_icons::Zap.1))
                                            .label("Steer")
                                            .tooltip("Interrupt the current turn")
                                            .accessibility_label(
                                                "Steer the current turn with this message",
                                            )
                                            .disabled(send_disabled)
                                            .on_click(cx.listener(|this, _, window, cx| {
                                                this.steer_composer(window, cx)
                                            })),
                                    )
                                })
                                .child(
                                    Button::new("send")
                                        .primary()
                                        .size_11()
                                        .icon(icon(kit_icons::SendHorizontal.1))
                                        .accessibility_label(if working {
                                            "Queue for next turn"
                                        } else {
                                            "Send message"
                                        })
                                        .disabled(send_disabled)
                                        .on_click(cx.listener(|this, _, window, cx| {
                                            this.submit_composer(window, cx)
                                        })),
                                ),
                        ),
                ),
            )
    }

    fn render_transcript_row(
        &mut self,
        ix: usize,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        use threadlane_ui_session::transcript::TranscriptRow;
        let Some(active) = &self.active else {
            return div().into_any_element();
        };
        let messages = active.transcript.messages.clone();
        match active.transcript.rows.get(ix).cloned() {
            Some(TranscriptRow::Message(index)) => messages
                .get(index)
                .map(|m| self.render_message(m, cx))
                .unwrap_or_else(|| div().into_any_element()),
            Some(TranscriptRow::Activities(range)) => div()
                .flex()
                .flex_col()
                .children(messages[range].iter().map(|m| self.render_message(m, cx)))
                .into_any_element(),
            Some(TranscriptRow::Working) => div()
                .px_5()
                .child(
                    Marker::new()
                        .id("session-working")
                        .role(Role::Status)
                        .loading(true)
                        .with_loading_style(MarkerLoadingStyle::Shimmer)
                        .content(MarkerContent::new().text("Working…")),
                )
                .into_any_element(),
            None => div().into_any_element(),
        }
    }

    fn render_message(&mut self, message: &ChatMessageInfo, cx: &mut Context<Self>) -> AnyElement {
        let is_user = message.role == MessageRole::User;
        let namespace =
            SharedString::from(self.client.active_session_id.clone().unwrap_or_default());
        let state = threadlane_ui_session::markdown::markdown_state(
            &mut self.markdown_states,
            namespace,
            message.id.clone(),
            &message.content,
            cx,
        );
        let reasoning = message
            .reasoning_content
            .as_ref()
            .filter(|text| !text.trim().is_empty())
            .map(|reasoning| {
                let detail = message.reasoning_expanded.then(|| {
                    if message.streaming {
                        threadlane_ui_session::reasoning_detail(cx)
                            .child(reasoning.clone())
                            .into_any_element()
                    } else {
                        let namespace = SharedString::from(
                            self.client.active_session_id.clone().unwrap_or_default(),
                        );
                        let markdown = threadlane_ui_session::markdown::markdown_state(
                            &mut self.markdown_states,
                            namespace,
                            format!("reasoning-{}", message.id),
                            reasoning,
                            cx,
                        );
                        threadlane_ui_session::reasoning_detail(cx)
                            .child(threadlane_ui_session::markdown::markdown_view(
                                &markdown,
                                |_, _| {},
                            ))
                            .into_any_element()
                    }
                });
                let owner = cx.entity().downgrade();
                let id = message.id.clone();
                threadlane_ui_session::reasoning_card(
                    message,
                    detail,
                    true,
                    move |_, cx| {
                        let _ = owner.update(cx, |this, cx| {
                            if let Some(message) =
                                this.client.messages_mut().iter_mut().find(|m| m.id == id)
                            {
                                message.reasoning_expanded = !message.reasoning_expanded;
                            }
                            if let Some(active) = &mut this.active {
                                active.transcript.list.pause_following_tail();
                                active.transcript.sync(
                                    this.client.messages.clone(),
                                    this.client.is_generating,
                                    false,
                                    true,
                                );
                            }
                            cx.notify();
                        });
                    },
                    cx,
                )
            });
        let tools = message
            .tool_activities
            .iter()
            .map(|tool| {
                let detail = tool.is_expanded.then(|| {
                    threadlane_ui_session::tool_detail(cx)
                        .child(tool.detail.clone())
                        .into_any_element()
                });
                let owner = cx.entity().downgrade();
                let id = tool.id.clone();
                threadlane_ui_session::tool_activity(
                    tool,
                    !tool.detail.trim().is_empty(),
                    detail,
                    true,
                    move |_, cx| {
                        let _ = owner.update(cx, |this, cx| {
                            if let Some(tool) = this
                                .client
                                .messages_mut()
                                .iter_mut()
                                .flat_map(|m| &mut m.tool_activities)
                                .find(|tool| tool.id == id)
                            {
                                tool.is_expanded = !tool.is_expanded;
                            }
                            if let Some(active) = &mut this.active {
                                active.transcript.list.pause_following_tail();
                                active.transcript.sync(
                                    this.client.messages.clone(),
                                    this.client.is_generating,
                                    false,
                                    true,
                                );
                            }
                            cx.notify();
                        });
                    },
                    cx,
                )
            })
            .collect::<Vec<_>>();
        let body = div()
            .text_sm()
            .line_height(relative(1.5))
            .children(reasoning)
            .when(!message.content.is_empty(), |el| {
                el.child(threadlane_ui_session::markdown::markdown_view(
                    &state,
                    |_, _| {},
                ))
            })
            .when(message.streaming && message.content.is_empty(), |this| {
                this.text_color(cx.theme().muted_foreground).child("…")
            })
            .children(tools);
        if is_user {
            let session_id = self
                .client
                .active_session_id
                .clone()
                .unwrap_or_default();
            let queued_entry = message
                .id
                .strip_prefix(&format!("queued-user-{session_id}-"))
                .map(|entry| entry.to_string());
            let is_queued = queued_entry.is_some()
                || message.id == format!("queued-user-{session_id}");
            let is_steered = message
                .id
                .starts_with(&format!("steered-user-{session_id}-"));
            let cancel_pending = queued_entry.as_ref().is_some_and(|entry| {
                self.pending_queued_cancel
                    .as_ref()
                    .is_some_and(|(sid, eid)| sid == &session_id && eid == entry)
            });
            let mut row = threadlane_ui_session::message_row(MessageRole::User)
                .child(threadlane_ui_session::user_message_bubble(cx).child(body));
            if is_queued || is_steered {
                let mut controls = div()
                    .flex()
                    .items_center()
                    .justify_end()
                    .gap_2()
                    .px_3()
                    .pb_1()
                    .child(Tag::new().small().child(if is_steered {
                        "Steered"
                    } else {
                        "Queued"
                    }));
                if let Some(entry_id) = queued_entry {
                    let steer_id = entry_id.clone();
                    let edit_id = entry_id.clone();
                    let remove_id = entry_id.clone();
                    controls = controls
                        .child(
                            Button::new(format!("queued-steer-{entry_id}"))
                                .ghost()
                                .small()
                                .h_9()
                                .icon(icon(kit_icons::Zap.1))
                                .label("Steer")
                                .tooltip("Send into the current turn")
                                .accessibility_label("Steer queued message into the current turn")
                                .disabled(cancel_pending)
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.steer_queued(&steer_id, cx)
                                })),
                        )
                        .child(
                            Button::new(format!("queued-edit-{entry_id}"))
                                .ghost()
                                .small()
                                .h_9()
                                .label("Edit")
                                .tooltip("Restore to composer")
                                .accessibility_label("Restore queued message to composer")
                                .disabled(cancel_pending)
                                .on_click(cx.listener(move |this, _, window, cx| {
                                    this.cancel_queued_message(&edit_id, true, window, cx)
                                })),
                        )
                        .child(
                            Button::new(format!("queued-remove-{entry_id}"))
                                .ghost()
                                .small()
                                .h_9()
                                .icon(icon(kit_icons::X.1))
                                .label("Remove")
                                .tooltip("Discard queued message")
                                .accessibility_label("Discard queued message")
                                .disabled(cancel_pending)
                                .on_click(cx.listener(move |this, _, window, cx| {
                                    this.cancel_queued_message(&remove_id, false, window, cx)
                                })),
                        );
                }
                row = row.child(controls);
            }
            row.into_any_element()
        } else {
            threadlane_ui_session::message_row(message.role.clone())
                .child(body)
                .into_any_element()
        }
    }

    fn render_permission(
        &self,
        request: &PermissionRequest,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let owner = cx.entity().downgrade();
        let connected = self.daemon.as_ref().is_some_and(|d| d.is_connected());
        threadlane_ui_session::permission_card(
            request,
            true,
            connected,
            None,
            move |request_id, decision, _, cx| {
                let _ = owner.update(cx, |this, cx| {
                    if this
                        .client
                        .active_session_id
                        .as_ref()
                        .and_then(|id| this.client.pending_permissions.get(id))
                        .is_some_and(|r| r.id == request_id)
                    {
                        this.answer_permission(decision, cx);
                    }
                });
            },
            cx,
        )
    }

    /// The front question request: option toggles per item, one custom-answer
    /// input per `allow_custom` item, then Submit/Dismiss.
    fn render_question(
        &mut self,
        request: &QuestionRequest,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let Some(active) = &self.active else {
            return div().into_any_element();
        };
        let answers = active.answers.clone();
        // Lazily create one custom-text input per allow_custom question so the
        // entity (and focus) survives re-renders.
        for item in request.questions.iter().filter(|item| item.allow_custom) {
            let key = question_key(&request.id, &item.id);
            if !self.question_inputs.contains_key(&key) {
                let input = cx
                    .new(|cx| InputState::new(window, cx).placeholder("Custom answer (optional)…"));
                let subscription =
                    cx.subscribe_in(&input, window, |_, _, event, _, _| match event {
                        InputEvent::Focus => gpui_mobile::show_keyboard(),
                        InputEvent::Blur => gpui_mobile::hide_keyboard(),
                        _ => {}
                    });
                self.question_inputs.insert(key, (input, subscription));
            }
        }
        // Drop state for superseded requests so a new question starts clean.
        let prefix = format!("{}\0", request.id);
        self.question_inputs
            .retain(|key, _| key.starts_with(&prefix));
        let mut items = Vec::new();
        for item in &request.questions {
            let key = question_key(&request.id, &item.id);
            let owner = cx.entity().downgrade();
            let option_key = key.clone();
            items.push(threadlane_ui_session::question_item(
                &request.id,
                item,
                answers.get(&key).map(Vec::as_slice).unwrap_or_default(),
                self.question_inputs.get(&key).map(|(input, _)| input),
                true,
                move |value, _, cx| {
                    let _ = owner.update(cx, |this, cx| {
                        this.toggle_question_option(&option_key, value, cx)
                    });
                },
                cx,
            ));
        }
        // Sending with zero selections and zero custom text resolves an empty
        // answer (indistinguishable from a real one downstream), so Submit
        // stays disabled until something is answered.
        let has_answer = request.questions.iter().any(|item| {
            let key = question_key(&request.id, &item.id);
            let selected = answers.get(&key).is_some_and(|picked| !picked.is_empty());
            let custom = self
                .question_inputs
                .get(&key)
                .is_some_and(|(input, _)| !input.read(cx).value().trim().is_empty());
            selected || custom
        });
        let remaining = self
            .client
            .queued_questions
            .get(&active.id)
            .map_or(0, Vec::len);
        threadlane_ui_session::question_surface(cx)
            .flex_none()
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(
                        icon(kit_icons::CircleQuestionMark.1)
                            .xsmall()
                            .text_color(cx.theme().accent),
                    )
                    .child(
                        div()
                            .text_sm()
                            .font_bold()
                            .child(if request.questions.len() > 1 {
                                format!("The agent has {} questions", request.questions.len())
                            } else {
                                "The agent has a question".to_string()
                            }),
                    )
                    .when(remaining > 0, |this| {
                        this.child(
                            div()
                                .text_xs()
                                .text_color(cx.theme().muted_foreground)
                                .child(format!("+{remaining} more")),
                        )
                    }),
            )
            .children(items)
            .child(
                div()
                    .flex()
                    .gap_2()
                    .child(
                        Button::new("question-submit")
                            .primary()
                            .small()
                            .label("Submit")
                            .disabled(!has_answer)
                            .on_click(
                                cx.listener(|this, _, _, cx| this.answer_question(false, cx)),
                            ),
                    )
                    .child(
                        Button::new("question-dismiss")
                            .ghost()
                            .small()
                            .label("Dismiss")
                            .on_click(cx.listener(|this, _, _, cx| this.answer_question(true, cx))),
                    ),
            )
            .into_any_element()
    }

    /// Route a `GitRequest` at the sidebar-selected project, if one is set.
    fn send_git(&self, operation: GitOperation) {
        if let (Some(work_dir), Some(daemon)) =
            (self.client.sidebar_project_filter.clone(), &self.daemon)
        {
            daemon.request(SessionCommand::GitRequest { work_dir, operation });
        }
    }

    /// Route a `GitHubRequest` at the sidebar-selected project — the
    /// daemon shells out to `gh` against that checkout's forge remote.
    fn send_github(&self, operation: GitHubOperation) {
        if let (Some(work_dir), Some(daemon)) =
            (self.client.sidebar_project_filter.clone(), &self.daemon)
        {
            daemon.request(SessionCommand::GitHubRequest { work_dir, operation });
        }
    }

    fn refresh_issues(&self) {
        self.send_github(GitHubOperation::ListIssues {
            state: self.issue_filter,
            query: None,
            limit: 30,
        });
    }

    fn refresh_prs(&self) {
        self.send_github(GitHubOperation::ListPullRequests {
            state: self.pr_filter,
            query: None,
            limit: 30,
        });
    }

    /// The automation store is daemon-global (not per project), so this
    /// needs no work_dir.
    fn send_automation(&self, command: AutomationCommand) {
        if let Some(daemon) = &self.daemon {
            daemon.request(SessionCommand::AutomationRequest { command });
        }
    }

    fn refresh_automations(&self) {
        self.send_automation(AutomationCommand::GetSnapshot);
    }

    /// Open the editor: `Some(definition)` pre-fills for editing, `None`
    /// starts a fresh draft scoped to the selected project.
    fn open_auto_form(&mut self, definition: Option<Definition>, window: &mut Window, cx: &mut Context<Self>) {
        let draft = definition.unwrap_or_else(|| Definition {
            id: threadlane_protocol::automation::new_id(),
            revision: 0,
            name: String::new(),
            prompt: String::new(),
            project: self
                .client
                .sidebar_project_filter
                .clone()
                .or_else(|| self.client.projects.first().map(|p| p.work_dir.clone()))
                .unwrap_or_default(),
            model: self.selected_model.clone().unwrap_or_default(),
            effort: self
                .effort
                .map(|effort| effort.label().to_string())
                .unwrap_or_else(|| "medium".into()),
            worktree: true,
            schedule: Schedule::Interval { minutes: 60 },
            enabled: true,
            notify_all: false,
            anchor: threadlane_protocol::automation::now(),
            next_at: None,
            failures: 0,
            paused_reason: None,
        });
        self.auto_schedule = match &draft.schedule {
            Schedule::Manual => AutoSchedule::Manual,
            Schedule::Interval { .. } => AutoSchedule::Interval,
            Schedule::Calendar { .. } => AutoSchedule::DailyUtc,
        };
        self.auto_worktree = draft.worktree;
        self.auto_enabled = draft.enabled;
        for (entity, value) in [
            (&self.auto_name, draft.name.clone()),
            (&self.auto_model, draft.model.clone()),
            (&self.auto_effort, draft.effort.clone()),
            (
                &self.auto_minutes,
                match &draft.schedule {
                    Schedule::Interval { minutes } => minutes.to_string(),
                    _ => "60".into(),
                },
            ),
        ] {
            entity.update(cx, |input, cx| input.set_value(value, window, cx));
        }
        self.auto_prompt.update(cx, |input, cx| {
            input.set_value(draft.prompt.clone(), window, cx)
        });
        self.auto_editing = Some(draft);
        cx.notify();
    }

    /// Save the editor's draft: read inputs, rebuild the Definition,
    /// validate locally, and send `Save`. The panel closes when the
    /// projection reply confirms it landed.
    fn save_auto_form(&mut self, cx: &mut Context<Self>) {
        let Some(mut draft) = self.auto_editing.clone() else {
            return;
        };
        draft.name = self.auto_name.read(cx).value().trim().to_owned();
        draft.model = self.auto_model.read(cx).value().trim().to_owned();
        draft.effort = self.auto_effort.read(cx).value().trim().to_owned();
        draft.prompt = self.auto_prompt.read(cx).value().trim().to_owned();
        draft.worktree = self.auto_worktree;
        draft.enabled = self.auto_enabled;
        draft.schedule = match self.auto_schedule {
            AutoSchedule::Manual => Schedule::Manual,
            AutoSchedule::Interval => Schedule::Interval {
                minutes: self
                    .auto_minutes
                    .read(cx)
                    .value()
                    .trim()
                    .parse()
                    .unwrap_or(0),
            },
            AutoSchedule::DailyUtc => Schedule::Calendar {
                hour: 9,
                minute: 0,
                days: (0..7).collect(),
                timezone: "UTC".into(),
            },
        };
        if draft.project.as_os_str().is_empty() {
            self.client.session_status = Some("Choose a project first".into());
            cx.notify();
            return;
        }
        if let Err(error) = draft.validate() {
            self.client.session_status = Some(error);
            cx.notify();
            return;
        }
        self.send_automation(AutomationCommand::Save {
            definition: draft,
        });
        cx.notify();
    }

    /// The sidebar's project pick is the scope for every panel — chats and
    /// Git both read `sidebar_project_filter`, so switching re-scopes the
    /// whole surface at once.
    fn select_project(&mut self, work_dir: std::path::PathBuf, cx: &mut Context<Self>) {
        self.client.sidebar_project_filter = Some(work_dir.clone());
        self.sidebar_open = false;
        self.git_diff = None;
        self.issues = None;
        self.issue_detail = None;
        self.prs = None;
        self.pr_detail = None;
        self.send_git(GitOperation::Inspect { sync_remote: false });
        if matches!(self.tab, Tab::Issues) {
            self.refresh_issues();
        } else if matches!(self.tab, Tab::Pulls) {
            self.refresh_prs();
        }
        self.refresh_session_rows();
        cx.notify();
    }

    fn select_tab(&mut self, tab: Tab, cx: &mut Context<Self>) {
        if self.tab == tab {
            return;
        }
        self.tab = tab;
        self.sidebar_open = false;
        match tab {
            Tab::Git => {
                self.git_diff = None;
                self.send_git(GitOperation::Inspect { sync_remote: false });
            }
            Tab::Issues => {
                if self.issues.is_none() {
                    self.refresh_issues();
                }
            }
            Tab::Pulls => {
                if self.prs.is_none() {
                    self.refresh_prs();
                }
            }
            Tab::Automations => {
                self.refresh_automations();
            }
            Tab::Chats => {}
        }
        cx.notify();
    }

    /// File rows open a full-screen diff rather than a popover — a phone
    /// has no hover, and a code view wants the whole viewport.
    fn open_file_diff(
        &mut self,
        title: String,
        operation: GitOperation,
        cx: &mut Context<Self>,
    ) {
        self.git_diff_pending = Some(title);
        self.send_git(operation);
        cx.notify();
    }

    /// Expand or collapse a commit/stash object on the Git tab, fetching
    /// its file list the first time it opens.
    fn toggle_git_object(
        &mut self,
        key: String,
        operation: GitOperation,
        cx: &mut Context<Self>,
    ) {
        if self.git_expanded.contains(&key) {
            self.git_expanded.remove(&key);
        } else {
            self.git_expanded.insert(key.clone());
            if !self.git_object_files.contains_key(&key) {
                self.send_git(operation);
            }
        }
        cx.notify();
    }
}

impl MobileApp {
    /// The shell: tabbed content over a bottom tab bar, with the sidebar
    /// and the diff overlay stacked above both.
    fn render_main(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let content = match self.tab {
            Tab::Chats => {
                if self.active.is_some() {
                    self.render_session(window, cx).into_any_element()
                } else {
                    self.render_sessions(cx).into_any_element()
                }
            }
            Tab::Git => self.render_git_panel(cx).into_any_element(),
            Tab::Issues => self.render_issues_panel(cx).into_any_element(),
            Tab::Pulls => self.render_pulls_panel(cx).into_any_element(),
            Tab::Automations => self.render_automations_panel(cx).into_any_element(),
        };
        div()
            .id("mobile-main")
            .size_full()
            .relative()
            .bg(cx.theme().background)
            .text_color(cx.theme().foreground)
            .child(
                div()
                    .size_full()
                    .flex()
                    .flex_col()
                    .child(div().flex_1().min_h_0().w_full().child(content))
                    .child(self.render_tab_bar(cx)),
            )
            .when(self.sidebar_open, |this| {
                this.child(self.render_sidebar(window, cx))
            })
            .when_some(self.issue_detail.clone(), |this, detail| {
                this.child(self.render_issue_detail(&detail, cx))
            })
            .when_some(self.pr_detail.clone(), |this, pr| {
                this.child(self.render_pr_detail(&pr, cx))
            })
            .when(self.new_issue_open, |this| {
                this.child(self.render_new_issue(cx))
            })
            .when(self.auto_editing.is_some(), |this| {
                this.child(self.render_auto_form(cx))
            })
            .when_some(self.git_diff.clone(), |this, (title, text)| {
                this.child(self.render_git_diff(&title, &text, cx))
            })
    }

    fn render_tab_bar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let tabs = [
            (Tab::Chats, "Chats", kit_icons::MessagesSquare.1),
            (Tab::Git, "Git", kit_icons::GitBranch.1),
            (Tab::Issues, "Issues", kit_icons::CircleDot.1),
            (Tab::Pulls, "PRs", kit_icons::GitPullRequest.1),
            (Tab::Automations, "Automations", kit_icons::Zap.1),
        ];
        div()
            .flex_none()
            .w_full()
            .flex()
            .items_stretch()
            .border_t_1()
            .border_color(cx.theme().border)
            .bg(cx.theme().background)
            .children(tabs.into_iter().map(|(tab, label, bytes)| {
                let color = if self.tab == tab {
                    cx.theme().primary
                } else {
                    cx.theme().muted_foreground
                };
                Button::new(format!("tab-{label}"))
                    .ghost()
                    .flex_1()
                    .h_12()
                    .accessibility_label(format!("{label} tab"))
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .items_center()
                            .justify_center()
                            .gap_1()
                            .child(icon(bytes).small().text_color(color))
                            .child(div().text_xs().text_color(color).child(label)),
                    )
                    .on_click(cx.listener(move |this, _, _, cx| this.select_tab(tab, cx)))
            }))
    }

    /// Slide-over drawer: the phone has no room for a persistent sidebar,
    /// so project picking and connection controls live behind the scrim.
    fn render_sidebar(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let width = window.viewport_size().width * 0.82;
        let saved_pairing = self.saved_pairing;
        let connected = self.daemon.as_ref().is_some_and(|d| d.is_connected());
        div()
            .id("mobile-sidebar")
            .absolute()
            .inset_0()
            .child(
                div()
                    .id("sidebar-scrim")
                    .absolute()
                    .inset_0()
                    .bg(threadlane_ui_theme::overlay_scrim())
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.sidebar_open = false;
                        cx.notify();
                    })),
            )
            .child(
                div()
                    .id("sidebar-panel")
                    .absolute()
                    .top_0()
                    .left_0()
                    .bottom_0()
                    .w(width)
                    .flex()
                    .flex_col()
                    .bg(cx.theme().sidebar)
                    .border_r_1()
                    .border_color(cx.theme().sidebar_border)
                    .child(
                        div()
                            .flex_none()
                            .flex()
                            .items_center()
                            .gap_2()
                            .px_4()
                            .py_3()
                            .border_b_1()
                            .border_color(cx.theme().sidebar_border)
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .flex()
                                    .flex_col()
                                    .child(
                                        div()
                                            .flex()
                                            .items_center()
                                            .gap_2()
                                            .child(div().font_bold().child("Threadlane"))
                                            .child(self.link_dot(cx)),
                                    )
                                    .child(
                                        div()
                                            .text_xs()
                                            .text_color(cx.theme().muted_foreground)
                                            .child(self.link_state.clone()),
                                    ),
                            )
                            .child(
                                Button::new("close-sidebar")
                                    .ghost()
                                    .icon(icon(kit_icons::X.1))
                                    .accessibility_label("Close menu")
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.sidebar_open = false;
                                        cx.notify();
                                    })),
                            ),
                    )
                    .child(
                        div().flex_none().px_4().pt_3().pb_1().child(
                            div()
                                .text_xs()
                                .text_color(cx.theme().muted_foreground)
                                .child("Projects"),
                        ),
                    )
                    .child(
                        div()
                            .id("sidebar-projects")
                            .flex_1()
                            .min_h_0()
                            .overflow_y_scroll()
                            .flex()
                            .flex_col()
                            .children(self.client.projects.iter().map(|project| {
                                self.render_sidebar_project(project, cx)
                            })),
                    )
                    .child(
                        div()
                            .flex_none()
                            .border_t_1()
                            .border_color(cx.theme().sidebar_border)
                            .p_3()
                            .flex()
                            .flex_col()
                            .gap_2()
                            .child(
                                Button::new("sidebar-disconnect")
                                    .ghost()
                                    .w_full()
                                    .h_11()
                                    .icon(icon(kit_icons::Unplug.1))
                                    .label("Disconnect")
                                    .disabled(!connected)
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.sidebar_open = false;
                                        this.disconnect(cx);
                                    })),
                            )
                            .when(saved_pairing, |this| {
                                this.child(
                                    Button::new("sidebar-forget")
                                        .ghost()
                                        .w_full()
                                        .h_11()
                                        .icon(icon(kit_icons::Trash.1))
                                        .label("Forget saved pairing")
                                        .on_click(cx.listener(|this, _, window, cx| {
                                            this.sidebar_open = false;
                                            this.forget_pairing(window, cx);
                                        })),
                                )
                            }),
                    ),
            )
    }

    fn render_sidebar_project(
        &self,
        project: &threadlane_protocol::daemon::ProjectInfo,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let work_dir = project.work_dir.clone();
        let selected = self.client.sidebar_project_filter.as_ref() == Some(&work_dir);
        let accent = if selected {
            cx.theme().sidebar_accent
        } else {
            cx.theme().muted_foreground
        };
        let status = self
            .project_git
            .get(&work_dir)
            .map(|git| {
                format!(
                    "{} · {}{}",
                    git.branch.as_deref().unwrap_or("Detached"),
                    if git.has_changes { "Modified" } else { "Clean" },
                    if git.ahead > 0 || git.behind > 0 {
                        format!(" · ↑{} ↓{}", git.ahead, git.behind)
                    } else {
                        String::new()
                    }
                )
            })
            .unwrap_or_else(|| "Git status unavailable".into());
        div()
            .px_2()
            .child(
                Button::new(format!("sidebar-project-{}", work_dir.display()))
                    .ghost()
                    .w_full()
                    .h_auto()
                    .min_h_12()
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .w_full()
                            .min_w_0()
                            .child(icon(kit_icons::Folder.1).xsmall().text_color(accent))
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .flex()
                                    .flex_col()
                                    .items_start()
                                    .gap_1()
                                    .child(
                                        div().font_bold().truncate().child(project.name.clone()),
                                    )
                                    .child(
                                        div()
                                            .text_xs()
                                            .text_color(cx.theme().muted_foreground)
                                            .truncate()
                                            .child(format!(
                                                "{} {} · {}",
                                                project.sessions.len(),
                                                if project.sessions.len() == 1 {
                                                    "chat"
                                                } else {
                                                    "chats"
                                                },
                                                status
                                            )),
                                    ),
                            )
                            .when(selected, |this| {
                                this.child(icon(kit_icons::Check.1).xsmall().text_color(accent))
                            }),
                    )
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.select_project(work_dir.clone(), cx)
                    })),
            )
            .into_any_element()
    }
}

impl MobileApp {
    /// The Git tab: the desktop's right-panel git surface stacked into one
    /// scrolling column — sync, changes, commit, branches, stashes,
    /// commits — scoped to the sidebar-selected project.
    fn render_git_panel(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let work_dir = self.client.sidebar_project_filter.clone();
        let project_name = work_dir
            .as_ref()
            .and_then(|dir| self.client.projects.iter().find(|p| &p.work_dir == dir))
            .map(|project| project.name.clone());
        let status = work_dir
            .as_ref()
            .and_then(|dir| self.project_git.get(dir).cloned());
        let status_line = self.client.session_status.clone();
        div()
            .size_full()
            .flex()
            .flex_col()
            .bg(cx.theme().background)
            .text_color(cx.theme().foreground)
            .child(
                div()
                    .flex_none()
                    .flex()
                    .items_center()
                    .gap_2()
                    .px_3()
                    .py_3()
                    .border_b_1()
                    .border_color(cx.theme().border)
                    .child(
                        Button::new("git-open-sidebar")
                            .ghost()
                            .h_11()
                            .icon(icon(kit_icons::Menu.1))
                            .accessibility_label("Menu")
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.sidebar_open = true;
                                cx.notify();
                            })),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .flex_col()
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap_2()
                                    .child(
                                        div().font_bold().truncate().child(
                                            project_name
                                                .map(|name| format!("Git — {name}"))
                                                .unwrap_or_else(|| "Git".into()),
                                        ),
                                    )
                                    .child(self.link_dot(cx)),
                            )
                            .when_some(status_line, |this, line| {
                                this.child(
                                    div()
                                        .text_xs()
                                        .text_color(cx.theme().muted_foreground)
                                        .child(line),
                                )
                            }),
                    )
                    .child(
                        Button::new("git-refresh")
                            .ghost()
                            .h_11()
                            .icon(icon(kit_icons::RefreshCw.1))
                            .accessibility_label("Fetch and refresh")
                            .disabled(work_dir.is_none())
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.send_git(GitOperation::Inspect { sync_remote: true });
                                cx.notify();
                            })),
                    ),
            )
            .when(work_dir.is_none(), |this| {
                this.child(
                    div()
                        .flex_1()
                        .flex()
                        .flex_col()
                        .items_center()
                        .justify_center()
                        .gap_3()
                        .px_6()
                        .child(
                            icon(kit_icons::GitBranch.1)
                                .large()
                                .text_color(cx.theme().muted_foreground),
                        )
                        .child(
                            div()
                                .text_sm()
                                .text_color(cx.theme().muted_foreground)
                                .child("Choose a project to see its repository"),
                        )
                        .child(
                            Button::new("git-choose-project")
                                .primary()
                                .h_11()
                                .label("Choose project")
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.sidebar_open = true;
                                    cx.notify();
                                })),
                        ),
                )
            })
            .when_some(work_dir, |this, _work_dir| match status {
                Some(status) => this.child(self.render_git_status(&status, cx)),
                None => this.child(
                    div()
                        .flex_1()
                        .flex()
                        .items_center()
                        .justify_center()
                        .child(
                            div()
                                .text_sm()
                                .text_color(cx.theme().muted_foreground)
                                .child("Loading Git status…"),
                        ),
                ),
            })
    }

    fn render_git_status(&mut self, status: &GitStatus, cx: &mut Context<Self>) -> impl IntoElement {
        let staged: Vec<&GitFile> = status.files.iter().filter(|file| file.staged).collect();
        let unstaged: Vec<&GitFile> = status
            .files
            .iter()
            .filter(|file| file.unstaged)
            .collect();
        let staged_paths: Vec<String> = staged.iter().map(|file| file.path.clone()).collect();
        let push_paths = staged_paths.clone();
        let message = self.git_message.read(cx).value().trim().to_owned();
        let commit_ready = !message.is_empty() && !staged_paths.is_empty();
        let new_branch = self.git_branch.read(cx).value().trim().to_owned();
        // Checkout targets: local branches only, current first then the
        // default branch, name-ordered after that so the menu is stable.
        // Owned because the dropdown builder closure must be 'static.
        let mut branches: Vec<GitBranchInfo> = status
            .branch_details
            .iter()
            .filter(|branch| !branch.is_remote)
            .cloned()
            .collect();
        branches.sort_by(|a, b| {
            b.is_current
                .cmp(&a.is_current)
                .then(b.is_default.cmp(&a.is_default))
                .then(a.name.cmp(&b.name))
        });
        let entity = cx.entity();
        div()
            .id("git-panel")
            .flex_1()
            .min_h_0()
            .w_full()
            .overflow_y_scroll()
            .child(
                div()
                    .px_4()
                    .py_3()
                    .flex()
                    .flex_col()
                    .gap_4()
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap_2()
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap_2()
                                    .child(
                                        icon(kit_icons::GitBranch.1)
                                            .xsmall()
                                            .text_color(cx.theme().muted_foreground),
                                    )
                                    .child(
                                        div().flex_1().min_w_0().truncate().font_bold().child(
                                            status
                                                .branch
                                                .clone()
                                                .unwrap_or_else(|| "Detached HEAD".into()),
                                        ),
                                    )
                                    .when(status.ahead > 0 || status.behind > 0, |this| {
                                        this.child(
                                            div()
                                                .text_xs()
                                                .text_color(cx.theme().muted_foreground)
                                                .child(format!(
                                                    "↑{} ↓{}",
                                                    status.ahead, status.behind
                                                )),
                                        )
                                    }),
                            )
                            .child(
                                div()
                                    .flex()
                                    .gap_2()
                                    .child(
                                        Button::new("git-fetch")
                                            .ghost()
                                            .h_11()
                                            .flex_1()
                                            .icon(icon(kit_icons::RefreshCw.1))
                                            .label("Fetch")
                                            .on_click(cx.listener(|this, _, _, cx| {
                                                this.send_git(GitOperation::Fetch);
                                                cx.notify();
                                            })),
                                    )
                                    .child(
                                        Button::new("git-pull")
                                            .ghost()
                                            .h_11()
                                            .flex_1()
                                            .icon(icon(kit_icons::Download.1))
                                            .label(if status.behind > 0 {
                                                format!("Pull ({})", status.behind)
                                            } else {
                                                "Pull".into()
                                            })
                                            .disabled(!status.has_upstream)
                                            .on_click(cx.listener(|this, _, _, cx| {
                                                this.send_git(GitOperation::Pull);
                                                cx.notify();
                                            })),
                                    )
                                    .child(
                                        Button::new("git-push")
                                            .ghost()
                                            .h_11()
                                            .flex_1()
                                            .icon(icon(kit_icons::Upload.1))
                                            .label(if status.ahead > 0 {
                                                format!("Push ({})", status.ahead)
                                            } else {
                                                "Push".into()
                                            })
                                            .on_click(cx.listener(|this, _, _, cx| {
                                                this.send_git(GitOperation::Push);
                                                cx.notify();
                                            })),
                                    ),
                            ),
                    )
                    .when_some(status.pr.clone(), |this, pr| {
                        this.child(self.render_git_pr(&pr, cx))
                    })
                    .when(
                        status.pr.is_none() && status.pr_ready && status.has_upstream,
                        |this| {
                            this.child(
                                Button::new("git-create-pr")
                                    .outline()
                                    .h_11()
                                    .icon(icon(kit_icons::GitPullRequest.1))
                                    .label("Create pull request")
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.send_git(GitOperation::CreatePullRequest);
                                        cx.notify();
                                    })),
                            )
                        },
                    )
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap_1()
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap_2()
                                    .child(
                                        div()
                                            .flex_1()
                                            .text_xs()
                                            .text_color(cx.theme().muted_foreground)
                                            .child(if staged.is_empty() && unstaged.is_empty() {
                                                "Working tree clean".to_string()
                                            } else {
                                                format!("Changes ({})", staged.len() + unstaged.len())
                                            }),
                                    )
                                    .when(!unstaged.is_empty(), |this| {
                                        this.child(
                                            Button::new("git-stage-all")
                                                .ghost()
                                                .small()
                                                .label("Stage all")
                                                .on_click(cx.listener(|this, _, _, cx| {
                                                    this.send_git(GitOperation::StageAll);
                                                    cx.notify();
                                                })),
                                        )
                                    })
                                    .when(!staged.is_empty(), |this| {
                                        this.child(
                                            Button::new("git-unstage-all")
                                                .ghost()
                                                .small()
                                                .label("Unstage all")
                                                .on_click(cx.listener(|this, _, _, cx| {
                                                    this.send_git(GitOperation::UnstageAll);
                                                    cx.notify();
                                                })),
                                        )
                                    }),
                            )
                            .when(!staged.is_empty(), |this| {
                                this.child(
                                    div()
                                        .text_xs()
                                        .text_color(cx.theme().muted_foreground)
                                        .pt_1()
                                        .child("Staged"),
                                )
                            })
                            .children(staged.iter().map(|file| {
                                self.render_git_file(
                                    format!("git-file-staged-{}", file.path),
                                    file,
                                    GitOperation::DiffFile {
                                        path: file.path.clone(),
                                        options: DiffOptions::default(),
                                    },
                                    Some(GitOperation::Unstage {
                                        paths: vec![file.path.clone()],
                                    }),
                                    cx,
                                )
                            }))
                            .when(!unstaged.is_empty(), |this| {
                                this.child(
                                    div()
                                        .text_xs()
                                        .text_color(cx.theme().muted_foreground)
                                        .pt_1()
                                        .child("Unstaged"),
                                )
                            })
                            .children(unstaged.iter().map(|file| {
                                self.render_git_file(
                                    format!("git-file-unstaged-{}", file.path),
                                    file,
                                    GitOperation::DiffFile {
                                        path: file.path.clone(),
                                        options: DiffOptions::default(),
                                    },
                                    Some(GitOperation::Stage {
                                        paths: vec![file.path.clone()],
                                    }),
                                    cx,
                                )
                            })),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap_2()
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(cx.theme().muted_foreground)
                                    .child("Commit"),
                            )
                            .child(
                                Textarea::new(&self.git_message)
                                    .aria_label("Commit message")
                                    .h_11(),
                            )
                            .child(
                                div()
                                    .flex()
                                    .gap_2()
                                    .child(
                                        Button::new("git-commit")
                                            .primary()
                                            .h_11()
                                            .flex_1()
                                            .label("Commit")
                                            .disabled(!commit_ready)
                                            .on_click(cx.listener(move |this, _, _, cx| {
                                                let message = this
                                                    .git_message
                                                    .read(cx)
                                                    .value()
                                                    .trim()
                                                    .to_owned();
                                                if message.is_empty() {
                                                    return;
                                                }
                                                this.send_git(GitOperation::Commit {
                                                    message,
                                                    selected_paths: staged_paths.clone(),
                                                    push: false,
                                                });
                                                cx.notify();
                                            })),
                                    )
                                    .child(
                                        Button::new("git-commit-push")
                                            .ghost()
                                            .h_11()
                                            .flex_1()
                                            .label("Commit & push")
                                            .disabled(!commit_ready)
                                            .on_click(cx.listener(move |this, _, _, cx| {
                                                let message = this
                                                    .git_message
                                                    .read(cx)
                                                    .value()
                                                    .trim()
                                                    .to_owned();
                                                if message.is_empty() {
                                                    return;
                                                }
                                                this.send_git(GitOperation::Commit {
                                                    message,
                                                    selected_paths: push_paths.clone(),
                                                    push: true,
                                                });
                                                cx.notify();
                                            })),
                                    ),
                            ),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap_2()
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(cx.theme().muted_foreground)
                                    .child("Branches"),
                            )
                            .child(
                                Button::new("git-branch-menu")
                                    .outline()
                                    .h_11()
                                    .w_full()
                                    .label(format!(
                                        "{} ▾",
                                        status.branch.as_deref().unwrap_or("Detached")
                                    ))
                                    .accessibility_label("Switch branch")
                                    .dropdown_caret(true)
                                    .dropdown_menu_with_anchor(Anchor::BottomLeft, {
                                        move |menu, _, _| {
                                            branches.iter().fold(menu.scrollable(true), |menu, branch| {
                                                let entity = entity.clone();
                                                let name = branch.name.clone();
                                                menu.item(
                                                    PopupMenuItem::new(branch.name.clone())
                                                        .checked(branch.is_current)
                                                        .on_click(move |_, _, cx| {
                                                            let _ = entity.update(cx, |this, cx| {
                                                                this.send_git(GitOperation::Checkout {
                                                                    branch: name.clone(),
                                                                    mode: CheckoutMode::Clean,
                                                                });
                                                                cx.notify();
                                                            });
                                                        }),
                                                )
                                            })
                                        }
                                    }),
                            )
                            .child(
                                div()
                                    .flex()
                                    .gap_2()
                                    .child(
                                        div().flex_1().min_w_0().child(
                                            Input::new(&self.git_branch)
                                                .aria_label("New branch name")
                                                .h_11(),
                                        ),
                                    )
                                    .child(
                                        Button::new("git-create-branch")
                                            .outline()
                                            .h_11()
                                            .icon(icon(kit_icons::Plus.1))
                                            .label("Create")
                                            .disabled(new_branch.is_empty())
                                            .on_click(cx.listener(|this, _, window, cx| {
                                                let name = this
                                                    .git_branch
                                                    .read(cx)
                                                    .value()
                                                    .trim()
                                                    .to_owned();
                                                if name.is_empty() {
                                                    return;
                                                }
                                                this.send_git(GitOperation::CreateBranch { name });
                                                this.git_branch.update(cx, |input, cx| {
                                                    input.set_value("", window, cx)
                                                });
                                                cx.notify();
                                            })),
                                    ),
                            ),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap_1()
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap_2()
                                    .child(
                                        div()
                                            .flex_1()
                                            .text_xs()
                                            .text_color(cx.theme().muted_foreground)
                                            .child(if status.stashes.is_empty() {
                                                "Stash".to_string()
                                            } else {
                                                format!("Stashes ({})", status.stashes.len())
                                            }),
                                    )
                                    .child(
                                        Button::new("git-stash-push")
                                            .ghost()
                                            .small()
                                            .icon(icon(kit_icons::Inbox.1))
                                            .label("Stash changes")
                                            .disabled(!status.has_changes)
                                            .on_click(cx.listener(|this, _, _, cx| {
                                                this.send_git(GitOperation::StashPush {
                                                    message: None,
                                                    include_untracked: true,
                                                });
                                                cx.notify();
                                            })),
                                    ),
                            )
                            .children(
                                status
                                    .stashes
                                    .iter()
                                    .map(|stash| self.render_stash_row(stash, cx)),
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
                                    .text_color(cx.theme().muted_foreground)
                                    .child("Recent commits"),
                            )
                            .when(status.recent_commits.is_empty(), |this| {
                                this.child(
                                    div()
                                        .text_sm()
                                        .text_color(cx.theme().muted_foreground)
                                        .child("No commits yet"),
                                )
                            })
                            .children(
                                status
                                    .recent_commits
                                    .iter()
                                    .map(|commit| self.render_commit_row(commit, cx)),
                            ),
                    ),
            )
    }

    fn render_git_pr(&self, pr: &GitHubPrInfo, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .flex()
            .items_center()
            .gap_2()
            .child(
                icon(kit_icons::GitPullRequest.1)
                    .xsmall()
                    .text_color(cx.theme().accent),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .child(
                        div()
                            .text_sm()
                            .font_bold()
                            .truncate()
                            .child(format!("#{} — {}", pr.number, pr.title)),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(cx.theme().muted_foreground)
                            .child(format!(
                                "{}{} · {} checks{}",
                                if pr.is_draft { "Draft " } else { "" },
                                pr.state,
                                pr.passing_checks,
                                if pr.total_checks > 0 {
                                    format!("/{} passing", pr.total_checks)
                                } else {
                                    String::new()
                                }
                            )),
                    ),
            )
    }

    /// One file row: tap opens the full-screen diff; the trailing button
    /// carries the stage/unstage action when the row is a working-tree
    /// file (commit and stash rows pass `None`).
    fn render_git_file(
        &self,
        id: String,
        file: &GitFile,
        diff: GitOperation,
        action: Option<GitOperation>,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let title = file.path.clone();
        let action_key = file.path.clone();
        let (action_label, action_verb) = match &action {
            Some(GitOperation::Stage { .. }) => (Some("stage"), Some("Stage")),
            Some(GitOperation::Unstage { .. }) => (Some("unstage"), Some("Unstage")),
            _ => (None, None),
        };
        div()
            .flex()
            .items_center()
            .gap_1()
            .child(
                Button::new(id)
                    .ghost()
                    .flex_1()
                    .h_auto()
                    .min_h_10()
                    .child(self.render_git_file_label(file, cx))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.open_file_diff(title.clone(), diff.clone(), cx);
                    })),
            )
            .when_some(action, |this, action| {
                this.child(
                    Button::new(format!("git-action-{}-{action_key}", action_label.unwrap_or("noop")))
                        .ghost()
                        .small()
                        .h_9()
                        .label(action_verb.unwrap_or_default())
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.send_git(action.clone());
                            cx.notify();
                        })),
                )
            })
    }

    fn render_git_file_label(&self, file: &GitFile, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .flex()
            .items_center()
            .gap_2()
            .w_full()
            .min_w_0()
            .child(
                div()
                    .w_4()
                    .flex_none()
                    .text_xs()
                    .font_family(cx.theme().mono_font_family.clone())
                    .text_color(git_status_color(file.status_char(), cx))
                    .child(file.status_char().to_string()),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .items_start()
                    .child(div().text_sm().truncate().child(file.path.clone()))
                    .when(file.additions + file.deletions > 0, |this| {
                        this.child(
                            div()
                                .flex()
                                .gap_1()
                                .text_xs()
                                .child(
                                    div()
                                        .text_color(cx.theme().success)
                                        .child(format!("+{}", file.additions)),
                                )
                                .child(
                                    div()
                                        .text_color(cx.theme().danger)
                                        .child(format!("-{}", file.deletions)),
                                ),
                        )
                    }),
            )
    }

    fn render_stash_row(&self, stash: &GitStashInfo, cx: &mut Context<Self>) -> impl IntoElement {
        let key = format!("stash-{}", stash.index);
        let index = stash.index;
        let expanded = self.git_expanded.contains(&key);
        let files = self.git_object_files.get(&key).cloned();
        let title = if stash.message.is_empty() {
            stash.name.clone()
        } else {
            stash.message.clone()
        };
        div()
            .flex()
            .flex_col()
            .gap_1()
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_1()
                    .child(
                        Button::new(format!("git-{key}"))
                            .ghost()
                            .flex_1()
                            .h_auto()
                            .min_h_10()
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap_2()
                                    .w_full()
                                    .min_w_0()
                                    .child(
                                        icon(kit_icons::Inbox.1)
                                            .xsmall()
                                            .text_color(cx.theme().muted_foreground),
                                    )
                                    .child(
                                        div()
                                            .flex_1()
                                            .min_w_0()
                                            .flex()
                                            .flex_col()
                                            .items_start()
                                            .child(div().text_sm().truncate().child(title))
                                            .child(
                                                div()
                                                    .text_xs()
                                                    .text_color(cx.theme().muted_foreground)
                                                    .child(stash.relative_time.clone()),
                                            ),
                                    )
                                    .child(
                                        icon(if expanded {
                                            kit_icons::ChevronDown.1
                                        } else {
                                            kit_icons::ChevronRight.1
                                        })
                                        .xsmall()
                                        .text_color(cx.theme().muted_foreground),
                                    ),
                            )
                            .accessibility_label(format!(
                                "{} files in {}",
                                if expanded { "Hide" } else { "Show" },
                                stash.name
                            ))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.toggle_git_object(
                                    key.clone(),
                                    GitOperation::StashFiles { index },
                                    cx,
                                );
                            })),
                    )
                    .child(
                        Button::new(format!("git-stash-pop-{index}"))
                            .ghost()
                            .small()
                            .h_9()
                            .label("Pop")
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.send_git(GitOperation::PopStash {
                                    index: Some(index),
                                });
                                cx.notify();
                            })),
                    )
                    .child(
                        Button::new(format!("git-stash-drop-{index}"))
                            .ghost()
                            .small()
                            .h_9()
                            .label("Drop")
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.send_git(GitOperation::DropStash {
                                    index: Some(index),
                                });
                                cx.notify();
                            })),
                    ),
            )
            .when(expanded, |this| {
                this.child(
                    div().pl_6().flex().flex_col().children(match files {
                        Some(files) if files.is_empty() => vec![div()
                            .text_xs()
                            .text_color(cx.theme().muted_foreground)
                            .child("No files")
                            .into_any_element()],
                        Some(files) => files
                            .iter()
                            .enumerate()
                            .map(|(ix, file)| {
                                self.render_git_file(
                                    format!("git-stash-file-{index}-{ix}"),
                                    file,
                                    GitOperation::DiffStashFile {
                                        index,
                                        path: file.path.clone(),
                                    },
                                    None,
                                    cx,
                                )
                                .into_any_element()
                            })
                            .collect(),
                        None => vec![div()
                            .text_xs()
                            .text_color(cx.theme().muted_foreground)
                            .child("Loading…")
                            .into_any_element()],
                    }),
                )
            })
    }

    fn render_commit_row(&self, commit: &GitCommitInfo, cx: &mut Context<Self>) -> impl IntoElement {
        let key = format!("commit-{}", commit.sha);
        let expanded = self.git_expanded.contains(&key);
        let files = self.git_object_files.get(&key).cloned();
        let sha = commit.sha.clone();
        div()
            .flex()
            .flex_col()
            .gap_1()
            .child(
                Button::new(format!("git-{key}"))
                    .ghost()
                    .w_full()
                    .h_auto()
                    .min_h_12()
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .w_full()
                            .min_w_0()
                            .child(
                                icon(kit_icons::GitCommitHorizontal.1)
                                    .xsmall()
                                    .text_color(cx.theme().muted_foreground),
                            )
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .flex()
                                    .flex_col()
                                    .items_start()
                                    .child(
                                        div().text_sm().truncate().child(commit.summary.clone()),
                                    )
                                    .child(
                                        div()
                                            .text_xs()
                                            .text_color(cx.theme().muted_foreground)
                                            .truncate()
                                            .child(format!(
                                                "{} · {} · {}",
                                                commit.short_sha,
                                                commit.author_name,
                                                commit.relative_time
                                            )),
                                    ),
                            )
                            .child(
                                icon(if expanded {
                                    kit_icons::ChevronDown.1
                                } else {
                                    kit_icons::ChevronRight.1
                                })
                                .xsmall()
                                .text_color(cx.theme().muted_foreground),
                            ),
                    )
                    .accessibility_label(format!(
                        "{} files in commit {}",
                        if expanded { "Hide" } else { "Show" },
                        commit.short_sha
                    ))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.toggle_git_object(
                            key.clone(),
                            GitOperation::CommitFiles { sha: sha.clone() },
                            cx,
                        );
                    })),
            )
            .when(expanded, |this| {
                this.child(
                    div().pl_6().flex().flex_col().children(match files {
                        Some(files) if files.is_empty() => vec![div()
                            .text_xs()
                            .text_color(cx.theme().muted_foreground)
                            .child("No files")
                            .into_any_element()],
                        Some(files) => files
                            .iter()
                            .enumerate()
                            .map(|(ix, file)| {
                                self.render_git_file(
                                    format!("git-commit-file-{}-{ix}", commit.sha),
                                    file,
                                    GitOperation::DiffCommitFile {
                                        sha: commit.sha.clone(),
                                        path: file.path.clone(),
                                    },
                                    None,
                                    cx,
                                )
                                .into_any_element()
                            })
                            .collect(),
                        None => vec![div()
                            .text_xs()
                            .text_color(cx.theme().muted_foreground)
                            .child("Loading…")
                            .into_any_element()],
                    }),
                )
            })
    }

    /// Full-screen unified diff — per-line tinting mirrors the desktop's
    /// diff rows without dragging its tool-detail machinery into mobile.
    fn render_git_diff(&self, title: &str, text: &str, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let rows = if text.is_empty() {
            vec![div()
                .w_full()
                .px_3()
                .py_1()
                .text_xs()
                .text_color(theme.muted_foreground)
                .child("No changes".to_string())
                .into_any_element()]
        } else {
            text.lines()
                .enumerate()
                .map(|(ix, line)| {
                    let (tint, color) = if line.starts_with("+++") || line.starts_with("---") {
                        (theme.muted_foreground.opacity(0.0), theme.muted_foreground)
                    } else if line.starts_with('+') {
                        (theme.success.opacity(0.10), theme.success)
                    } else if line.starts_with('-') {
                        (theme.danger.opacity(0.10), theme.danger)
                    } else if line.starts_with("@@") {
                        (theme.info.opacity(0.10), theme.info)
                    } else if line.starts_with("diff --git") || line.starts_with("index ") {
                        (theme.muted.opacity(0.25), theme.muted_foreground)
                    } else {
                        (theme.muted_foreground.opacity(0.0), theme.foreground)
                    };
                    div()
                        .id(("git-diff-row", ix))
                        .w_full()
                        .px_3()
                        .py_0p5()
                        .bg(tint)
                        .text_xs()
                        .font_family(theme.mono_font_family.clone())
                        .text_color(color)
                        .whitespace_nowrap()
                        .child(line.to_string())
                        .into_any_element()
                })
                .collect()
        };
        div()
            .id("git-diff")
            .absolute()
            .inset_0()
            .flex()
            .flex_col()
            .bg(cx.theme().background)
            .text_color(cx.theme().foreground)
            .child(
                div()
                    .flex_none()
                    .flex()
                    .items_center()
                    .gap_2()
                    .px_3()
                    .py_3()
                    .border_b_1()
                    .border_color(cx.theme().border)
                    .child(
                        Button::new("close-diff")
                            .ghost()
                            .h_11()
                            .icon(icon(kit_icons::X.1))
                            .accessibility_label("Close diff")
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.git_diff = None;
                                cx.notify();
                            })),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .font_bold()
                            .child(title.to_string()),
                    ),
            )
            .child(
                div()
                    .id("git-diff-body")
                    .flex_1()
                    .min_h_0()
                    .w_full()
                    .overflow_y_scroll()
                    .overflow_x_scrollbar()
                    .child(div().flex().flex_col().py_1().children(rows)),
            )
    }
}

impl MobileApp {
    /// Shared panel header: drawer button, title + link state, a refresh
    /// affordance. `refresh` is the panel's own list reload so the header
    /// stays one shape.
    fn github_panel_header(
        &self,
        title: String,
        refresh: fn(&Self),
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let status_line = self.client.session_status.clone();
        div()
            .flex_none()
            .flex()
            .items_center()
            .gap_2()
            .px_3()
            .py_3()
            .border_b_1()
            .border_color(cx.theme().border)
            .child(
                Button::new(format!("panel-menu-{title}"))
                    .ghost()
                    .h_11()
                    .icon(icon(kit_icons::Menu.1))
                    .accessibility_label("Menu")
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.sidebar_open = true;
                        cx.notify();
                    })),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .child(div().font_bold().truncate().child(title.clone()))
                            .child(self.link_dot(cx)),
                    )
                    .when_some(status_line, |this, line| {
                        this.child(
                            div()
                                .text_xs()
                                .text_color(cx.theme().muted_foreground)
                                .child(line),
                        )
                    }),
            )
            .child(
                Button::new(format!("panel-refresh-{title}"))
                    .ghost()
                    .h_11()
                    .icon(icon(kit_icons::RefreshCw.1))
                    .accessibility_label("Refresh")
                    .on_click(cx.listener(move |this, _, _, cx| {
                        refresh(this);
                        cx.notify();
                    })),
            )
    }

    /// Empty state when no project is selected — every project-scoped
    /// panel lands here until the sidebar pick resolves.
    fn render_choose_project(
        &self,
        id: &'static str,
        glyph: &'static [u8],
        hint: &'static str,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        div()
            .flex_1()
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .gap_3()
            .px_6()
            .child(
                icon(glyph)
                    .large()
                    .text_color(cx.theme().muted_foreground),
            )
            .child(
                div()
                    .text_sm()
                    .text_color(cx.theme().muted_foreground)
                    .child(hint),
            )
            .child(
                Button::new(id)
                    .primary()
                    .h_11()
                    .label("Choose project")
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.sidebar_open = true;
                        cx.notify();
                    })),
            )
    }

    fn render_issues_panel(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let work_dir = self.client.sidebar_project_filter.clone();
        let filter = self.issue_filter;
        let entity = cx.entity();
        let states = [GitHubIssueListState::Open, GitHubIssueListState::Closed];
        div()
            .size_full()
            .flex()
            .flex_col()
            .bg(cx.theme().background)
            .text_color(cx.theme().foreground)
            .child(self.github_panel_header("Issues".into(), Self::refresh_issues, cx))
            .when(work_dir.is_some(), |this| {
                this.child(
                    div()
                        .flex_none()
                        .flex()
                        .items_center()
                        .gap_2()
                        .px_3()
                        .py_2()
                        .child(
                            Button::new("issue-state-filter")
                                .outline()
                                .h_10()
                                .label(format!("{} ▾", filter.as_str()))
                                .accessibility_label(format!("Issue filter: {}", filter.as_str()))
                                .dropdown_caret(true)
                                .dropdown_menu_with_anchor(Anchor::BottomLeft, move |menu, _, _| {
                                    states.iter().fold(menu, |menu, state| {
                                        let entity = entity.clone();
                                        let state = *state;
                                        menu.item(
                                            PopupMenuItem::new(state.as_str().to_string())
                                                .checked(state == filter)
                                                .on_click(move |_, _, cx| {
                                                    let _ = entity.update(cx, |this, cx| {
                                                        this.issue_filter = state;
                                                        this.issues = None;
                                                        this.refresh_issues();
                                                        cx.notify();
                                                    });
                                                }),
                                        )
                                    })
                                }),
                        )
                        .child(
                            div().flex_1().min_w_0().child(
                                Input::new(&self.issue_search)
                                    .aria_label("Search issues")
                                    .h_10(),
                            ),
                        )
                        .child(
                            Button::new("issue-search-go")
                                .ghost()
                                .h_10()
                                .icon(icon(kit_icons::Search.1))
                                .accessibility_label("Search")
                                .on_click(cx.listener(|this, _, _, cx| {
                                    let query = this.issue_search.read(cx).value().trim().to_owned();
                                    this.send_github(GitHubOperation::ListIssues {
                                        state: this.issue_filter,
                                        query: (!query.is_empty()).then_some(query),
                                        limit: 30,
                                    });
                                    cx.notify();
                                })),
                        )
                        .child(
                            Button::new("issue-new")
                                .primary()
                                .h_10()
                                .icon(icon(kit_icons::Plus.1))
                                .accessibility_label("New issue")
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.new_issue_open = true;
                                    cx.notify();
                                })),
                        ),
                )
            })
            .child(
                div()
                    .id("issue-list")
                    .flex_1()
                    .min_h_0()
                    .w_full()
                    .overflow_y_scroll()
                    .flex()
                    .flex_col()
                    .when(work_dir.is_none(), |this| {
                        this.child(self.render_choose_project(
                            "issues-choose",
                            kit_icons::CircleDot.1,
                            "Choose a project to see its issues",
                            cx,
                        ))
                    })
                    .when(work_dir.is_some() && self.issues.is_none(), |this| {
                        this.child(
                            div()
                                .flex_1()
                                .flex()
                                .items_center()
                                .justify_center()
                                .child(
                                    div()
                                        .text_sm()
                                        .text_color(cx.theme().muted_foreground)
                                        .child("Loading issues…"),
                                ),
                        )
                    })
                    .when_some(self.issues.clone(), |this, issues| {
                        if issues.is_empty() {
                            this.child(
                                div()
                                    .flex_1()
                                    .flex()
                                    .flex_col()
                                    .items_center()
                                    .justify_center()
                                    .gap_2()
                                    .child(
                                        div()
                                            .text_sm()
                                            .text_color(cx.theme().muted_foreground)
                                            .child(format!("No {} issues", filter.as_str())),
                                    ),
                            )
                        } else {
                            this.children(
                                issues
                                    .iter()
                                    .map(|issue| self.render_issue_row(issue, cx)),
                            )
                        }
                    }),
            )
    }

    fn render_issue_row(
        &self,
        issue: &GitHubIssueSummary,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let number = issue.issue.number;
        let open = issue.state == "open";
        let dot = if open {
            cx.theme().success
        } else {
            cx.theme().muted_foreground
        };
        div().px_2().py_1().child(
            Button::new(format!("issue-row-{number}"))
                .ghost()
                .w_full()
                .h_auto()
                .min_h_12()
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap_2()
                        .w_full()
                        .min_w_0()
                        .child(
                            icon(kit_icons::CircleDot.1)
                                .xsmall()
                                .text_color(dot),
                        )
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .flex()
                                .flex_col()
                                .items_start()
                                .gap_1()
                                .child(
                                    div().text_sm().font_bold().truncate().child(issue.title.clone()),
                                )
                                .child(
                                    div()
                                        .text_xs()
                                        .text_color(cx.theme().muted_foreground)
                                        .truncate()
                                        .child(format!(
                                            "#{number} · {} · {}",
                                            issue.author, issue.updated_at
                                        )),
                                ),
                        )
                        .children(issue.labels.iter().take(2).map(|label| {
                            Tag::new().small().child(label.name.clone()).into_any_element()
                        }))
                        .when(issue.comments_count > 0, |this| {
                            this.child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap_1()
                                    .child(
                                        icon(kit_icons::MessageSquare.1)
                                            .xsmall()
                                            .text_color(cx.theme().muted_foreground),
                                    )
                                    .child(
                                        div()
                                            .text_xs()
                                            .text_color(cx.theme().muted_foreground)
                                            .child(issue.comments_count.to_string()),
                                    ),
                            )
                        }),
                )
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.issue_detail_pending = Some(number);
                    this.issue_confirm_delete = false;
                    this.send_github(GitHubOperation::InspectIssue { number });
                    cx.notify();
                })),
        )
    }

    /// Issue detail as a full-screen overlay: body, labels, the comment
    /// thread, and a footer with the comment composer and state actions.
    fn render_issue_detail(
        &mut self,
        detail: &GitHubIssueDetail,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let issue = &detail.summary;
        let number = issue.issue.number;
        let open = issue.state == "open";
        let pending = self.issue_detail_pending == Some(number);
        let confirming = self.issue_confirm_delete;
        let comment_ready = !self.issue_comment.read(cx).value().trim().is_empty();
        div()
            .id("issue-detail")
            .absolute()
            .inset_0()
            .flex()
            .flex_col()
            .bg(cx.theme().background)
            .text_color(cx.theme().foreground)
            .child(
                div()
                    .flex_none()
                    .flex()
                    .items_center()
                    .gap_2()
                    .px_3()
                    .py_3()
                    .border_b_1()
                    .border_color(cx.theme().border)
                    .child(
                        Button::new("issue-detail-close")
                            .ghost()
                            .h_11()
                            .icon(icon(kit_icons::X.1))
                            .accessibility_label("Close issue")
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.issue_detail = None;
                                cx.notify();
                            })),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .font_bold()
                            .child(format!("#{number} — {}", issue.title)),
                    )
                    .child(Tag::new().small().child(issue.state.clone()))
                    .when(pending, |this| {
                        this.child(
                            div()
                                .text_xs()
                                .text_color(cx.theme().muted_foreground)
                                .child("…"),
                        )
                    }),
            )
            .child(
                div()
                    .id("issue-detail-body")
                    .flex_1()
                    .min_h_0()
                    .w_full()
                    .overflow_y_scroll()
                    .child(
                        div()
                            .px_4()
                            .py_3()
                            .flex()
                            .flex_col()
                            .gap_3()
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(cx.theme().muted_foreground)
                                    .child(format!(
                                        "Opened by {} · updated {}",
                                        issue.author, issue.updated_at
                                    )),
                            )
                            .when(!issue.labels.is_empty(), |this| {
                                this.child(
                                    div().flex().gap_1().flex_wrap().children(
                                        issue.labels.iter().map(|label| {
                                            Tag::new().small().child(label.name.clone())
                                        }),
                                    ),
                                )
                            })
                            .child(
                                div()
                                    .text_sm()
                                    .child(if detail.body.trim().is_empty() {
                                        "No description.".to_string()
                                    } else {
                                        detail.body.clone()
                                    }),
                            )
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(cx.theme().muted_foreground)
                                    .pt_2()
                                    .child(format!("Comments ({})", detail.comments.len())),
                            )
                            .children(detail.comments.iter().map(|comment| {
                                div()
                                    .border_1()
                                    .border_color(cx.theme().border)
                                    .rounded_md()
                                    .p_3()
                                    .flex()
                                    .flex_col()
                                    .gap_1()
                                    .child(
                                        div()
                                            .text_xs()
                                            .text_color(cx.theme().muted_foreground)
                                            .child(format!(
                                                "{} · {}",
                                                comment.author, comment.created_at
                                            )),
                                    )
                                    .child(div().text_sm().child(comment.body.clone()))
                            })),
                    ),
            )
            .when(confirming, |this| {
                this.child(
                    div()
                        .flex_none()
                        .flex()
                        .items_center()
                        .gap_2()
                        .px_4()
                        .py_2()
                        .bg(cx.theme().danger.opacity(0.08))
                        .child(
                            div()
                                .flex_1()
                                .text_sm()
                                .text_color(cx.theme().danger)
                                .child(format!("Delete issue #{number}? This cannot be undone.")),
                        )
                        .child(
                            Button::new("issue-delete-confirm")
                                .danger()
                                .small()
                                .label("Delete")
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.issue_confirm_delete = false;
                                    this.send_github(GitHubOperation::DeleteIssue { number });
                                    cx.notify();
                                })),
                        )
                        .child(
                            Button::new("issue-delete-cancel")
                                .ghost()
                                .small()
                                .label("Cancel")
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.issue_confirm_delete = false;
                                    cx.notify();
                                })),
                        ),
                )
            })
            .child(
                div()
                    .flex_none()
                    .border_t_1()
                    .border_color(cx.theme().border)
                    .p_3()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .child(Textarea::new(&self.issue_comment).aria_label("Add a comment"))
                    .child(
                        div()
                            .flex()
                            .gap_2()
                            .child(
                                Button::new("issue-comment-send")
                                    .primary()
                                    .h_11()
                                    .flex_1()
                                    .label("Comment")
                                    .disabled(!comment_ready)
                                    .on_click(cx.listener(move |this, _, window, cx| {
                                        let body = this
                                            .issue_comment
                                            .read(cx)
                                            .value()
                                            .trim()
                                            .to_owned();
                                        if body.is_empty() {
                                            return;
                                        }
                                        this.send_github(GitHubOperation::CommentIssue {
                                            number,
                                            body,
                                        });
                                        this.issue_comment.update(cx, |input, cx| {
                                            input.set_value("", window, cx)
                                        });
                                        cx.notify();
                                    })),
                            )
                            .child(
                                Button::new("issue-toggle-state")
                                    .outline()
                                    .h_11()
                                    .label(if open { "Close" } else { "Reopen" })
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.send_github(GitHubOperation::SetIssueState {
                                            number,
                                            close: open,
                                        });
                                        cx.notify();
                                    })),
                            )
                            .child(
                                Button::new("issue-delete")
                                    .danger()
                                    .h_11()
                                    .icon(icon(kit_icons::Trash.1))
                                    .accessibility_label("Delete issue")
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.issue_confirm_delete = true;
                                        cx.notify();
                                    })),
                            ),
                    ),
            )
    }

    /// The new-issue form — another overlay, since the composer belongs
    /// over the list it will update.
    fn render_new_issue(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let title_ready = !self.issue_title.read(cx).value().trim().is_empty();
        div()
            .id("new-issue")
            .absolute()
            .inset_0()
            .flex()
            .flex_col()
            .bg(cx.theme().background)
            .text_color(cx.theme().foreground)
            .child(
                div()
                    .flex_none()
                    .flex()
                    .items_center()
                    .gap_2()
                    .px_3()
                    .py_3()
                    .border_b_1()
                    .border_color(cx.theme().border)
                    .child(
                        Button::new("new-issue-close")
                            .ghost()
                            .h_11()
                            .icon(icon(kit_icons::X.1))
                            .accessibility_label("Cancel new issue")
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.new_issue_open = false;
                                cx.notify();
                            })),
                    )
                    .child(div().flex_1().font_bold().child("New issue")),
            )
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .px_4()
                    .py_3()
                    .flex()
                    .flex_col()
                    .gap_3()
                    .child(
                        Input::new(&self.issue_title)
                            .aria_label("Issue title")
                            .h_11(),
                    )
                    .child(Textarea::new(&self.issue_body).aria_label("Issue body")),
            )
            .child(
                div()
                    .flex_none()
                    .border_t_1()
                    .border_color(cx.theme().border)
                    .p_3()
                    .flex()
                    .gap_2()
                    .child(
                        Button::new("new-issue-create")
                            .primary()
                            .h_11()
                            .flex_1()
                            .label("Create issue")
                            .disabled(!title_ready)
                            .on_click(cx.listener(|this, _, window, cx| {
                                let title = this.issue_title.read(cx).value().trim().to_owned();
                                if title.is_empty() {
                                    return;
                                }
                                let body = this.issue_body.read(cx).value().trim().to_owned();
                                this.send_github(GitHubOperation::CreateIssue { title, body });
                                this.issue_title.update(cx, |input, cx| {
                                    input.set_value("", window, cx)
                                });
                                this.issue_body.update(cx, |input, cx| {
                                    input.set_value("", window, cx)
                                });
                                cx.notify();
                            })),
                    ),
            )
    }

    fn render_pulls_panel(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let work_dir = self.client.sidebar_project_filter.clone();
        let filter = self.pr_filter;
        let entity = cx.entity();
        let states = [
            GitHubPrListState::Open,
            GitHubPrListState::Closed,
            GitHubPrListState::Merged,
        ];
        div()
            .size_full()
            .flex()
            .flex_col()
            .bg(cx.theme().background)
            .text_color(cx.theme().foreground)
            .child(self.github_panel_header("Pull requests".into(), Self::refresh_prs, cx))
            .when(work_dir.is_some(), |this| {
                this.child(
                    div()
                        .flex_none()
                        .flex()
                        .items_center()
                        .gap_2()
                        .px_3()
                        .py_2()
                        .child(
                            Button::new("pr-state-filter")
                                .outline()
                                .h_10()
                                .label(format!("{} ▾", filter.as_str()))
                                .accessibility_label(format!("Pull request filter: {}", filter.as_str()))
                                .dropdown_caret(true)
                                .dropdown_menu_with_anchor(Anchor::BottomLeft, move |menu, _, _| {
                                    states.iter().fold(menu, |menu, state| {
                                        let entity = entity.clone();
                                        let state = *state;
                                        menu.item(
                                            PopupMenuItem::new(state.as_str().to_string())
                                                .checked(state == filter)
                                                .on_click(move |_, _, cx| {
                                                    let _ = entity.update(cx, |this, cx| {
                                                        this.pr_filter = state;
                                                        this.prs = None;
                                                        this.refresh_prs();
                                                        cx.notify();
                                                    });
                                                }),
                                        )
                                    })
                                }),
                        )
                        .child(
                            div().flex_1().min_w_0().child(
                                Input::new(&self.pr_search)
                                    .aria_label("Search pull requests")
                                    .h_10(),
                            ),
                        )
                        .child(
                            Button::new("pr-search-go")
                                .ghost()
                                .h_10()
                                .icon(icon(kit_icons::Search.1))
                                .accessibility_label("Search")
                                .on_click(cx.listener(|this, _, _, cx| {
                                    let query = this.pr_search.read(cx).value().trim().to_owned();
                                    this.send_github(GitHubOperation::ListPullRequests {
                                        state: this.pr_filter,
                                        query: (!query.is_empty()).then_some(query),
                                        limit: 30,
                                    });
                                    cx.notify();
                                })),
                        ),
                )
            })
            .child(
                div()
                    .id("pr-list")
                    .flex_1()
                    .min_h_0()
                    .w_full()
                    .overflow_y_scroll()
                    .flex()
                    .flex_col()
                    .when(work_dir.is_none(), |this| {
                        this.child(self.render_choose_project(
                            "prs-choose",
                            kit_icons::GitPullRequest.1,
                            "Choose a project to see its pull requests",
                            cx,
                        ))
                    })
                    .when(work_dir.is_some() && self.prs.is_none(), |this| {
                        this.child(
                            div()
                                .flex_1()
                                .flex()
                                .items_center()
                                .justify_center()
                                .child(
                                    div()
                                        .text_sm()
                                        .text_color(cx.theme().muted_foreground)
                                        .child("Loading pull requests…"),
                                ),
                        )
                    })
                    .when_some(self.prs.clone(), |this, prs| {
                        if prs.is_empty() {
                            this.child(
                                div()
                                    .flex_1()
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .child(
                                        div()
                                            .text_sm()
                                            .text_color(cx.theme().muted_foreground)
                                            .child(format!("No {} pull requests", filter.as_str())),
                                    ),
                            )
                        } else {
                            this.children(prs.iter().map(|pr| self.render_pr_row(pr, cx)))
                        }
                    }),
            )
    }

    fn render_pr_row(
        &self,
        pr: &GitHubPullRequestSummary,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let number = pr.number;
        let glyph = if pr.state == "merged" {
            cx.theme().info
        } else if pr.state == "open" {
            cx.theme().success
        } else {
            cx.theme().muted_foreground
        };
        let (passing, total) = pr
            .checks
            .iter()
            .fold((0usize, 0usize), |(ok, all), check| {
                (ok + usize::from(check.conclusion.as_deref() == Some("success")), all + 1)
            });
        div().px_2().py_1().child(
            Button::new(format!("pr-row-{number}"))
                .ghost()
                .w_full()
                .h_auto()
                .min_h_12()
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap_2()
                        .w_full()
                        .min_w_0()
                        .child(icon(kit_icons::GitPullRequest.1).xsmall().text_color(glyph))
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .flex()
                                .flex_col()
                                .items_start()
                                .gap_1()
                                .child(
                                    div().text_sm().font_bold().truncate().child(pr.title.clone()),
                                )
                                .child(
                                    div()
                                        .text_xs()
                                        .text_color(cx.theme().muted_foreground)
                                        .truncate()
                                        .child(format!(
                                            "#{number} · {} → {} · {} · {}",
                                            pr.head_ref, pr.base_ref, pr.author, pr.updated_at
                                        )),
                                ),
                        )
                        .when(pr.is_draft, |this| {
                            this.child(Tag::new().small().child("Draft"))
                        })
                        .when(total > 0, |this| {
                            let ok = passing == total;
                            this.child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap_1()
                                    .child(
                                        icon(if ok {
                                            kit_icons::CircleCheck.1
                                        } else {
                                            kit_icons::CircleX.1
                                        })
                                        .xsmall()
                                        .text_color(if ok {
                                            cx.theme().success
                                        } else {
                                            cx.theme().danger
                                        }),
                                    )
                                    .child(
                                        div()
                                            .text_xs()
                                            .text_color(if ok {
                                                cx.theme().success
                                            } else {
                                                cx.theme().danger
                                            })
                                            .child(format!("{passing}/{total}")),
                                    ),
                            )
                        }),
                )
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.pr_detail_pending = Some(number);
                    this.send_github(GitHubOperation::InspectPullRequest { number });
                    cx.notify();
                })),
        )
    }

    /// Pull-request detail overlay: state tags, refs, checks, files, the
    /// conversation, a View-diff button, and a comment composer.
    fn render_pr_detail(&mut self, pr: &GitHubPrInfo, cx: &mut Context<Self>) -> impl IntoElement {
        let number = pr.number;
        let pending = self.pr_detail_pending == Some(number);
        let comment_ready = !self.pr_comment.read(cx).value().trim().is_empty();
        div()
            .id("pr-detail")
            .absolute()
            .inset_0()
            .flex()
            .flex_col()
            .bg(cx.theme().background)
            .text_color(cx.theme().foreground)
            .child(
                div()
                    .flex_none()
                    .flex()
                    .items_center()
                    .gap_2()
                    .px_3()
                    .py_3()
                    .border_b_1()
                    .border_color(cx.theme().border)
                    .child(
                        Button::new("pr-detail-close")
                            .ghost()
                            .h_11()
                            .icon(icon(kit_icons::X.1))
                            .accessibility_label("Close pull request")
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.pr_detail = None;
                                cx.notify();
                            })),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .font_bold()
                            .child(format!("#{number} — {}", pr.title)),
                    )
                    .when(pending, |this| {
                        this.child(
                            div()
                                .text_xs()
                                .text_color(cx.theme().muted_foreground)
                                .child("…"),
                        )
                    }),
            )
            .child(
                div()
                    .id("pr-detail-body")
                    .flex_1()
                    .min_h_0()
                    .w_full()
                    .overflow_y_scroll()
                    .child(
                        div()
                            .px_4()
                            .py_3()
                            .flex()
                            .flex_col()
                            .gap_3()
                            .child(
                                div()
                                    .flex()
                                    .gap_1()
                                    .flex_wrap()
                                    .child(Tag::new().small().child(pr.state.clone()))
                                    .when(pr.is_draft, |this| {
                                        this.child(Tag::new().small().child("Draft"))
                                    })
                                    .when_some(pr.review_decision.clone(), |this, decision| {
                                        this.child(Tag::new().small().child(decision))
                                    }),
                            )
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(cx.theme().muted_foreground)
                                    .child(format!(
                                        "{} → {} · {} · updated {} · {} comments",
                                        pr.head_ref,
                                        pr.base_ref,
                                        pr.author,
                                        pr.updated_at,
                                        pr.comments_count
                                    )),
                            )
                            .child(
                                Button::new("pr-view-diff")
                                    .outline()
                                    .h_11()
                                    .icon(icon(kit_icons::FileDiff.1))
                                    .label("View diff")
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.git_diff_pending =
                                            Some(format!("PR #{number} diff"));
                                        this.send_github(GitHubOperation::PullRequestDiff {
                                            number,
                                        });
                                        cx.notify();
                                    })),
                            )
                            .when(!pr.body.trim().is_empty(), |this| {
                                this.child(div().text_sm().child(pr.body.clone()))
                            })
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(cx.theme().muted_foreground)
                                    .pt_2()
                                    .child(format!("Checks ({})", pr.checks.len())),
                            )
                            .when(pr.checks.is_empty(), |this| {
                                this.child(
                                    div()
                                        .text_sm()
                                        .text_color(cx.theme().muted_foreground)
                                        .child("No checks reported"),
                                )
                            })
                            .children(pr.checks.iter().map(|check| {
                                let (glyph, color) = match check.conclusion.as_deref() {
                                    Some("success") => {
                                        (kit_icons::CircleCheck.1, cx.theme().success)
                                    }
                                    Some(_) => (kit_icons::CircleX.1, cx.theme().danger),
                                    None => (kit_icons::Loader.1, cx.theme().muted_foreground),
                                };
                                div()
                                    .flex()
                                    .items_center()
                                    .gap_2()
                                    .child(icon(glyph).xsmall().text_color(color))
                                    .child(div().flex_1().min_w_0().text_sm().truncate().child(
                                        check.name.clone(),
                                    ))
                                    .child(
                                        div()
                                            .text_xs()
                                            .text_color(cx.theme().muted_foreground)
                                            .child(
                                                check
                                                    .conclusion
                                                    .clone()
                                                    .unwrap_or_else(|| check.status.clone()),
                                            ),
                                    )
                            }))
                            .when(!pr.files.is_empty(), |this| {
                                this.child(
                                    div()
                                        .text_xs()
                                        .text_color(cx.theme().muted_foreground)
                                        .pt_2()
                                        .child(format!("Files ({})", pr.files.len())),
                                )
                            })
                            .children(pr.files.iter().map(|file| {
                                div()
                                    .flex()
                                    .items_center()
                                    .gap_2()
                                    .child(
                                        div()
                                            .flex_1()
                                            .min_w_0()
                                            .text_sm()
                                            .truncate()
                                            .child(file.path.clone()),
                                    )
                                    .child(
                                        div()
                                            .text_xs()
                                            .text_color(cx.theme().success)
                                            .child(format!("+{}", file.additions)),
                                    )
                                    .child(
                                        div()
                                            .text_xs()
                                            .text_color(cx.theme().danger)
                                            .child(format!("-{}", file.deletions)),
                                    )
                            }))
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(cx.theme().muted_foreground)
                                    .pt_2()
                                    .child(format!("Conversation ({})", pr.issue_comments.len())),
                            )
                            .when(pr.issue_comments.is_empty(), |this| {
                                this.child(
                                    div()
                                        .text_sm()
                                        .text_color(cx.theme().muted_foreground)
                                        .child("No comments yet"),
                                )
                            })
                            .children(pr.issue_comments.iter().map(|comment| {
                                div()
                                    .border_1()
                                    .border_color(cx.theme().border)
                                    .rounded_md()
                                    .p_3()
                                    .flex()
                                    .flex_col()
                                    .gap_1()
                                    .child(
                                        div()
                                            .text_xs()
                                            .text_color(cx.theme().muted_foreground)
                                            .child(format!(
                                                "{} · {}",
                                                comment.author, comment.created_at
                                            )),
                                    )
                                    .child(div().text_sm().child(comment.body.clone()))
                            }))
                            .when(!pr.reviews.is_empty(), |this| {
                                this.child(
                                    div()
                                        .text_xs()
                                        .text_color(cx.theme().muted_foreground)
                                        .pt_2()
                                        .child(format!("Reviews ({})", pr.reviews.len())),
                                )
                            })
                            .children(pr.reviews.iter().map(|review| {
                                div()
                                    .border_1()
                                    .border_color(cx.theme().border)
                                    .rounded_md()
                                    .p_3()
                                    .flex()
                                    .flex_col()
                                    .gap_1()
                                    .child(
                                        div()
                                            .flex()
                                            .items_center()
                                            .gap_2()
                                            .child(Tag::new().small().child(review.state.clone()))
                                            .child(
                                                div()
                                                    .text_xs()
                                                    .text_color(cx.theme().muted_foreground)
                                                    .child(format!(
                                                        "{} · {}",
                                                        review.author, review.submitted_at
                                                    )),
                                            ),
                                    )
                                    .when(!review.body.trim().is_empty(), |this| {
                                        this.child(div().text_sm().child(review.body.clone()))
                                    })
                            })),
                    ),
            )
            .child(
                div()
                    .flex_none()
                    .border_t_1()
                    .border_color(cx.theme().border)
                    .p_3()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .child(Textarea::new(&self.pr_comment).aria_label("Add a comment"))
                    .child(
                        Button::new("pr-comment-send")
                            .primary()
                            .h_11()
                            .w_full()
                            .label("Comment")
                            .disabled(!comment_ready)
                            .on_click(cx.listener(move |this, _, window, cx| {
                                let body = this.pr_comment.read(cx).value().trim().to_owned();
                                if body.is_empty() {
                                    return;
                                }
                                this.send_github(GitHubOperation::CommentPullRequest {
                                    number,
                                    body,
                                });
                                this.pr_comment.update(cx, |input, cx| {
                                    input.set_value("", window, cx)
                                });
                                cx.notify();
                            })),
                    ),
            )
    }
}

impl MobileApp {
    /// The automations tab mirrors the daemon-global store: definitions
    /// with enable/run/edit/delete controls, then the run history with
    /// cancel/review/open-chat affordances.
    fn render_automations_panel(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let snapshot = self.client.automation.snapshot.clone();
        let attention: HashSet<String> = self
            .client
            .automation_permissions
            .keys()
            .chain(self.client.automation_questions.keys())
            .cloned()
            .collect();
        let delete_def = self.auto_delete_confirm.clone();
        let delete_run = self.auto_run_delete_confirm.clone();
        div()
            .size_full()
            .flex()
            .flex_col()
            .bg(cx.theme().background)
            .text_color(cx.theme().foreground)
            .child(
                self.github_panel_header(
                    "Automations".into(),
                    Self::refresh_automations,
                    cx,
                ),
            )
            .child(
                div().flex_none().px_3().py_2().child(
                    Button::new("auto-new")
                        .primary()
                        .h_10()
                        .w_full()
                        .icon(icon(kit_icons::Plus.1))
                        .label("New automation")
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.open_auto_form(None, window, cx)
                        })),
                ),
            )
            .when_some(delete_def, |this, id| {
                this.child(
                    div()
                        .flex_none()
                        .flex()
                        .items_center()
                        .gap_2()
                        .px_4()
                        .py_2()
                        .bg(cx.theme().danger.opacity(0.08))
                        .child(
                            div()
                                .flex_1()
                                .text_sm()
                                .text_color(cx.theme().danger)
                                .child("Delete this automation and its runs?"),
                        )
                        .child(
                            Button::new("auto-delete-confirm")
                                .danger()
                                .small()
                                .label("Delete")
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.auto_delete_confirm = None;
                                    this.send_automation(AutomationCommand::Delete {
                                        id: id.clone(),
                                    });
                                    cx.notify();
                                })),
                        )
                        .child(
                            Button::new("auto-delete-cancel")
                                .ghost()
                                .small()
                                .label("Cancel")
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.auto_delete_confirm = None;
                                    cx.notify();
                                })),
                        ),
                )
            })
            .when_some(delete_run, |this, id| {
                this.child(
                    div()
                        .flex_none()
                        .flex()
                        .items_center()
                        .gap_2()
                        .px_4()
                        .py_2()
                        .bg(cx.theme().danger.opacity(0.08))
                        .child(
                            div()
                                .flex_1()
                                .text_sm()
                                .text_color(cx.theme().danger)
                                .child("Delete this run? An active run is cancelled."),
                        )
                        .child(
                            Button::new("auto-run-delete-confirm")
                                .danger()
                                .small()
                                .label("Delete")
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.auto_run_delete_confirm = None;
                                    this.send_automation(AutomationCommand::DeleteRun {
                                        id: id.clone(),
                                    });
                                    cx.notify();
                                })),
                        )
                        .child(
                            Button::new("auto-run-delete-cancel")
                                .ghost()
                                .small()
                                .label("Cancel")
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.auto_run_delete_confirm = None;
                                    cx.notify();
                                })),
                        ),
                )
            })
            .child(
                div()
                    .id("auto-list")
                    .flex_1()
                    .min_h_0()
                    .w_full()
                    .overflow_y_scroll()
                    .child(
                        div()
                            .px_3()
                            .pb_4()
                            .flex()
                            .flex_col()
                            .gap_3()
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(cx.theme().muted_foreground)
                                    .pt_1()
                                    .child(format!("Definitions ({})", snapshot.definitions.len())),
                            )
                            .when(snapshot.definitions.is_empty(), |this| {
                                this.child(
                                    div()
                                        .py_6()
                                        .flex()
                                        .flex_col()
                                        .items_center()
                                        .gap_2()
                                        .child(
                                            icon(kit_icons::Zap.1)
                                                .large()
                                                .text_color(cx.theme().muted_foreground),
                                        )
                                        .child(
                                            div()
                                                .text_sm()
                                                .text_color(cx.theme().muted_foreground)
                                                .child("No automations yet — create one above"),
                                        ),
                                )
                            })
                            .children(
                                snapshot
                                    .definitions
                                    .iter()
                                    .map(|def| self.render_auto_definition(def, cx)),
                            )
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(cx.theme().muted_foreground)
                                    .pt_2()
                                    .child(format!("Runs ({})", snapshot.runs.len())),
                            )
                            .when(snapshot.runs.is_empty(), |this| {
                                this.child(
                                    div()
                                        .text_sm()
                                        .text_color(cx.theme().muted_foreground)
                                        .child("No runs recorded"),
                                )
                            })
                            .children(
                                snapshot
                                    .runs
                                    .iter()
                                    .map(|run| self.render_auto_run(run, cx, &attention)),
                            ),
                    ),
            )
    }

    fn render_auto_definition(
        &self,
        def: &Definition,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let toggle_id = def.id.clone();
        let run_id = def.id.clone();
        let delete_id = def.id.clone();
        let enabled = def.enabled;
        let definition = def.clone();
        let project_name = def
            .project
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default();
        let meta = format!(
            "{} · {}{}{}",
            def.schedule.label(),
            project_name,
            if def.failures > 0 {
                format!(" · {} failures", def.failures)
            } else {
                String::new()
            },
            def.next_at
                .map(|at| format!(
                    " · next {}",
                    threadlane_protocol::automation::display_time(at, "UTC")
                ))
                .unwrap_or_default()
        );
        div()
            .border_1()
            .border_color(cx.theme().border)
            .rounded_md()
            .p_3()
            .flex()
            .flex_col()
            .gap_2()
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(
                        icon(if def.enabled {
                            kit_icons::Zap.1
                        } else {
                            kit_icons::ZapOff.1
                        })
                        .xsmall()
                        .text_color(if def.enabled {
                            cx.theme().success
                        } else {
                            cx.theme().muted_foreground
                        }),
                    )
                    .child(div().flex_1().min_w_0().font_bold().truncate().child(
                        if def.name.trim().is_empty() {
                            "Untitled automation".to_string()
                        } else {
                            def.name.clone()
                        },
                    ))
                    .when_some(def.paused_reason.clone(), |this, reason| {
                        this.child(Tag::new().small().child(reason))
                    }),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child(meta),
            )
            .child(
                div()
                    .flex()
                    .gap_2()
                    .child(
                        Button::new(format!("auto-toggle-{}", def.id))
                            .ghost()
                            .small()
                            .h_10()
                            .icon(icon(if enabled {
                                kit_icons::ZapOff.1
                            } else {
                                kit_icons::Zap.1
                            }))
                            .label(if enabled { "Disable" } else { "Enable" })
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.send_automation(AutomationCommand::SetEnabled {
                                    id: toggle_id.clone(),
                                    enabled: !enabled,
                                });
                                cx.notify();
                            })),
                    )
                    .child(
                        Button::new(format!("auto-run-{}", def.id))
                            .outline()
                            .small()
                            .h_10()
                            .icon(icon(kit_icons::Play.1))
                            .label("Run now")
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.send_automation(AutomationCommand::RunNow {
                                    id: run_id.clone(),
                                });
                                cx.notify();
                            })),
                    )
                    .child(
                        Button::new(format!("auto-edit-{}", def.id))
                            .ghost()
                            .small()
                            .h_10()
                            .label("Edit")
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.open_auto_form(Some(definition.clone()), window, cx)
                            })),
                    )
                    .child(
                        Button::new(format!("auto-delete-{}", def.id))
                            .ghost()
                            .small()
                            .h_10()
                            .icon(icon(kit_icons::Trash.1))
                            .accessibility_label("Delete automation")
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.auto_delete_confirm = Some(delete_id.clone());
                                cx.notify();
                            })),
                    ),
            )
    }

    fn render_auto_run(
        &self,
        run: &Run,
        cx: &mut Context<Self>,
        attention: &HashSet<String>,
    ) -> impl IntoElement {
        let cancel_id = run.id.clone();
        let review_id = run.id.clone();
        let delete_id = run.id.clone();
        let active = run.status.active();
        let needs_you = attention.contains(&run.session_id)
            || matches!(
                run.status,
                RunStatus::WaitingPermission | RunStatus::WaitingAnswer
            );
        let (glyph, color) = match run.status {
            RunStatus::Succeeded => (kit_icons::CircleCheck.1, cx.theme().success),
            RunStatus::Failed => (kit_icons::CircleX.1, cx.theme().danger),
            RunStatus::Cancelled | RunStatus::Interrupted => {
                (kit_icons::Ban.1, cx.theme().muted_foreground)
            }
            RunStatus::WaitingPermission | RunStatus::WaitingAnswer => {
                (kit_icons::TriangleAlert.1, cx.theme().warning)
            }
            _ => (kit_icons::Loader.1, cx.theme().info),
        };
        let times = format!(
            "started {}{}",
            threadlane_protocol::automation::display_time(run.created_at, "UTC"),
            run.finished_at
                .map(|at| format!(
                    " · finished {}",
                    threadlane_protocol::automation::display_time(at, "UTC")
                ))
                .unwrap_or_default()
        );
        let session = self
            .client
            .projects
            .iter()
            .flat_map(|project| &project.sessions)
            .find(|session| session.id == run.session_id)
            .cloned();
        div()
            .border_1()
            .border_color(if needs_you {
                cx.theme().warning
            } else {
                cx.theme().border
            })
            .rounded_md()
            .p_3()
            .flex()
            .flex_col()
            .gap_2()
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(icon(glyph).xsmall().text_color(color))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .flex_col()
                            .child(
                                div().text_sm().font_bold().truncate().child(
                                    if run.definition.name.trim().is_empty() {
                                        run.definition.id.clone()
                                    } else {
                                        run.definition.name.clone()
                                    },
                                ),
                            )
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(cx.theme().muted_foreground)
                                    .child(format!("{} · {}", run.status.label(), times)),
                            ),
                    )
                    .when(needs_you, |this| {
                        this.child(Tag::new().small().child("Needs you"))
                    })
                    .when(
                        !run.reviewed
                            && matches!(
                                run.status,
                                RunStatus::Succeeded
                                    | RunStatus::Failed
                                    | RunStatus::Interrupted
                            ),
                        |this| this.child(Tag::new().small().child("Unreviewed")),
                    ),
            )
            .when_some(run.error.clone(), |this, error| {
                this.child(
                    div()
                        .text_sm()
                        .text_color(cx.theme().danger)
                        .child(error),
                )
            })
            .child(
                div()
                    .flex()
                    .gap_2()
                    .flex_wrap()
                    .when(active, |this| {
                        this.child(
                            Button::new(format!("auto-run-cancel-{}", run.id))
                                .ghost()
                                .small()
                                .h_10()
                                .icon(icon(kit_icons::Ban.1))
                                .label("Cancel")
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.send_automation(AutomationCommand::Cancel {
                                        id: cancel_id.clone(),
                                    });
                                    cx.notify();
                                })),
                        )
                    })
                    .when_some(session, |this, session| {
                        this.child(
                            Button::new(format!("auto-run-chat-{}", run.id))
                                .outline()
                                .small()
                                .h_10()
                                .icon(icon(kit_icons::MessagesSquare.1))
                                .label("Open chat")
                                .on_click(cx.listener(move |this, _, window, cx| {
                                    this.open_session(&session, window, cx)
                                })),
                        )
                    })
                    .when(
                        !run.reviewed
                            && matches!(
                                run.status,
                                RunStatus::Succeeded
                                    | RunStatus::Failed
                                    | RunStatus::Interrupted
                            ),
                        |this| {
                            this.child(
                                Button::new(format!("auto-run-review-{}", run.id))
                                    .ghost()
                                    .small()
                                    .h_10()
                                    .icon(icon(kit_icons::Eye.1))
                                    .label("Mark reviewed")
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.send_automation(AutomationCommand::Review {
                                            id: review_id.clone(),
                                        });
                                        cx.notify();
                                    })),
                            )
                        },
                    )
                    .child(
                        Button::new(format!("auto-run-delete-{}", run.id))
                            .ghost()
                            .small()
                            .h_10()
                            .icon(icon(kit_icons::Trash.1))
                            .accessibility_label("Delete run")
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.auto_run_delete_confirm = Some(delete_id.clone());
                                cx.notify();
                            })),
                    ),
            )
    }

    /// Automation editor overlay — name, prompt, model/effort, a small
    /// schedule preset set, and the worktree/enabled toggles.
    fn render_auto_form(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let editing = self.auto_editing.clone();
        let is_new = editing.as_ref().is_some_and(|def| def.revision == 0);
        let schedule = self.auto_schedule;
        let entity = cx.entity();
        let kinds = [AutoSchedule::Manual, AutoSchedule::Interval, AutoSchedule::DailyUtc];
        let project_label = editing
            .as_ref()
            .and_then(|def| def.project.file_name())
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| "No project".into());
        div()
            .id("auto-form")
            .absolute()
            .inset_0()
            .flex()
            .flex_col()
            .bg(cx.theme().background)
            .text_color(cx.theme().foreground)
            .child(
                div()
                    .flex_none()
                    .flex()
                    .items_center()
                    .gap_2()
                    .px_3()
                    .py_3()
                    .border_b_1()
                    .border_color(cx.theme().border)
                    .child(
                        Button::new("auto-form-close")
                            .ghost()
                            .h_11()
                            .icon(icon(kit_icons::X.1))
                            .accessibility_label("Close automation form")
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.auto_editing = None;
                                cx.notify();
                            })),
                    )
                    .child(
                        div()
                            .flex_1()
                            .font_bold()
                            .child(if is_new { "New automation" } else { "Edit automation" }),
                    ),
            )
            .child(
                div()
                    .id("auto-form-body")
                    .flex_1()
                    .min_h_0()
                    .w_full()
                    .overflow_y_scroll()
                    .child(
                        div()
                            .px_4()
                            .py_3()
                            .flex()
                            .flex_col()
                            .gap_3()
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(cx.theme().muted_foreground)
                                    .child(format!("Project: {project_label}")),
                            )
                            .child(Input::new(&self.auto_name).aria_label("Automation name").h_11())
                            .child(Textarea::new(&self.auto_prompt).aria_label("Automation prompt"))
                            .child(
                                div().flex().gap_2().children([
                                    div().flex_1().min_w_0().child(
                                        Input::new(&self.auto_model).aria_label("Model").h_11(),
                                    ),
                                    div().flex_1().min_w_0().child(
                                        Input::new(&self.auto_effort).aria_label("Effort").h_11(),
                                    ),
                                ]),
                            )
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap_2()
                                    .child(
                                        Button::new("auto-schedule-menu")
                                            .outline()
                                            .h_11()
                                            .label(format!("Schedule: {} ▾", schedule.label()))
                                            .accessibility_label("Schedule")
                                            .dropdown_caret(true)
                                            .dropdown_menu_with_anchor(
                                                Anchor::BottomLeft,
                                                move |menu, _, _| {
                                                    kinds.iter().fold(menu, |menu, kind| {
                                                        let entity = entity.clone();
                                                        let kind = *kind;
                                                        menu.item(
                                                            PopupMenuItem::new(
                                                                kind.label().to_string(),
                                                            )
                                                            .checked(kind == schedule)
                                                            .on_click(move |_, _, cx| {
                                                                let _ = entity.update(
                                                                    cx,
                                                                    |this, cx| {
                                                                        this.auto_schedule = kind;
                                                                        cx.notify();
                                                                    },
                                                                );
                                                            }),
                                                        )
                                                    })
                                                },
                                            ),
                                    )
                                    .when(schedule == AutoSchedule::Interval, |this| {
                                        this.child(
                                            div().w_24().child(
                                                Input::new(&self.auto_minutes)
                                                    .aria_label("Minutes between runs")
                                                    .h_11(),
                                            ),
                                        )
                                    }),
                            )
                            .child(
                                div()
                                    .flex()
                                    .gap_2()
                                    .child(
                                        Button::new("auto-worktree-toggle")
                                            .outline()
                                            .h_11()
                                            .flex_1()
                                            .icon(icon(if self.auto_worktree {
                                                kit_icons::Check.1
                                            } else {
                                                kit_icons::Minus.1
                                            }))
                                            .label("Run in worktree")
                                            .on_click(cx.listener(|this, _, _, cx| {
                                                this.auto_worktree = !this.auto_worktree;
                                                cx.notify();
                                            })),
                                    )
                                    .child(
                                        Button::new("auto-enabled-toggle")
                                            .outline()
                                            .h_11()
                                            .flex_1()
                                            .icon(icon(if self.auto_enabled {
                                                kit_icons::Check.1
                                            } else {
                                                kit_icons::Minus.1
                                            }))
                                            .label("Enabled")
                                            .on_click(cx.listener(|this, _, _, cx| {
                                                this.auto_enabled = !this.auto_enabled;
                                                cx.notify();
                                            })),
                                    ),
                            ),
                    ),
            )
            .child(
                div()
                    .flex_none()
                    .border_t_1()
                    .border_color(cx.theme().border)
                    .p_3()
                    .flex()
                    .gap_2()
                    .child(
                        Button::new("auto-form-save")
                            .primary()
                            .h_11()
                            .flex_1()
                            .label(if is_new { "Create automation" } else { "Save" })
                            .on_click(cx.listener(|this, _, _, cx| this.save_auto_form(cx))),
                    ),
            )
    }
}

fn git_status_color(status: char, cx: &App) -> Hsla {
    match status {
        'A' | '?' => cx.theme().success,
        'M' => cx.theme().warning,
        'D' => cx.theme().danger,
        'R' | 'C' => cx.theme().info,
        _ => cx.theme().muted_foreground,
    }
}

impl Render for MobileApp {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        match self.screen {
            Screen::Connect => self.render_connect(cx).into_any_element(),
            Screen::Main => self.render_main(window, cx).into_any_element(),
        }
    }
}
