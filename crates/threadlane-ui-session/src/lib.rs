//! Product session components. Hosts own navigation, services and native actions.
use gpui::{prelude::*, *};
use gpui_component::button::{Button, ButtonVariants};
use gpui_component::input::{Input, InputState, Textarea, TextareaState};
use gpui_component::scroll::ScrollableElement;
use gpui_component::spinner::Spinner;
use gpui_component::tag::{Tag, TagVariant};
use gpui_component::{ActiveTheme, Disableable, Selectable, Sizable, StyledExt};
use threadlane_protocol::daemon::{MessageRole, SessionAttention, SessionInfo};
use threadlane_protocol::QuestionItem;
use threadlane_ui_theme::{CHAT_CONTENT_MAX_WIDTH, QUESTION_CARD_MAX_WIDTH, USER_BUBBLE_MAX_WIDTH};

pub mod markdown;
pub mod transcript;

pub fn user_message_bubble(cx: &App) -> Div {
    let theme = cx.theme();
    div()
        .min_w_0()
        .max_w(rems(USER_BUBBLE_MAX_WIDTH))
        .px_4()
        .py_3()
        .rounded_2xl()
        .rounded_br_md()
        .border_1()
        .border_color(theme.border.opacity(0.22))
        .bg(theme.secondary.opacity(0.85))
        .text_sm()
        .text_color(theme.secondary_foreground)
}

/// One session card; callers supply identity, metadata, signals and actions.
pub fn session_card(id: &str, selected: bool, cx: &App) -> Stateful<Div> {
    let theme = cx.theme().colors;
    div()
        .id(SharedString::from(format!("session-card-{id}")))
        .group("session-card")
        .role(Role::ListItem)
        .relative()
        .flex()
        .items_stretch()
        .w_full()
        .my(rems(0.1875))
        .rounded_xl()
        .bg(if selected {
            theme.sidebar_accent
        } else {
            transparent_black()
        })
        .border_1()
        .border_color(if selected {
            theme.primary.opacity(0.28)
        } else {
            transparent_black()
        })
        .when(selected, |el| {
            el.shadow_sm().child(
                div()
                    .absolute()
                    .left_1()
                    .top_2()
                    .bottom_2()
                    .w(rems(0.125))
                    .rounded_full()
                    .bg(theme.primary.opacity(0.9)),
            )
        })
        .hover(move |style| {
            style.bg(if selected {
                theme.sidebar_accent
            } else {
                theme.list_hover.opacity(0.7)
            })
        })
}

pub fn session_attention(id: &str, attention: SessionAttention, cx: &App) -> Option<AnyElement> {
    let theme = cx.theme().colors;
    let session_id = id;
    match attention {
        SessionAttention::NeedsYou => Some(
            div()
                .debug_selector({
                    let id = session_id.to_owned();
                    move || format!("session-attention-{id}")
                })
                .flex()
                .flex_none()
                .items_center()
                .gap_1()
                .px_1p5()
                .py(rems(0.125))
                .rounded_full()
                .bg(theme.warning.opacity(0.12))
                .text_color(theme.warning)
                .child(div().size(rems(0.3125)).rounded_full().bg(theme.warning))
                .child(
                    div()
                        .text_xs()
                        .font_weight(FontWeight::SEMIBOLD)
                        .child(attention.label()),
                )
                .into_any_element(),
        ),
        SessionAttention::Working => Some(
            div()
                .flex()
                .flex_none()
                .items_center()
                .gap_1()
                .px_1p5()
                .py(rems(0.125))
                .rounded_full()
                .bg(theme.info.opacity(0.1))
                .text_color(theme.foreground)
                .child(Spinner::new().xsmall().color(theme.info))
                .child(
                    div()
                        .text_xs()
                        .font_weight(FontWeight::SEMIBOLD)
                        .child(attention.label()),
                )
                .into_any_element(),
        ),
        SessionAttention::Ready => Some(
            div()
                .flex_none()
                .px_1p5()
                .py(rems(0.125))
                .rounded_full()
                .bg(theme.muted.opacity(0.35))
                .text_xs()
                .font_medium()
                .text_color(theme.muted_foreground.opacity(0.9))
                .child(attention.label())
                .into_any_element(),
        ),
        SessionAttention::Idle => None,
    }
}

