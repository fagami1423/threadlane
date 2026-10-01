//! GPUI views for the Threadlane mobile monitor.
//!
//! Three screens in one view — connect (pairing entry), session list,
//! and the live transcript. All daemon traffic flows through
//! [`crate::client::MobileDaemon`]; the view pumps its event stream on
//! the GPUI executor and keeps a flat projection of the wire types.

use std::sync::Mutex;
use std::time::Duration;

use gpui::{prelude::*, *};
use gpui_kit::component::{
    button::{Button, ButtonVariants},
    input::{Input, InputEvent, InputState},
    marker::{Marker, MarkerContent, MarkerLoadingStyle},
    ActiveTheme, Disableable, Sizable,
};
use gpui_kit::component::StyledExt;

use threadlane_protocol::daemon::{
    ChatMessageInfo, MessageRole, PermissionDecision, ProjectInfo, SessionCommand, SessionEvent,
    SessionInfo,
};
use threadlane_protocol::events::AgentEvent;
use threadlane_protocol::interaction::{PermissionRequest, PermissionScope};

use crate::client::{MobileDaemon, MobileEvent};

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
    messages: Vec<ChatMessageInfo>,
    permission: Option<PermissionRequest>,
    working: bool,
    status: Option<String>,
    scroll: ScrollHandle,
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
            messages: Vec::new(),
            permission: None,
            working: matches!(
                info.health,
                threadlane_protocol::daemon::SessionHealth::Working
            ),
            status: None,
            scroll: ScrollHandle::new(),
        }
    }
}

/// Root view. One `MobileDaemon` drives all traffic; `screen` picks the
/// layout and `projects`/`active` hold the rendered projection.
pub struct MobileApp {
    screen: Screen,
    host: Entity<InputState>,
    port: Entity<InputState>,
    token: Entity<InputState>,
    connect_error: Option<String>,
    daemon: Option<MobileDaemon>,
    /// Human-readable link state shown in the sessions header.
    link_state: String,
    /// Deep links already consumed, so reconnect flows don't re-apply them.
    seen_links: Vec<String>,
    projects: Vec<ProjectInfo>,
    active: Option<ActiveSession>,
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
        let subscriptions = [&host, &port, &token]
            .iter()
            .map(|input| {
                cx.subscribe_in(input, window, |_, _, event, _, _| match event {
                    InputEvent::Focus => gpui_mobile::show_keyboard(),
                    InputEvent::Blur => gpui_mobile::hide_keyboard(),
                    _ => {}
                })
            })
            .collect::<Vec<_>>();

        // Poll for pairing deep links arriving while the app runs — the
        // UIKit handler runs outside GPUI and can only stash them.
        let link_task = cx.spawn_in(window, async move |this, cx| {
            // A link may have launched the app before this task started.
            if let Ok(Some(url)) = gpui_mobile::packages::deeplink::get_initial_link() {
                let _ = this.update_in(cx, |this, window, cx| {
                    this.apply_pairing_link(&url, window, cx);
                });
            }
            loop {
                cx.background_executor()
                    .timer(Duration::from_millis(500))
                    .await;
                let links = take_pending_links();
                if links.is_empty() {
                    continue;
                }
                let _ = this.update_in(cx, |this, window, cx| {
                    for url in links {
                        this.apply_pairing_link(&url, window, cx);
                    }
                });
            }
        });

