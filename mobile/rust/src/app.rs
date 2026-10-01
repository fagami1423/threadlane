//! GPUI views for the Threadlane mobile client.
//!
//! Three screens in one view — connect (pairing entry), session list,
//! and the live transcript with a composer. All daemon traffic flows through
//! [`crate::client::MobileDaemon`]; the view pumps its event stream on
//! the GPUI executor and keeps a flat projection of the wire types.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;

use gpui::{prelude::*, *};
use gpui_kit::component::StyledExt;
use gpui_kit::component::{
    button::{Button, ButtonVariants},
    input::{Input, InputEvent, InputState},
    marker::{Marker, MarkerContent, MarkerLoadingStyle},
    text::TextView,
    ActiveTheme, Disableable, Icon, Sizable,
};
use gpui_kit_assets::__private as kit_icons;

use threadlane_protocol::daemon::{
    ChatMessageInfo, MessageRole, PermissionDecision, ProjectInfo, SessionCommand, SessionEvent,
    SessionHealth, SessionInfo,
};
use threadlane_protocol::events::AgentEvent;
use threadlane_protocol::interaction::{
    PermissionRequest, PermissionScope, QuestionAnswer, QuestionItemAnswer, QuestionRequest,
};
use threadlane_protocol::messages::ReasoningEffort;

use crate::client::{MobileDaemon, MobileEvent};
use crate::preferences;

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

const PREF_HOST: &str = "threadlane.pair.host";
const PREF_PORT: &str = "threadlane.pair.port";
const PREF_TOKEN: &str = "threadlane.pair.token";