/// The same multiline editing control on desktop and iOS.
pub fn composer_input(input: &Entity<TextareaState>) -> impl IntoElement {
    Textarea::new(input)
        .appearance(false)
        .bordered(false)
        .aria_label("Message the agent")
}

pub fn composer_surface(cx: &App) -> Div {
    let theme = cx.theme().colors;
    div()
        .w_full()
        .max_w(rems(CHAT_CONTENT_MAX_WIDTH))
        .mx_auto()
        .relative()
        .min_h(rems(6.0))
        .flex()
        .flex_col()
        .justify_between()
        .px_4()
        .pt_3p5()
        .pb_3()
        .rounded_2xl()
        .border_1()
        .border_color(theme.border.opacity(0.5))
        .bg(theme.popover)
        .shadow_lg()
        .hover(|style| style.border_color(theme.primary.opacity(0.28)))
}

/// Message row geometry is shared; hosts can supply richer body/action slots.
pub fn message_row(role: MessageRole) -> Div {
    div()
        .w_full()
        .min_w_0()
        .flex()
        .flex_col()
        .when(role == MessageRole::User, |el| {
            el.items_end().my_2p5().px_5()
        })
        .when(role == MessageRole::Assistant, |el| el.my_3().px_5())
}

/// A controlled question item. Its input entity and selections belong to the card owner.
pub fn question_item(
    request_id: &str,
    item: &QuestionItem,
    selected: &[String],
    input: Option<&Entity<InputState>>,
    touch: bool,
    on_toggle: impl Fn(&str, &mut Window, &mut App) + 'static,
    cx: &App,
) -> Div {
    let theme = cx.theme().colors;
    let on_toggle = std::rc::Rc::new(on_toggle);
    let options = item
        .options
        .iter()
        .enumerate()
        .map(|(ix, option)| {
            let picked = selected.contains(option);
            let value = option.clone();
            let callback = on_toggle.clone();
            let label = if picked {
                "Selected — activate to remove"
            } else {
                "Toggle this answer"
            };
            Button::new(SharedString::from(format!(
                "question-{request_id}-{}-{option}",
                item.id
            )))
            .debug_selector({
                let id = item.id.clone();
                move || format!("question-option-{id}-{ix}")
            })
            .label(option.clone())
            .small()
            .outline()
            .rounded_full()
            .selected(picked)
            .tooltip(label)
            .accessibility_label(format!("{option}, {label}"))
            .when(touch, |button| button.h_11())
            .on_click(move |_, window, cx| callback(&value, window, cx))
        })
        .collect::<Vec<_>>();
    div()
        .flex()
        .flex_col()
        .gap_1()
        .child(
            div()
                .text_xs()
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(theme.foreground)
                .child(item.header.clone()),
        )
        .child(
            div()
                .text_xs()
                .text_color(theme.muted_foreground)
                .child(item.question.clone()),
        )
        .child(div().flex().flex_wrap().gap_1().children(options))
        .children(input.filter(|_| item.allow_custom).map(|input| {
            div().w_full().child(
                Input::new(input)
                    .small()
                    .when(touch, |input| input.large())
                    .aria_label(format!("Custom answer for {}", item.header)),
            )
        }))
}

/// The shared virtualized transcript viewport; screen-specific row actions are slots.
pub fn transcript_list(
    state: &transcript::TranscriptState,
    render: impl FnMut(usize, &mut Window, &mut App) -> AnyElement + 'static,
) -> gpui::List {
    gpui::list(state.list.clone(), render)
        .w_full()
        .h_full()
        .mx_auto()
        .pt_3()
        .pb_6()
        .with_sizing_behavior(ListSizingBehavior::Auto)
}