        Self {
            screen: Screen::Connect,
            host,
            port,
            token,
            connect_error: None,
            daemon: None,
            link_state: "Disconnected".to_string(),
            seen_links: Vec::new(),
            projects: Vec::new(),
            active: None,
            sessions_scroll: ScrollHandle::new(),
            _link_task: link_task,
            _pump: None,
            _subscriptions: subscriptions,
        }
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
            self.host.update(cx, |input, cx| input.set_value(host, window, cx));
        }
        if let Some(port) = query("port") {
            self.port.update(cx, |input, cx| input.set_value(port, window, cx));
        }
        if let Some(token) = query("token") {
            self.token.update(cx, |input, cx| input.set_value(token, window, cx));
        }
        self.connect_now(cx);
    }

    fn connect_now(&mut self, cx: &mut Context<Self>) {
        let host = self.host.read(cx).value().trim().to_owned();
        let port = self.port.read(cx).value().trim().to_owned();
        let token = self.token.read(cx).value().trim().to_owned();
        if host.is_empty() || port.is_empty() {
            self.connect_error = Some("Enter the host and port shown in the desktop QR dialog".into());
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
            let Some(events) = events.as_mut() else { return };
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
        self.link_state = "Disconnected".to_string();
        self.screen = Screen::Connect;
        cx.notify();
    }

    fn open_session(&mut self, info: &SessionInfo, cx: &mut Context<Self>) {
        if let Some(daemon) = &self.daemon {
            daemon.send(SessionCommand::GetSessionSnapshot {
                session_id: info.id.clone(),
            });
        }
        self.active = Some(ActiveSession::new(info));
        self.screen = Screen::Session;
        cx.notify();
    }

    fn apply_event(&mut self, event: MobileEvent, cx: &mut Context<Self>) {
        match event {
            MobileEvent::Connected => {
                self.link_state = "Live".to_string();
                self.connect_error = None;
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
            SessionEvent::SessionSnapshot { session_id, snapshot } => {
                if let Some(active) = self.active.as_mut().filter(|a| a.id == session_id) {
                    active.title = if snapshot.session.title.trim().is_empty() {
                        active.title.clone()
                    } else {
                        snapshot.session.title.clone()
                    };
                    active.messages = snapshot.messages;
                    active.status = None;
                }
            }
            SessionEvent::Agent { session_id, event } => {
                if let Some(active) = self.active.as_mut().filter(|a| a.id == session_id) {
                    active.apply_agent_event(&event);
                    active.scroll.scroll_to_bottom();
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
                for project in self.projects.iter_mut() {
                    project.sessions.retain(|session| session.id != session_id);
                }
                if self.active.as_ref().is_some_and(|a| a.id == session_id) {
                    self.active = None;
                    self.screen = Screen::Sessions;
                }
            }
            SessionEvent::DaemonError { session_id, message } => {
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

    fn answer_permission(&mut self, decision: PermissionDecision, cx: &mut Context<Self>) {
        let Some(active) = &mut self.active else { return };
        let Some(request) = active.permission.take() else { return };
        if let Some(daemon) = &self.daemon {
            daemon.send(SessionCommand::AnswerPermission {
                session_id: active.id.clone(),
                request_id: request.id,
                decision,
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
                        let reasoning = message
                            .reasoning_content
                            .get_or_insert_with(String::new);
                        reasoning.push_str(delta);
                    }
                    if let Some(name) = tool_call_name {
                        message
                            .tool_activities
                            .push(threadlane_protocol::daemon::ToolActivityInfo {
                                id: format!("tool-{}", message.tool_activities.len()),
                                category: "tool".to_string(),
                                title: name.clone(),
                                display_summary: String::new(),
                                detail: String::new(),
                                is_expanded: false,
                            });
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
    fn render_connect(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .size_full()
            .flex()
            .flex_col()
            .bg(cx.theme().background)
            .text_color(cx.theme().foreground)
            .p_4()
            .gap_4()
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .pt_8()
                    .child(div().text_xl().font_bold().child("Threadlane"))
                    .child(
                        div()
                            .text_sm()
                            .text_color(cx.theme().muted_foreground)
                            .child(
                                "Monitor your desktop sessions. Scan the QR code \
                                 in the desktop app (sidebar → Share with mobile) \
                                 or enter the pairing details below.",
                            ),
                    ),
            )
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
                this.child(
                    div()
                        .text_sm()
                        .text_color(cx.theme().danger)
                        .child(error),
                )
            })
            .child(
                Button::new("connect")
                    .primary()
                    .label("Connect")
                    .w_full()
                    .disabled(self.link_state == "Connecting…")
                    .on_click(cx.listener(|this, _, _, cx| this.connect_now(cx))),
            )
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
                            .child(div().font_bold().child("Sessions"))
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
                                    .children(project.sessions.iter().enumerate().map(|(index, session)| {
                                        let info = session.clone();
                                        Button::new(("session", index))
                                            .outline()
                                            .w_full()
                                            .child(
                                                div()
                                                    .flex()
                                                    .flex_col()
                                                    .items_start()
                                                    .w_full()
                                                    .child(
                                                        div().w_full().child(
                                                            if session.title.trim().is_empty() {
                                                                "Untitled session".to_string()
                                                            } else {
                                                                session.title.clone()
                                                            },
                                                        ),
                                                    )
                                                    .child(
                                                        div()
                                                            .text_xs()
                                                            .text_color(
                                                                cx.theme().muted_foreground,
                                                            )
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
                    }),
            )
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
                            .children(messages.iter().map(|message| self.render_message(message, cx)))
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
    }

    fn render_message(&self, message: &ChatMessageInfo, cx: &Context<Self>) -> AnyElement {
        let is_user = message.role == MessageRole::User;
        let bubble = div()
            .px_3()
            .py_2()
            .rounded_lg()
            .when(is_user, |this| this.bg(cx.theme().muted))
            .child(
                div()
                    .text_sm()
                    .line_height(relative(1.5))
                    .when(!message.content.is_empty(), |this| {
                        this.child(message.content.clone())
                    })
                    .when(message.streaming && message.content.is_empty(), |this| {
                        this.text_color(cx.theme().muted_foreground)
                            .child("…")
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
                            .text_xs()
                            .text_color(cx.theme().muted_foreground)
                            .child(format!("⚙ {}", tool.title))
                    })),
            );
        div()
            .w_full()
            .flex()
            .when(is_user, |this| this.justify_end())
            .child(div().max_w(rems(22.)).child(bubble))
            .into_any_element()
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
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.answer_permission(decision, cx)
                    })),
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
                    .text_sm()
                    .font_bold()
                    .child(format!("{}: {}", request.capability, request.title)),
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
                div()
                    .flex()
                    .gap_2()
                    .children(allow_buttons)
                    .child(
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
