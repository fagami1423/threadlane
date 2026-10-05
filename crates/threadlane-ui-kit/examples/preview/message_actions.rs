//! Preview-local clipboard and draft actions; never changes captured messages.
use super::SessionPreview;
use gpui::{prelude::*, *};
use gpui_component::menu::PopupMenu;
use gpui_component::{notification::Notification, WindowExt};
use threadlane_protocol::daemon::{ChatMessageInfo, MessageRole};
use threadlane_ui_kit as kit;

impl SessionPreview {
    pub(super) fn message_context_menu(
        message: &ChatMessageInfo,
    ) -> impl Fn(PopupMenu, &mut Window, &mut Context<PopupMenu>) -> PopupMenu + 'static {
        let content = message.content.clone();
        move |menu, _, _| {
            let content = content.clone();
            kit::message_context_menu(menu, move |_, window, cx| {
                cx.write_to_clipboard(ClipboardItem::new_string(content.clone()));
                window.push_notification(Notification::info("Copied to clipboard"), cx);
            })
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
        cx: &mut Context<Self>,
    ) -> Div {
        let key = format!("message-copy-{}", message.id);
        let copied = self.copied_message.as_deref() == Some(key.as_str());
        let content = message.content.clone();
        let edit_content = content.clone();
        kit::message_actions(align_end)
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
