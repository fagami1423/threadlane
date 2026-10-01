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
use gpui_component::menu::{DropdownMenu, PopupMenuItem};
use gpui_kit::component::StyledExt;
use gpui_kit::component::{
    button::{Button, ButtonVariants},
    input::{Input, InputEvent, InputState},
    marker::{Marker, MarkerContent, MarkerLoadingStyle},
    ActiveTheme, Disableable, Icon, Sizable,
};
use gpui_kit_assets::__private as kit_icons;
use threadlane_protocol::daemon::{CommandResponse, ComposerModel};
use threadlane_protocol::repo::{GitOperation, GitResponse, GitStatus};
use threadlane_protocol::{OrchestratorMode, ReasoningEffort};

use crate::client::{MobileDaemon, MobileEvent};
use crate::preferences;
use threadlane_client::ClientState;
use threadlane_protocol::daemon::{
    ChatMessageInfo, MessageRole, PermissionDecision, SessionCommand, SessionEvent, SessionHealth,
    SessionInfo,
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
    /// Queued question requests; the front entry is rendered.
    /// Toggled options per question item id, for the front request.
    answers: HashMap<String, Vec<String>>,
    transcript: threadlane_ui_session::transcript::TranscriptState,
    confirm_delete: bool,
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
        let composer = cx.new(|cx| {
            gpui_kit::component::input::TextareaState::new(window, cx)
                .placeholder("Message")
                .auto_grow(1, 8)
                .submit_on_enter(true)
                .soft_wrap(true)
        });
        let mut subscriptions = [&host, &port, &token, &search]
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
            daemon: None,
            link_state: "Disconnected".to_string(),
            seen_links: Vec::new(),
            client: ClientState::default(),
            markdown_states: HashMap::new(),
            active: None,
            sessions_list: ListState::new(0, ListAlignment::Top, window.rem_size() * 4.5),
            session_rows: Vec::new(),
            project_git: HashMap::new(),
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
        self.daemon = Some(daemon);
        self.screen = Screen::Sessions;

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
        self.client.projects.clear();
        self.active = None;
        self.client.pending_permissions.clear();
        self.client.pending_questions.clear();
        self.client.queued_questions.clear();
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

    fn open_session(&mut self, info: &SessionInfo, window: &mut Window, cx: &mut Context<Self>) {
        if !self.session_drafts.iter().any(|draft| draft.id == info.id) {
            if let Some(daemon) = &self.daemon {
                daemon.send(SessionCommand::GetSessionSnapshot {
                    session_id: info.id.clone(),
                });
            }
        }
        if let Some(daemon) = &self.daemon {
            daemon.request(SessionCommand::GetComposerOptions {
                work_dir: info.work_dir.clone(),
                session_id: Some(info.id.clone()),
            });
        }
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
        self.screen = Screen::Session;
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
                self.screen = Screen::Session;
                if let Some(daemon) = &self.daemon {
                    daemon.request(SessionCommand::GetComposerOptions {
                        work_dir: session.work_dir,
                        session_id: Some(session.id),
                    });
                }
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
            CommandResponse::Git {
                response: GitResponse::Status { status },
            } => {
                if let SessionCommand::GitRequest { work_dir, .. } = command {
                    self.project_git.insert(work_dir.clone(), *status);
                    self.refresh_session_rows();
                }
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
            self.daemon.as_ref().is_some_and(|d| d.is_connected()) && !self.client.is_generating;
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
                                if let (Some(active), Some(daemon)) = (&this.active, &this.daemon) {
                                    daemon.request(SessionCommand::SetModel { session_id: active.id.clone(), model: id.clone() });
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
                                        if let (Some(active), Some(daemon)) = (&this.active, &this.daemon) {
                                            daemon.request(SessionCommand::SetReasoningEffort { session_id: active.id.clone(), effort: value });
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
                                    if let (Some(active), Some(daemon)) = (&this.active, &this.daemon) {
                                        daemon.request(SessionCommand::SetOrchestratorMode { session_id: active.id.clone(), mode: value });
                                    } cx.notify();
                                }); }))
                        })
                    })))
            .into_any_element()
    }

    fn apply_event(&mut self, event: MobileEvent, window: &mut Window, cx: &mut Context<Self>) {
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
                    self.sending = false;
                    if let SessionCommand::SubmitPrompt { session_id, .. } = &command {
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
                    if matches!(command, SessionCommand::GitRequest { .. }) {
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
            self.screen = Screen::Sessions;
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
            let echo_id = format!("pending-user-{}", active.id);
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
        }
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
                                    .child(div().font_bold().child("Projects & chats"))
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
                            .h_11()
                            .on_click(cx.listener(|this, _, _, cx| this.disconnect(cx))),
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
                                this.screen = Screen::Sessions;
                                cx.notify();
                            })),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .child(div().font_bold().truncate().child(title))
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
                        .child(self.composer_options(cx))
                        .child(
                            div().flex().justify_end().child(
                                Button::new("send")
                                    .primary()
                                    .size_11()
                                    .icon(icon(kit_icons::SendHorizontal.1))
                                    .accessibility_label(if working {
                                        "Queue for next turn"
                                    } else {
                                        "Send message"
                                    })
                                    .disabled(
                                        composer_empty
                                            || self.sending
                                            || !self
                                                .daemon
                                                .as_ref()
                                                .is_some_and(|d| d.is_connected()),
                                    )
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
            threadlane_ui_session::message_row(MessageRole::User)
                .child(threadlane_ui_session::user_message_bubble(cx).child(body))
                .into_any_element()
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
}

impl Render for MobileApp {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        match self.screen {
            Screen::Connect => self.render_connect(cx).into_any_element(),
            Screen::Sessions => self.render_sessions(cx).into_any_element(),
            Screen::Session => self.render_session(window, cx).into_any_element(),
        }
    }
}
