//! Sample buffers; all presentation comes from the production UI kit.
use gpui::{prelude::*, *};
use gpui_component::WindowExt;
use gpui_component::button::ButtonVariant;
use gpui_component::dialog::DialogButtonProps;
use gpui_component::input::{EditorState, InputEvent, TabSize};
use gpui_component::menu::ContextMenuExt;
use gpui_component::text::TextViewState;
use std::collections::HashMap;
use threadlane_ui_kit as kit;

actions!(ui_kit_editor_preview, [SaveSampleBuffer]);

pub(crate) const FILE: &str = "example.rs";
const DIFF: &str = "diff:example.rs";
const SAMPLE: &str = "// UI kit sample — edits stay in this preview.\nfn main() {\n    println!(\"Hello, Threadlane!\");\n}\n";

const SAMPLE_DIFF: &str = "--- a/example.rs\n+++ b/example.rs\n@@ -1,3 +1,3 @@\n fn main() {\n-    println!(\"Hello!\");\n+    println!(\"Hello, Threadlane!\");\n }";

pub struct EditorPreview {
    buffer: Entity<EditorState>,
    diffs: HashMap<String, Entity<TextViewState>>,
    saved: String,
    tabs: Vec<String>,
    selected: Option<String>,
    status: Option<String>,
    _subscription: Subscription,
}

impl EditorPreview {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        cx.bind_keys([
            KeyBinding::new("cmd-s", SaveSampleBuffer, Some("EditorPreview")),
            KeyBinding::new("ctrl-s", SaveSampleBuffer, Some("EditorPreview")),
        ]);
        let buffer = cx.new(|cx| {
            EditorState::new(window, cx)
                .language("rust")
                .line_number(true)
                .folding(true)
                .show_whitespaces(false)
                .tab_size(TabSize {
                    tab_size: 4,
                    hard_tabs: false,
                })
                .default_value(SAMPLE)
        });
        let subscription = cx.subscribe(&buffer, |_, _, _: &InputEvent, cx| cx.notify());
        let markdown = format!("```diff\n{SAMPLE_DIFF}\n```");
        let diff = cx.new(|cx| TextViewState::markdown(&markdown, cx));
        Self {
            buffer,
            diffs: HashMap::from([(DIFF.into(), diff)]),
            saved: SAMPLE.into(),
            tabs: vec![FILE.into(), DIFF.into()],
            selected: Some(FILE.into()),
            status: None,
            _subscription: subscription,
        }
    }

    fn dirty(&self, cx: &App) -> bool {
        self.buffer.read(cx).value().as_str() != self.saved
    }

    pub fn open_sample(&mut self, cx: &mut Context<Self>) {
        if !self.tabs.iter().any(|id| id == FILE) {
            self.tabs.push(FILE.into());
        }
        self.selected = Some(FILE.into());
        cx.notify();
    }

    pub fn open_diff(&mut self, title: String, content: String, cx: &mut Context<Self>) {
        let id = format!("diff:{title}");
        let markdown = format!("```diff\n{}\n```", content.replace("```", "` ` `"));
        let state = cx.new(|cx| TextViewState::markdown(&markdown, cx));
        self.diffs.insert(id.clone(), state);
        if !self.tabs.contains(&id) {
            self.tabs.push(id.clone());
        }
        self.selected = Some(id);
        self.status = None;
        cx.notify();
    }

    fn save(&mut self, cx: &mut Context<Self>) {
        if self.selected.as_deref() == Some(FILE) && self.dirty(cx) {
            self.saved = self.buffer.read(cx).value().to_string();
            self.status = Some("Saved in this preview".into());
            cx.notify();
        }
    }

    pub fn reset(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.buffer
            .update(cx, |buffer, cx| buffer.set_value(SAMPLE, window, cx));
        self.saved = SAMPLE.into();
        self.diffs.clear();
        self.open_diff(FILE.into(), SAMPLE_DIFF.into(), cx);
        self.tabs = vec![FILE.into(), DIFF.into()];
        self.selected = Some(FILE.into());
        self.status = None;
        cx.notify();
    }

    fn close(
        &mut self,
        tab: &str,
        action: kit::EditorTabAction,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let targets: Vec<_> = self
            .tabs
            .iter()
            .filter(|id| match action {
                kit::EditorTabAction::Close => id.as_str() == tab,
                kit::EditorTabAction::CloseOthers => id.as_str() != tab,
                kit::EditorTabAction::CloseAll => true,
            })
            .cloned()
            .collect();
        if targets.iter().any(|id| id == FILE) && self.dirty(cx) {
            let entity = cx.entity().downgrade();
            let buffer = self.buffer.read(cx).value();
            window.open_alert_dialog(cx, move |dialog, _, _| {
                let targets = targets.clone();
                let entity = entity.clone();
                let buffer = buffer.clone();
                dialog
                    .title("Discard sample edits?")
                    .description("Close example.rs and discard its unsaved edits in this preview.")
                    .button_props(
                        DialogButtonProps::default()
                            .ok_text("Discard")
                            .ok_variant(ButtonVariant::Danger)
                            .show_cancel(true),
                    )
                    .on_ok(move |_, _, cx| {
                        let _ = entity.update(cx, |host, cx| {
                            // Keep any edits made after this confirmation opened.
                            if host.buffer.read(cx).value() == buffer {
                                host.remove(&targets, cx);
                            }
                        });
                        true
                    })
            });
        } else {
            self.remove(&targets, cx);
        }
    }

    fn remove(&mut self, targets: &[String], cx: &mut Context<Self>) {
        self.tabs.retain(|id| !targets.contains(id));
        if self
            .selected
            .as_ref()
            .is_some_and(|id| targets.contains(id))
        {
            self.selected = self.tabs.last().cloned();
        }
        self.diffs.retain(|id, _| !targets.contains(id));
        self.status = None;
        cx.notify();
    }
}

