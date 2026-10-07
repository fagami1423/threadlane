//! Preview-local clipboard and draft actions; never changes captured messages.
use super::SessionPreview;
use gpui::{prelude::*, *};
use gpui_component::menu::PopupMenu;
use gpui_component::text::TextViewState;
use gpui_component::{notification::Notification, WindowExt};
use threadlane_protocol::daemon::{ChatMessageInfo, MessageRole};
use threadlane_ui_kit as kit;

/// Mirrors the desktop quote contract without pulling in the chat surface:
/// exactly one selected segment, capped at 4,000 Unicode scalar values.
const PREVIEW_QUOTE_MAX_SCALARS: usize = 4_000;
const PREVIEW_QUOTE_NO_SELECTION: &str =
    "Select text within one response paragraph or code block";
const PREVIEW_QUOTE_OVER_LIMIT: &str = "Select 4,000 characters or fewer";

/// Render-time state for a message's Quote controls in the preview.
#[derive(Clone)]
pub(super) struct PreviewQuote {
    pub enabled: bool,
    pub reason: SharedString,
    /// The selected text captured for insertion when enabled.
    pub text: Option<String>,
}

impl SessionPreview {
    /// Evaluates whether the current window selection is quotable for this
    /// message: nonempty, inside exactly one of its content views, and within
    /// the length cap. Returns the disabled reason otherwise.
    pub(super) fn quote_control(
        states: &[Entity<TextViewState>],
        streaming: bool,
        window: &mut Window,
        cx: &mut App,
    ) -> PreviewQuote {
        let disabled = |reason: &'static str| PreviewQuote {
            enabled: false,
            reason: SharedString::from(reason),
            text: None,
        };
        if streaming || states.is_empty() {
            return disabled(PREVIEW_QUOTE_NO_SELECTION);
        }
        let window_selection = gpui_kit::base::TextSelection::selected_text(window, cx);
        let selected: Vec<String> = states
            .iter()
            .map(|state| state.read(cx).selected_text())
            .filter(|text| !text.trim().is_empty())
            .collect();
        let Some(text) = selected.first().cloned() else {
            return disabled(PREVIEW_QUOTE_NO_SELECTION);
        };
        if selected.len() != 1 || text != window_selection {
            return disabled(PREVIEW_QUOTE_NO_SELECTION);
        }
        if text.chars().count() > PREVIEW_QUOTE_MAX_SCALARS {
            return disabled(PREVIEW_QUOTE_OVER_LIMIT);
        }
        PreviewQuote {
            enabled: true,
            reason: SharedString::default(),
            text: Some(text),
        }
    }

    /// Appends the labeled blockquote to the draft and focuses the composer,
    /// preserving any draft text already present.
    pub(super) fn insert_quote(
        &mut self,
        text: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let mut draft = self.input.read(cx).value().to_string();
        if !draft.is_empty() && !draft.ends_with('\n') {
            draft.push('\n');
        }
        draft.push_str("Quoted from assistant response:\n");
        for line in text.split('\n') {
            draft.push_str("> ");
            draft.push_str(line);
            draft.push('\n');
        }
        draft.push('\n');
        self.input
            .update(cx, |input, cx| input.set_value(draft, window, cx));
        self.input.update(cx, |input, cx| input.focus(window, cx));
        cx.notify();
    }

    pub(super) fn message_context_menu(
        &self,
        message: &ChatMessageInfo,
        quote_states: Option<Vec<Entity<TextViewState>>>,
        cx: &mut Context<Self>,
    ) -> impl Fn(PopupMenu, &mut Window, &mut Context<PopupMenu>) -> PopupMenu + 'static {
        let content = message.content.clone();
        let streaming = message.streaming;
        let host = cx.entity().downgrade();
        move |menu, window, cx| {
            let content = content.clone();
            let quote_item = quote_states.as_ref().map(|states| {
                let quote = Self::quote_control(states, streaming, window, cx);
                let host = host.clone();
                kit::message_quote_menu_item(
                    quote.enabled,
                    (!quote.enabled).then_some(quote.reason),
                    move |_, window, cx| {
                        if let Some(text) = quote.text.clone() {
                            let _ = host.update(cx, |this, cx| {
                                this.insert_quote(text, window, cx);
                            });
                        }
                    },
                )
            });
            kit::message_context_menu_with_quote(
                menu,
                move |_, window, cx| {
                    cx.write_to_clipboard(ClipboardItem::new_string(content.clone()));
                    window.push_notification(Notification::info("Copied to clipboard"), cx);
                },
                quote_item,
            )
        }
    }
    pub(super) fn copy_text(&mut self, key: String, content: String, cx: &mut Context<Self>) {
        cx.write_to_clipboard(ClipboardItem::new_string(content));
        self.copied_message = Some(key);
        // Retain one portable timer. A newer copy replaces the confirmation.
        self.copy_feedback_task = Some(cx.spawn(async move |owner, cx| {
            cx.background_executor()
                .timer(kit::MESSAGE_COPY_FEEDBACK_WINDOW)
                .await;
            let _ = owner.update(cx, |host, cx| {
                host.copied_message = None;
                cx.notify();
            });
        }));
        cx.notify();
    }
    pub(super) fn render_message_actions(
        &self,
        message: &ChatMessageInfo,
        align_end: bool,
        quote: Option<PreviewQuote>,
        cx: &mut Context<Self>,
    ) -> Div {
        let key = format!("message-copy-{}", message.id);
        let copied = self.copied_message.as_deref() == Some(key.as_str());
        let content = message.content.clone();
        let edit_content = content.clone();
        kit::message_actions(align_end)
            .when_some(quote, |el, quote| {
                el.child(
                    kit::message_quote_button(&message.id, quote.enabled, quote.reason)
                        .on_click(cx.listener(move |host, _, window, cx| {
                            if let Some(text) = quote.text.clone() {
                                host.insert_quote(text, window, cx);
                            }
                        })),
                )
            })
            .child(
                kit::message_copy_button(&message.id, copied, cx).on_click(cx.listener(
                    move |host, _, _, cx| {
                        host.copy_text(key.clone(), content.clone(), cx);
                    },
                )),
            )
            .when(message.role == MessageRole::User, |el| {
                el.child(kit::message_edit_button(&message.id).on_click(cx.listener(
                    move |host, _, window, cx| {
                        if !host.input.read(cx).value().is_empty() {
                            window.push_notification(
                                Notification::info(
                                    "Send or clear your draft before editing a message",
                                ),
                                cx,
                            );
                        } else {
                            host.prompt_recall = None;
                            host.input.update(cx, |input, cx| {
                                input.set_value(edit_content.clone(), window, cx)
                            });
                        }
                        host.input.update(cx, |input, cx| input.focus(window, cx));
                        cx.notify();
                    },
                )))
            })
    }
}