/// Permission decisions carry the retained request ID so stale cards cannot answer a replacement.
pub fn permission_card(
    request: &threadlane_protocol::PermissionRequest,
    touch: bool,
    enabled: bool,
    details: Option<AnyElement>,
    on_decision: impl Fn(&str, threadlane_protocol::daemon::PermissionDecision, &mut Window, &mut App)
        + 'static,
    cx: &App,
) -> Stateful<Div> {
    use threadlane_protocol::{daemon::PermissionDecision, PermissionScope};
    let theme = cx.theme().colors;
    let callback = std::rc::Rc::new(on_decision);
    let button = |id: &'static str, label: &'static str, decision, primary| {
        let callback = callback.clone();
        let request_id = request.id.clone();
        Button::new(id)
            .label(label)
            .xsmall()
            .rounded_md()
            .disabled(!enabled)
            .when(primary, |b| b.primary())
            .when(touch, |b| b.h_11())
            .on_click(move |_, window, cx| callback(&request_id, decision, window, cx))
    };
    div()
        .id("permission-prompt-card")
        .role(Role::Alert)
        .aria_label("Permission request")
        .w_full()
        .max_w(rems(CHAT_CONTENT_MAX_WIDTH))
        .mx_auto()
        .px_3p5()
        .py_2p5()
        .rounded_xl()
        .border_1()
        .border_color(theme.warning.opacity(0.4))
        .bg(theme.secondary.opacity(0.35))
        .shadow_sm()
        .flex()
        .flex_wrap()
        .items_center()
        .gap_2()
        .child(
            div()
                .flex_1()
                .min_w_0()
                .flex()
                .flex_col()
                .gap_1()
                .child(
                    div()
                        .text_xs()
                        .font_weight(FontWeight::MEDIUM)
                        .text_color(theme.foreground)
                        .child(request.title.clone()),
                )
                .child(
                    div()
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .child(request.detail.clone()),
                ),
        )
        .children(details)
        .child(button(
            "permission-deny",
            "Deny",
            PermissionDecision::Deny,
            false,
        ))
        .when(request.scopes.contains(&PermissionScope::Once), |el| {
            el.child(button(
                "permission-allow-once",
                "Allow once",
                PermissionDecision::AllowOnce,
                true,
            ))
        })
        .when(request.scopes.contains(&PermissionScope::Session), |el| {
            el.child(
                button(
                    "permission-allow-session",
                    "Allow session",
                    PermissionDecision::AllowSession,
                    false,
                )
                .debug_selector(|| "permission-inline-session".into()),
            )
        })
        .when(request.scopes.contains(&PermissionScope::Always), |el| {
            el.child(
                button(
                    "permission-allow-always",
                    "Always allow",
                    PermissionDecision::AllowAlways,
                    false,
                )
                .debug_selector(|| "permission-inline-always".into()),
            )
        })
}

pub fn question_surface(cx: &App) -> Div {
    let theme = cx.theme().colors;
    div()
        .w_full()
        .max_w(rems(QUESTION_CARD_MAX_WIDTH))
        .mx_auto()
        .px_3p5()
        .py_3()
        .rounded_xl()
        .border_1()
        .border_color(theme.border.opacity(0.8))
        .bg(theme.popover)
        .shadow_md()
        .flex()
        .flex_col()
        .gap_2()
}

pub struct SessionIdentity {
    pub title: String,
    pub tooltip: String,
}