fn restore_pairing() -> Option<(String, String, String)> {
    match (
        preferences::get_string(PREF_HOST),
        preferences::get_string(PREF_PORT),
    ) {
        (Some(host), Some(port)) if !host.is_empty() && !port.is_empty() => {
            Some((
                host,
                port,
                preferences::get_string(PREF_TOKEN).unwrap_or_default(),
            ))
        }
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

/// Whether the transcript is scrolled to (within a few px of) the bottom —
/// only then do incoming messages drag the view down.
fn scroll_pinned_bottom(handle: &ScrollHandle) -> bool {
    let max = handle.max_offset().y;
    max <= px(1.) || handle.offset().y + max <= px(32.)
}

#[derive(Clone, Copy, PartialEq)]
enum Screen {
    Connect,
    Sessions,
    Session,
}

/// The session whose transcript the view is watching.
struct ActiveSession {
    id: String,
    title: String,
    /// `SessionInfo::runtime_work_dir` — the execution dir `SubmitPrompt` wants.
    work_dir: std::path::PathBuf,
    /// Needed by `DeleteSession` to archive the transcript.
    session_file: std::path::PathBuf,
    messages: Vec<ChatMessageInfo>,
    permission: Option<PermissionRequest>,
    /// Queued question requests; the front entry is rendered.
    questions: Vec<QuestionRequest>,
    /// Toggled options per question item id, for the front request.
    answers: HashMap<String, Vec<String>>,
    working: bool,
    status: Option<String>,
    scroll: ScrollHandle,
    confirm_delete: bool,
}

impl ActiveSession {
    fn new(info: &SessionInfo) -> Self {
        Self {
            id: info.id.clone(),
            title: if info.title.trim().is_empty() {
                "Untitled session".to_string()
            } else {
                info.title.clone()
            },
            work_dir: info.runtime_work_dir.clone(),
            session_file: info.session_file.clone(),
            messages: Vec::new(),
            permission: None,
            questions: Vec::new(),
            answers: HashMap::new(),
            working: matches!(info.health, SessionHealth::Working),
            status: None,
            scroll: ScrollHandle::new(),
            confirm_delete: false,
        }
    }

    fn pop_question(&mut self) {
        if !self.questions.is_empty() {
            self.questions.remove(0);
        }
        self.answers.clear();
    }
}

/// Root view. One `MobileDaemon` drives all traffic; `screen` picks the
/// layout and `projects`/`active` hold the rendered projection.
pub struct MobileApp {
    screen: Screen,
    host: Entity<InputState>,
    port: Entity<InputState>,
    token: Entity<InputState>,
    composer: Entity<InputState>,
    /// Shared input for a question's custom answer — requests with several
    /// `allow_custom` items are rare, so one input under the first such item.
    question_custom: Entity<InputState>,
    connect_error: Option<String>,
    /// Whether a persisted pairing exists — drives the Forget button.
    saved_pairing: bool,
    daemon: Option<MobileDaemon>,
    /// Human-readable link state shown in the sessions header.
    link_state: String,
    /// Deep links already consumed, so reconnect flows don't re-apply them.
    seen_links: Vec<String>,
    projects: Vec<ProjectInfo>,
    active: Option<ActiveSession>,
    /// Pending permission requests keyed by session id — the wire has no
    /// pending-permission field in `SessionSnapshot`, so requests that
    /// arrive while another screen is shown must be retained here.
    pending_permissions: HashMap<String, PermissionRequest>,
    /// Same retention for question requests, which the snapshot also lacks.
    pending_questions: HashMap<String, Vec<QuestionRequest>>,
    sessions_scroll: ScrollHandle,
    _link_task: Task<()>,
    _pump: Option<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

impl MobileApp {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let host = cx.new(|cx| InputState::new(window, cx).placeholder("192.168.x.x"));
        let port = cx.new(|cx| InputState::new(window, cx).placeholder("port"));
        let token = cx.new(|cx| InputState::new(window, cx).placeholder("pairing token"));
        let composer = cx.new(|cx| InputState::new(window, cx).placeholder("Message"));
        let question_custom = cx.new(|cx| InputState::new(window, cx).placeholder("Custom answer"));
        let mut subscriptions = [&host, &port, &token, &question_custom]
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
            question_custom,
            connect_error: None,
            saved_pairing,
            daemon: None,
            link_state: "Disconnected".to_string(),
            seen_links: Vec::new(),
            projects: Vec::new(),
            active: None,
            pending_permissions: HashMap::new(),
            pending_questions: HashMap::new(),
            sessions_scroll: ScrollHandle::new(),
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
            app.connect_now(cx);
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
        self.connect_now(cx);
    }

    fn connect_now(&mut self, cx: &mut Context<Self>) {
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
        let daemon = MobileDaemon::connect(url, token);
        self.link_state = "Connecting…".to_string();
        self.connect_error = None;
        self.projects.clear();
        self.active = None;
        self.daemon = Some(daemon);
        self.screen = Screen::Sessions;

        // Pump the wire onto the view until the daemon is dropped.
        let mut events = self.daemon.as_mut().and_then(|daemon| daemon.take_events());
        self._pump = Some(cx.spawn(async move |this, cx| {
            let Some(events) = events.as_mut() else {
                return;
            };
            while let Some(event) = events.recv().await {
                if this
                    .update(cx, |this, cx| this.apply_event(event, cx))
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
        self.projects.clear();
        self.active = None;
        self.pending_permissions.clear();
        self.pending_questions.clear();
        self.link_state = "Disconnected".to_string();
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

    fn open_session(&mut self, info: &SessionInfo, cx: &mut Context<Self>) {
        if let Some(daemon) = &self.daemon {
            daemon.send(SessionCommand::GetSessionSnapshot {
                session_id: info.id.clone(),
            });
        }
        let mut active = ActiveSession::new(info);
        // Recover permission/question requests that arrived before the
        // session was opened — snapshots do not carry them.
        active.permission = self.pending_permissions.remove(&info.id);
        if let Some(questions) = self.pending_questions.remove(&info.id) {
            active.questions = questions;
        }
        self.active = Some(active);
        self.screen = Screen::Session;
        cx.notify();
    }

    fn apply_event(&mut self, event: MobileEvent, cx: &mut Context<Self>) {
        match event {
            MobileEvent::Connected => {
                self.link_state = "Live".to_string();
                self.connect_error = None;
                // Remember the pairing so the next launch reconnects.
                let host = self.host.read(cx).value().trim().to_owned();
                let port = self.port.read(cx).value().trim().to_owned();
                let token = self.token.read(cx).value().trim().to_owned();
                store_pairing(&host, &port, &token);
                self.saved_pairing = true;
            }
            MobileEvent::Reconnecting => {
                self.link_state = "Reconnecting…".to_string();
            }
            MobileEvent::Fatal(error) => {
                self.link_state = "Failed".to_string();
                self.connect_error = Some(error);
                self.screen = Screen::Connect;
            }
            MobileEvent::Event(event) => self.apply_session_event(event, cx),
        }
        cx.notify();
    }

    fn apply_session_event(&mut self, event: SessionEvent, cx: &mut Context<Self>) {
        match event {
            SessionEvent::ProjectChanged { project } => {
                if project.is_expanded {
                    match self
                        .projects
                        .iter_mut()
                        .find(|entry| entry.work_dir == project.work_dir)
                    {
                        Some(entry) => *entry = project,
                        None => self.projects.push(project),
                    }
                } else {
                    self.projects
                        .retain(|entry| entry.work_dir != project.work_dir);
                }
            }
            SessionEvent::SessionSnapshot {
                session_id,
                snapshot,
            } => {
                if let Some(active) = self.active.as_mut().filter(|a| a.id == session_id) {
                    let pinned = scroll_pinned_bottom(&active.scroll);
                    active.title = if snapshot.session.title.trim().is_empty() {
                        active.title.clone()
                    } else {
                        snapshot.session.title.clone()
                    };
                    active.messages = snapshot.messages;
                    active.status = None;
                    if pinned {
                        active.scroll.scroll_to_bottom();
                    }
                }
            }
            SessionEvent::Agent { session_id, event } => {
                // Requests for a session that is not open are stashed — they
                // badge its row and are recovered when it is opened.
                let is_open = self
                    .active
                    .as_ref()
                    .is_some_and(|active| active.id == session_id);
                if !is_open {
                    if let AgentEvent::PermissionRequested { request } = &event {
                        self.pending_permissions
                            .insert(session_id.clone(), request.clone());
                    }
                    if let AgentEvent::QuestionRequested { request } = &event {
                        self.pending_questions
                            .entry(session_id.clone())
                            .or_default()
                            .push(request.clone());
                    }
                }
                if let Some(active) = self.active.as_mut().filter(|a| a.id == session_id) {
                    let pinned = scroll_pinned_bottom(&active.scroll);
                    active.apply_agent_event(&event);
                    if pinned {
                        active.scroll.scroll_to_bottom();
                    }
                }
            }
            SessionEvent::Finished { session_id, .. } => {
                if let Some(active) = self.active.as_mut().filter(|a| a.id == session_id) {
                    active.working = false;
                    active.status = None;
                    for message in active.messages.iter_mut() {
                        message.streaming = false;
                    }
                }
                // Snapshot refresh pulls in the finalized transcript and
                // updated title/health.
                if let Some(daemon) = &self.daemon {
                    daemon.send(SessionCommand::GetSessionSnapshot { session_id });
                }
            }
            SessionEvent::TitleGenerated { session_id, .. } => {
                if let Some(daemon) = &self.daemon {
                    daemon.send(SessionCommand::GetSessionSnapshot { session_id });
                }
            }
            SessionEvent::SessionRemoved { session_id, .. } => {
                self.pending_permissions.remove(&session_id);
                self.pending_questions.remove(&session_id);
                for project in self.projects.iter_mut() {
                    project.sessions.retain(|session| session.id != session_id);
                }
                if self.active.as_ref().is_some_and(|a| a.id == session_id) {
                    self.active = None;
                    self.screen = Screen::Sessions;
                }
            }
            SessionEvent::DaemonError {
                session_id,
                message,
            } => {
                if let Some(active) = self
                    .active
                    .as_mut()
                    .filter(|a| Some(a.id.as_str()) == session_id.as_deref())
                {
                    active.status = Some(message);
                } else {
                    self.connect_error = Some(message);
                }
            }
            _ => {}
        }
        cx.notify();
    }

    /// Send the composer text: `SteerMessage` mid-run so it reaches the
    /// model during the current turn, `SubmitPrompt` otherwise.
    fn submit_composer(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let text = self.composer.read(cx).value().trim().to_owned();
        if text.is_empty() {
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
            active.status = Some("Not connected — message was not sent".to_string());
            cx.notify();
            return;
        }
        let session_id = active.id.clone();
        let command = if active.working {
            SessionCommand::SteerMessage {
                session_id,
                text: text.clone(),
                images: Vec::new(),
            }
        } else {
            SessionCommand::SubmitPrompt {
                session_id,
                work_dir: active.work_dir.clone(),
                text: text.clone(),
                images: Vec::new(),
                effort: ReasoningEffort::default(),
                acp_config: Vec::new(),
                model: None,
            }
        };
        if let Some(daemon) = &self.daemon {
            daemon.send(command);
        }
        // Optimistic echo — the daemon does not replay user prompts; the
        // next `SessionSnapshot` replaces it with the persisted row.
        active.messages.push(ChatMessageInfo {
            id: format!("queued-user-{}-{}", active.id, active.messages.len()),
            role: MessageRole::User,
            content: text,
            tool_activities: Vec::new(),
            streaming: false,
            reasoning_content: None,
            reasoning_expanded: false,
        });
        self.composer
            .update(cx, |input, cx| input.set_value("", window, cx));
        active.scroll.scroll_to_bottom();
        cx.notify();
    }

    fn answer_permission(&mut self, decision: PermissionDecision, cx: &mut Context<Self>) {
        let Some(active) = &mut self.active else {
            return;
        };
        let Some(request) = active.permission.clone() else {
            return;
        };
        // Commands queued while the socket is down are drained without
        // delivery, so keep the prompt unless a live socket can take it.
        let connected = self
            .daemon
            .as_ref()
            .is_some_and(|daemon| daemon.is_connected());
        if !connected {
            active.status = Some("Not connected — answer was not sent".to_string());
            cx.notify();
            return;
        }
        active.permission = None;
        self.pending_permissions.remove(&active.id);
        if let Some(daemon) = &self.daemon {
            daemon.send(SessionCommand::AnswerPermission {
                session_id: active.id.clone(),
                request_id: request.id,
                decision,
            });
        }
        cx.notify();
    }

    fn toggle_question_option(&mut self, item_id: &str, option: &str, cx: &mut Context<Self>) {
        let Some(active) = &mut self.active else {
            return;
        };
        let selected = active.answers.entry(item_id.to_string()).or_default();
        if let Some(position) = selected.iter().position(|picked| picked == option) {
            selected.remove(position);
        } else {
            selected.push(option.to_string());
        }
        cx.notify();
    }

    fn answer_question(&mut self, dismiss: bool, window: &mut Window, cx: &mut Context<Self>) {
        let Some(active) = &mut self.active else {
            return;
        };
        let Some(request) = active.questions.first().cloned() else {
            return;
        };
        let connected = self
            .daemon
            .as_ref()
            .is_some_and(|daemon| daemon.is_connected());
        if !connected {
            active.status = Some("Not connected — answer was not sent".to_string());
            cx.notify();
            return;
        }
        let custom = self.question_custom.read(cx).value().trim().to_owned();
        let mut custom_used = false;
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
                        let custom_text = if item.allow_custom && !custom.is_empty() && !custom_used
                        {
                            custom_used = true;
                            Some(custom.clone())
                        } else {
                            None
                        };
                        QuestionItemAnswer {
                            question_id: item.id.clone(),
                            selected: active.answers.get(&item.id).cloned().unwrap_or_default(),
                            custom_text,
                        }
                    })
                    .collect(),
            }
        };
        active.pop_question();
        self.question_custom
            .update(cx, |input, cx| input.set_value("", window, cx));
        if let Some(daemon) = &self.daemon {
            daemon.send(SessionCommand::AnswerQuestion {
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
            active.status = Some("Not connected".to_string());
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
        active.status = Some("Archiving session…".to_string());
        cx.notify();
    }
}

impl ActiveSession {
    /// Fold one agent event into the rendered transcript. Streaming text
    /// lands on a synthetic trailing message; `MessageEnd` finalizes it.
    fn apply_agent_event(&mut self, event: &AgentEvent) {
        match event {
            AgentEvent::AgentStart => self.working = true,
            AgentEvent::AgentEnd { .. } => self.working = false,
            AgentEvent::MessageStart { role } => {
                let role = match role.as_str() {
                    "user" => MessageRole::User,
                    "system" => MessageRole::System,
                    "error" => MessageRole::Error,
                    _ => MessageRole::Assistant,
                };
                self.messages.push(ChatMessageInfo {
                    id: format!("live-{}", self.messages.len()),
                    role,
                    content: String::new(),
                    tool_activities: Vec::new(),
                    streaming: true,
                    reasoning_content: None,
                    reasoning_expanded: false,
                });
            }
            AgentEvent::MessageUpdate {
                text_delta,
                reasoning_delta,
                tool_call_name,
            } => {
                let message = self
                    .messages
                    .iter_mut()
                    .rev()
                    .find(|message| message.streaming);
                if let Some(message) = message {
                    if let Some(delta) = text_delta {
                        message.content.push_str(delta);
                    }
                    if let Some(delta) = reasoning_delta {
                        let reasoning = message.reasoning_content.get_or_insert_with(String::new);
                        reasoning.push_str(delta);
                    }
                    if let Some(name) = tool_call_name {
                        message.tool_activities.push(
                            threadlane_protocol::daemon::ToolActivityInfo {
                                id: format!("tool-{}", message.tool_activities.len()),
                                category: "tool".to_string(),
                                title: name.clone(),
                                display_summary: String::new(),
                                detail: String::new(),
                                is_expanded: false,
                            },
                        );
                    }
                }
            }
            AgentEvent::MessageEnd { .. } => {
                if let Some(message) = self
                    .messages
                    .iter_mut()
                    .rev()
                    .find(|message| message.streaming)
                {
                    message.streaming = false;
                }
            }
            AgentEvent::ToolExecutionStart { name, .. } => {
                self.status = Some(format!("Running {name}…"));
            }
            AgentEvent::ToolExecutionEnd { name, .. } => {
                if self.status.as_deref() == Some(format!("Running {name}…").as_str()) {
                    self.status = None;
                }
            }
            AgentEvent::PermissionRequested { request } => {
                self.permission = Some(request.clone());
            }
            AgentEvent::QuestionRequested { request } => {
                self.questions.push(request.clone());
            }
            AgentEvent::AgentError { error } => {
                self.status = Some(error.clone());
            }
            AgentEvent::FusionUpdate { message, .. } => {
                self.status = Some(message.clone());
            }
            _ => {}
        }
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
        let mut header = div()
            .flex()
            .flex_col()
            .gap_2()
            .pt_8()
            .child(div().text_xl().font_bold().child("Threadlane"));
        if let Some(logo) = threadlane_ui_theme::bundled_icon("icons/threadlane.svg") {
            header = header.child(logo.with_size(px(44.)).text_color(cx.theme().accent));
        }
        header = header.child(
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
            .size_full()
            .flex()
            .flex_col()
            .bg(cx.theme().background)
            .text_color(cx.theme().foreground)
            .p_4()
            .gap_4()
            .child(header)
            .child(
                div()
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
                            .child(Input::new(&self.host)),
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
                            .child(Input::new(&self.port)),
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
                            .child(Input::new(&self.token)),
                    ),
            )
            .when_some(self.connect_error.clone(), |this, error| {
                this.child(div().text_sm().text_color(cx.theme().danger).child(error))
            })
            .child(
                Button::new("connect")
                    .primary()
                    .label("Connect")
                    .w_full()
                    .disabled(self.link_state == "Connecting…")
                    .on_click(cx.listener(|this, _, _, cx| this.connect_now(cx))),
            )
            .when(self.saved_pairing, |this| {
                this.child(
                    Button::new("forget-pairing")
                        .ghost()
                        .small()
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
                    .justify_between()
                    .px_4()
                    .py_3()
                    .border_b_1()
                    .border_color(cx.theme().border)
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap_2()
                                    .child(div().font_bold().child("Sessions"))
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
                        Button::new("disconnect")
                            .ghost()
                            .label("Disconnect")
                            .small()
                            .on_click(cx.listener(|this, _, _, cx| this.disconnect(cx))),
                    ),
            )
            .child(
                div()
                    .id("sessions")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .track_scroll(&self.sessions_scroll)
                    .child(
                        div()
                            .p_3()
                            .flex()
                            .flex_col()
                            .gap_4()
                            .when(self.projects.is_empty(), |this| {
                                this.child(
                                    div()
                                        .py_8()
                                        .text_sm()
                                        .text_color(cx.theme().muted_foreground)
                                        .child(if self.link_state == "Live" {
                                            "No projects attached on the desktop yet."
                                        } else {
                                            "Connecting to the desktop…"
                                        }),
                                )
                            })
                            .children(self.projects.iter().map(|project| {
                                div()
                                    .flex()
                                    .flex_col()
                                    .gap_2()
                                    .child(
                                        div()
                                            .text_sm()
                                            .font_bold()
                                            .text_color(cx.theme().muted_foreground)
                                            .child(project.name.clone()),
                                    )
                                    .children(project.sessions.iter().map(|session| {
                                        let info = session.clone();
                                        // Key rows by session id — a
                                        // per-project index collides across
                                        // projects and shifts on reorder.
                                        let needs_you =
                                            self.pending_permissions.contains_key(&session.id)
                                                || self.pending_questions.contains_key(&session.id);
                                        let working =
                                            matches!(session.health, SessionHealth::Working);
                                        Button::new(format!("session-{}", session.id))
                                            .outline()
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
                                                            .child(div().child(
                                                                if session.title.trim().is_empty() {
                                                                    "Untitled session".to_string()
                                                                } else {
                                                                    session.title.clone()
                                                                },
                                                            ))
                                                            .when(needs_you, |this| {
                                                                this.child(
                                                                    div()
                                                                        .flex()
                                                                        .items_center()
                                                                        .gap_1()
                                                                        .text_xs()
                                                                        .text_color(
                                                                            cx.theme().warning,
                                                                        )
                                                                        .child(
                                                                            icon(
                                                                                kit_icons::BellDot
                                                                                    .1,
                                                                            )
                                                                            .xsmall()
                                                                            .text_color(
                                                                                cx.theme().warning,
                                                                            ),
                                                                        )
                                                                        .child("Needs you"),
                                                                )
                                                            })
                                                            .when(working && !needs_you, |this| {
                                                                this.child(
                                                                    div()
                                                                        .text_xs()
                                                                        .text_color(
                                                                            cx.theme().accent,
                                                                        )
                                                                        .child("Working"),
                                                                )
                                                            }),
                                                    )
                                                    .child(
                                                        div()
                                                            .text_xs()
                                                            .text_color(cx.theme().muted_foreground)
                                                            .child(format!(
                                                                "{}{}",
                                                                session.runtime_work_dir.display(),
                                                                if session.is_worktree {
                                                                    " · worktree"
                                                                } else {
                                                                    ""
                                                                },
                                                            )),
                                                    ),
                                            )
                                            .on_click(cx.listener(move |this, _, _, cx| {
                                                this.open_session(&info, cx);
                                            }))
                                    }))
                            })),
                    ),
            )
    }

    fn render_session(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let Some(active) = &self.active else {
            return div().size_full().bg(cx.theme().background);
        };
        let title = active.title.clone();
        let working = active.working;
        let status = active.status.clone();
        let messages = active.messages.clone();
        let permission = active.permission.clone();
        let question = active.questions.first().cloned();
        let confirm_delete = active.confirm_delete;
        let composer_empty = self.composer.read(cx).value().trim().is_empty();
        let scroll = active.scroll.clone();

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
                            .small()
                            .icon(icon(kit_icons::ArrowLeft.1))
                            .label("Back")
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.active = None;
                                this.screen = Screen::Sessions;
                                cx.notify();
                            })),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .child(div().font_bold().child(title))
                            .when_some(status, |this, status| {
                                this.child(
                                    div()
                                        .text_xs()
                                        .text_color(cx.theme().muted_foreground)
                                        .child(status),
                                )
                            }),
                    )
                    .when(working, |this| {
                        this.child(
                            Button::new("cancel-run")
                                .danger()
                                .small()
                                .label("Stop")
                                .on_click(cx.listener(|this, _, _, cx| this.cancel_run(cx))),
                        )
                    })
                    .child(
                        Button::new("delete-session")
                            .ghost()
                            .small()
                            .icon(icon(kit_icons::Trash.1).text_color(cx.theme().muted_foreground))
                            .accessibility_label("Archive session")
                            .on_click(cx.listener(|this, _, _, cx| {
                                if let Some(active) = &mut this.active {
                                    active.confirm_delete = !active.confirm_delete;
                                }
                                cx.notify();
                            })),
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
            .child(
                div()
                    .id("transcript")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .track_scroll(&scroll)
                    .child(
                        div()
                            .p_3()
                            .flex()
                            .flex_col()
                            .gap_4()
                            .when(messages.is_empty(), |this| {
                                this.child(
                                    div()
                                        .py_8()
                                        .text_sm()
                                        .text_color(cx.theme().muted_foreground)
                                        .child("No messages yet."),
                                )
                            })
                            .children(
                                messages
                                    .iter()
                                    .map(|message| self.render_message(message, cx)),
                            )
                            .when(working, |this| {
                                this.child(
                                    Marker::new()
                                        .id("session-working")
                                        .role(Role::Status)
                                        .loading(true)
                                        .with_loading_style(MarkerLoadingStyle::Shimmer)
                                        .content(MarkerContent::new().text("Working…")),
                                )
                            }),
                    ),
            )
            .when_some(permission, |this, permission| {
                this.child(self.render_permission(&permission, cx))
            })
            .when_some(question, |this, question| {
                this.child(self.render_question(&question, cx))
            })
            .child(
                div()
                    .flex_none()
                    .flex()
                    .items_center()
                    .gap_2()
                    .px_3()
                    .py_2()
                    .border_t_1()
                    .border_color(cx.theme().border)
                    .child(div().flex_1().min_w_0().child(Input::new(&self.composer)))
                    .child(
                        Button::new("send")
                            .primary()
                            .icon(icon(kit_icons::SendHorizontal.1))
                            .accessibility_label(if working {
                                "Steer the running turn"
                            } else {
                                "Send message"
                            })
                            .disabled(composer_empty)
                            .on_click(
                                cx.listener(|this, _, window, cx| this.submit_composer(window, cx)),
                            ),
                    ),
            )
    }

    fn render_message(&self, message: &ChatMessageInfo, cx: &Context<Self>) -> AnyElement {
        let is_user = message.role == MessageRole::User;
        let body = div()
            .text_sm()
            .line_height(relative(1.5))
            .when(!message.content.is_empty(), |this| {
                if is_user {
                    this.child(message.content.clone())
                } else {
                    // Assistant/system text is markdown — same renderer as
                    // the desktop chat surface.
                    this.child(
                        TextView::markdown(format!("msg-{}", message.id), message.content.clone())
                            .selectable(true),
                    )
                }
            })
            .when(message.streaming && message.content.is_empty(), |this| {
                this.text_color(cx.theme().muted_foreground).child("…")
            })
            .children(message.reasoning_content.as_ref().map(|reasoning| {
                div()
                    .mt_2()
                    .pt_2()
                    .border_t_1()
                    .border_color(cx.theme().border)
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child(reasoning.clone())
            }))
            .children(message.tool_activities.iter().map(|tool| {
                div()
                    .mt_1()
                    .flex()
                    .items_center()
                    .gap_1()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child(
                        icon(kit_icons::Wrench.1)
                            .xsmall()
                            .text_color(cx.theme().muted_foreground),
                    )
                    .child(tool.title.clone())
            }));
        if is_user {
            div()
                .w_full()
                .flex()
                .justify_end()
                .child(
                    div()
                        .min_w_0()
                        .max_w(rems(40.))
                        .px_4()
                        .py_3()
                        .rounded_2xl()
                        .rounded_br_md()
                        .border_1()
                        .border_color(cx.theme().border.opacity(0.22))
                        .bg(cx.theme().secondary.opacity(0.85))
                        .text_sm()
                        .text_color(cx.theme().secondary_foreground)
                        .child(body),
                )
                .into_any_element()
        } else {
            div().w_full().child(body).into_any_element()
        }
    }

    fn render_permission(
        &self,
        request: &PermissionRequest,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let mut allow_buttons = Vec::new();
        for (index, scope) in request.scopes.iter().enumerate() {
            let (label, decision) = match scope {
                PermissionScope::Once => ("Allow once", PermissionDecision::AllowOnce),
                PermissionScope::Session => ("Allow session", PermissionDecision::AllowSession),
                PermissionScope::Always => ("Allow always", PermissionDecision::AllowAlways),
            };
            allow_buttons.push(
                Button::new(("permission-allow", index))
                    .primary()
                    .small()
                    .label(label)
                    .on_click(
                        cx.listener(move |this, _, _, cx| this.answer_permission(decision, cx)),
                    ),
            );
        }
        div()
            .flex_none()
            .p_3()
            .border_t_1()
            .border_color(cx.theme().border)
            .bg(cx.theme().muted.opacity(0.3))
            .flex()
            .flex_col()
            .gap_2()
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(
                        icon(kit_icons::ShieldAlert.1)
                            .xsmall()
                            .text_color(cx.theme().warning),
                    )
                    .child(
                        div()
                            .text_sm()
                            .font_bold()
                            .child(format!("{}: {}", request.capability, request.title)),
                    ),
            )
            .when(!request.detail.is_empty(), |this| {
                this.child(
                    div()
                        .text_xs()
                        .text_color(cx.theme().muted_foreground)
                        .child(request.detail.clone()),
                )
            })
            .child(
                div().flex().gap_2().children(allow_buttons).child(
                    Button::new("permission-deny")
                        .danger()
                        .small()
                        .label("Deny")
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.answer_permission(PermissionDecision::Deny, cx)
                        })),
                ),
            )
    }

    /// The front question request: option toggles per item, one shared
    /// custom-answer input under the first `allow_custom` item, then
    /// Submit/Dismiss.
    fn render_question(
        &self,
        request: &QuestionRequest,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let Some(active) = &self.active else {
            return div().into_any_element();
        };
        let answers = active.answers.clone();
        let mut custom_rendered = false;
        let mut items = Vec::new();
        for item in &request.questions {
            let mut card = div()
                .flex()
                .flex_col()
                .gap_2()
                .when(!item.header.is_empty(), |this| {
                    this.child(
                        div()
                            .text_xs()
                            .font_bold()
                            .text_color(cx.theme().muted_foreground)
                            .child(item.header.clone()),
                    )
                })
                .child(div().text_sm().child(item.question.clone()))
                .child(div().flex().flex_wrap().gap_2().children(
                    item.options.iter().enumerate().map(|(index, option)| {
                        let selected = answers
                            .get(&item.id)
                            .is_some_and(|picked| picked.iter().any(|o| o == option));
                        let item_id = item.id.clone();
                        let option = option.clone();
                        let mut button = Button::new(format!("qopt-{}-{index}", item.id))
                            .small()
                            .label(option.clone());
                        button = if selected {
                            button.primary()
                        } else {
                            button.outline()
                        };
                        button.on_click(cx.listener(move |this, _, _, cx| {
                            this.toggle_question_option(&item_id, &option, cx)
                        }))
                    }),
                ));
            // The shared custom-answer input renders under the first item
            // that accepts one; `answer_question` attaches its text there.
            if item.allow_custom && !custom_rendered {
                custom_rendered = true;
                card = card.child(
                    div()
                        .flex()
                        .flex_col()
                        .gap_1()
                        .child(
                            div()
                                .text_xs()
                                .text_color(cx.theme().muted_foreground)
                                .child("Or write your own"),
                        )
                        .child(Input::new(&self.question_custom)),
                );
            }
            items.push(card);
        }
        let remaining = active.questions.len() - 1;
        div()
            .flex_none()
            .p_3()
            .border_t_1()
            .border_color(cx.theme().border)
            .bg(cx.theme().muted.opacity(0.3))
            .flex()
            .flex_col()
            .gap_3()
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
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.answer_question(false, window, cx)
                            })),
                    )
                    .child(
                        Button::new("question-dismiss")
                            .ghost()
                            .small()
                            .label("Dismiss")
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.answer_question(true, window, cx)
                            })),
                    ),
            )
            .into_any_element()
    }
}

impl Render for MobileApp {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        match self.screen {
            Screen::Connect => self.render_connect(cx).into_any_element(),
            Screen::Sessions => self.render_sessions(cx).into_any_element(),
            Screen::Session => self.render_session(cx).into_any_element(),
        }
    }
}