impl Render for EditorPreview {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let dirty = self.dirty(cx);
        let selected_diff = self
            .selected
            .as_ref()
            .is_some_and(|id| self.diffs.contains_key(id));
        kit::editor_surface(cx)
            .key_context("EditorPreview")
            .on_action(cx.listener(|host, _: &SaveSampleBuffer, _, cx| host.save(cx)))
            .children((!self.tabs.is_empty()).then(|| {
                kit::editor_tab_bar(cx)
                    .child(kit::editor_tabs().children(self.tabs.iter().map(|id| {
                        let select = cx.entity().downgrade();
                        let close = cx.entity().downgrade();
                        let menu = cx.entity().downgrade();
                        let selected_id = id.clone();
                        let closed_id = id.clone();
                        let menu_id = id.clone();
                        let diff = self.diffs.contains_key(id);
                        kit::editor_tab(
                            format!("sample-editor-{id}"),
                            kit::editor_tab_title(id, diff),
                            id.strip_prefix("diff:").unwrap_or(id).to_string(),
                            self.selected.as_ref() == Some(id),
                            dirty && id == FILE,
                            diff,
                            move |_, _, cx| {
                                let _ = select.update(cx, |host, cx| {
                                    host.selected = Some(selected_id.clone());
                                    cx.notify();
                                });
                            },
                            move |_, window, cx| {
                                let _ = close.update(cx, |host, cx| {
                                    host.close(&closed_id, kit::EditorTabAction::Close, window, cx)
                                });
                            },
                            cx,
                        )
                        .context_menu(move |popup, _, _| {
                            let menu = menu.clone();
                            let id = menu_id.clone();
                            kit::editor_tab_menu(popup, move |action, window, cx| {
                                let _ =
                                    menu.update(cx, |host, cx| host.close(&id, action, window, cx));
                            })
                        })
                    })))
                    .child(kit::editor_actions(
                        self.status.clone().map(|message| (message, false)),
                        kit::editor_save_button(
                            dirty && self.selected.as_deref() == Some(FILE),
                            selected_diff,
                        )
                        .on_click(cx.listener(|host, _, _, cx| host.save(cx))),
                        cx,
                    ))
            }))
            .child(if self.selected.as_deref() == Some(FILE) {
                kit::editor_buffer(&self.buffer).into_any_element()
            } else if let Some(diff) = self.selected.as_ref().and_then(|id| self.diffs.get(id)) {
                kit::editor_diff(diff, cx).into_any_element()
            } else {
                kit::editor_empty_state(cx).into_any_element()
            })
    }
}

#[cfg(test)]
#[path = "editor_tests.rs"]
mod tests;