pub fn session_identity(session: &SessionInfo) -> SessionIdentity {
    let Some(issue) = session.github_issue.as_ref() else {
        return SessionIdentity {
            title: session.title.clone(),
            tooltip: session.title.clone(),
        };
    };
    let prefix = format!("#{}", issue.number);
    let title = session.title.trim();
    let issue_title = title
        .strip_prefix(&prefix)
        .filter(|rest| rest.is_empty() || rest.chars().next().is_some_and(char::is_whitespace))
        .map(str::trim_start)
        .unwrap_or(title);
    let title = if issue_title.is_empty() {
        prefix.clone()
    } else {
        format!("{prefix} {issue_title}")
    };
    SessionIdentity {
        tooltip: format!("{}/{}\n{}", issue.owner, issue.repo, title),
        title,
    }
}

/// Shared virtualized navigation list and its scrollbar. Grouping belongs to the owner.
pub fn session_list(
    state: ListState,
    render: impl FnMut(usize, &mut Window, &mut App) -> AnyElement + 'static,
) -> Div {
    div()
        .relative()
        .size_full()
        .pt_2()
        .child(
            list(state.clone(), render)
                .size_full()
                .pb_3()
                .with_sizing_behavior(ListSizingBehavior::Auto),
        )
        .child(
            div()
                .absolute()
                .inset_0()
                .child(gpui_component::scroll::Scrollbar::vertical(&state)),
        )
}

pub fn reasoning_token_badge(is_streaming: bool, reasoning_len: usize) -> String {
    if is_streaming {
        return "thinking…".to_string();
    }
    let approx_tokens = (reasoning_len + 3) / 4;
    if approx_tokens == 1 {
        "~1 token".to_string()
    } else {
        format!("~{approx_tokens} tokens")
    }
}

pub fn tool_activity_glyph(category: &str) -> &'static str {
    match category {
        "Error" => "!",
        "Working" | "Thinking" => "◌",
        "Completed" | "Edited" | "Created" | "Ran" | "Loaded" | "Explored" => "✓",
        _ => "•",
    }
}

pub fn reasoning_card(
    msg: &threadlane_protocol::daemon::ChatMessageInfo,
    detail: Option<AnyElement>,
    touch: bool,
    on_toggle: impl Fn(&mut Window, &mut App) + 'static,
    cx: &App,
) -> AnyElement {
    let theme = cx.theme().colors;
    let is_streaming = msg.streaming;
    let is_expanded = msg.reasoning_expanded;
    let approx_badge = reasoning_token_badge(
        is_streaming,
        msg.reasoning_content.as_ref().map_or(0, String::len),
    );
    let token_badge = Tag::new()
        .child(approx_badge)
        .small()
        .with_variant(TagVariant::Secondary);

    let header = Button::new(SharedString::from(format!("reasoning-toggle-{}", msg.id)))
        .debug_selector(|| "reasoning-disclosure".into())
        .accessibility_label(if is_expanded {
            "Collapse thought process"
        } else {
            "Expand thought process"
        })
        .tooltip(if is_expanded {
            "Collapse thought process"
        } else {
            "Expand thought process"
        })
        .ghost()
        .small()
        .w_full()
        .when(is_expanded, |button| button.px_3().rounded_none())
        .flex()
        .items_center()
        .justify_between()
        .child(
            div()
                .flex()
                .items_center()
                .gap_2()
                .min_w_0()
                .flex_1()
                .child(
                    div()
                        .text_sm()
                        .text_color(if is_streaming {
                            theme.primary
                        } else {
                            theme.muted_foreground
                        })
                        .child("✦"),
                )
                .child(
                    div()
                        .text_xs()
                        .font_weight(FontWeight::MEDIUM)
                        .text_color(theme.muted_foreground)
                        .child(if is_streaming {
                            "Thinking…"
                        } else {
                            "Thought process"
                        }),
                )
                .child(token_badge),
        )
        .child(
            gpui_component::Icon::default()
                .data(if is_expanded {
                    gpui_kit_assets::__private::ChevronDown.1
                } else {
                    gpui_kit_assets::__private::ChevronRight.1
                })
                .xsmall()
                .text_color(theme.muted_foreground),
        )
        .when(touch, |button| button.h_11())
        .on_click(move |_, window, cx| on_toggle(window, cx));
    // Collapsed reasoning reads as a quiet transcript row, like tool
    // activity; the bordered card only appears around expanded content.
    let container = div()
        .w_full()
        .min_w_0()
        .overflow_hidden()
        .when(is_expanded, |el| {
            el.rounded_xl()
                .border_1()
                .border_color(theme.border.opacity(0.3))
                .bg(theme.muted.opacity(0.14))
        });

    container.child(header).children(detail).into_any_element()
}

pub fn reasoning_detail(cx: &App) -> gpui_component::scroll::Scrollable<Div> {
    let theme = cx.theme().colors;
    div()
        .p_3()
        .max_h(rems(21.25))
        .border_t_1()
        .border_color(theme.border.opacity(0.35))
        .bg(theme.background.opacity(0.4))
        .text_xs()
        .text_color(theme.muted_foreground)
        .overflow_y_scrollbar()
}

pub fn tool_activity(
    activity: &threadlane_protocol::daemon::ToolActivityInfo,
    has_detail: bool,
    detail: Option<AnyElement>,
    touch: bool,
    on_toggle: impl Fn(&mut Window, &mut App) + 'static,
    cx: &App,
) -> AnyElement {
    let theme = cx.theme().colors;
    let marker = tool_activity_glyph(activity.category.as_str());
    let marker_color = match activity.category.as_str() {
        "Error" => theme.danger,
        "Working" | "Thinking" => theme.primary,
        "Completed" | "Edited" | "Created" | "Ran" | "Loaded" | "Explored" => theme.success,
        _ => theme.muted_foreground,
    };
    let row_id = SharedString::from(activity.id.clone());
    let display_summary = activity.display_summary.clone();
    let is_error = activity.category == "Error";
    let summary_color = if is_error {
        theme.danger
    } else {
        theme.muted_foreground
    };

    div()
        .w_full()
        .min_w_0()
        .flex()
        .flex_col()
        .py_1()
        .child(
            Button::new(row_id)
                .debug_selector(|| "tool-activity-disclosure".into())
                .accessibility_label(display_summary.clone())
                .tooltip(display_summary.clone())
                .ghost()
                .small()
                .when(touch, |button| button.h_11())
                .w_full()
                .justify_start()
                .disabled(!has_detail)
                .gap_2()
                .when(has_detail, |row| {
                    row.on_click(move |_, window, cx| on_toggle(window, cx))
                })
                .child({
                    let marker_el = div()
                        .w(rems(1.125))
                        .flex_none()
                        .text_center()
                        .text_xs()
                        .font_weight(FontWeight::BOLD)
                        .text_color(marker_color)
                        .child(marker);
                    marker_el.into_any_element()
                })
                .child(
                    div()
                        .min_w_0()
                        .flex_1()
                        .truncate()
                        .text_sm()
                        .text_color(summary_color)
                        .child(display_summary.clone()),
                )
                .children(has_detail.then(|| {
                    gpui_component::Icon::default()
                        .data(if activity.is_expanded {
                            gpui_kit_assets::__private::ChevronDown.1
                        } else {
                            gpui_kit_assets::__private::ChevronRight.1
                        })
                        .xsmall()
                        .text_color(theme.muted_foreground)
                })),
        )
        .children(detail)
        .into_any_element()
}

pub fn tool_detail(cx: &App) -> gpui_component::scroll::Scrollable<Div> {
    let theme = cx.theme().colors;
    div()
        .ml(rems(1.625))
        .mt_1()
        .p_2p5()
        .max_h(rems(15.0))
        .rounded_lg()
        .border_1()
        .border_color(theme.border.opacity(0.5))
        .bg(theme.title_bar)
        .text_xs()
        .text_color(theme.muted_foreground)
        .overflow_y_scrollbar()
}
