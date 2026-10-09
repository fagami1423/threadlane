//! Retained editor affordances shared by the desktop host and preview.
use std::rc::Rc;

use gpui::{prelude::*, *};
use gpui_component::button::{Button, ButtonVariants};
use gpui_component::input::{
    EditorState, Input, InputState, Position, Replace, Rope, RopeExt, Search,
};
use gpui_component::menu::DropdownMenu;
use gpui_component::{ActiveTheme, IconName, Sizable, WindowExt};

use crate::editor_completion::BufferWords;

actions!(
    editor_workbench,
    [
        GoToLine,
        ToggleWrap,
        ToggleWhitespace,
        ToggleGuides,
        ToggleWords
    ]
);

/// Presentation and in-memory editing only; the host still owns I/O and save guards.
pub struct EditorWorkbench {
    editor: Entity<EditorState>,
    language: SharedString,
    wrap: bool,
    whitespace: bool,
    guides: bool,
    words: bool,
    _observe: Subscription,
}

impl EditorWorkbench {
    /// Attach once per buffer, retaining display preferences across tab switches.
    pub fn new(
        editor: Entity<EditorState>,
        language: impl Into<SharedString>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        cx.bind_keys([
            KeyBinding::new("ctrl-g", GoToLine, Some("EditorWorkbench")),
            KeyBinding::new("alt-z", ToggleWrap, Some("EditorWorkbench")),
        ]);
        editor.update(cx, |state, cx| {
            state.set_soft_wrap(true, window, cx);
            state.set_indent_guides(true, window, cx);
            state.set_show_whitespaces(false, window, cx);
            state.set_searchable(true, cx);
            state.lsp_mut().completion_provider = Some(Rc::new(BufferWords));
        });
        let observe = cx.observe(&editor, |_, _, cx| cx.notify());
        Self {
            editor,
            language: language.into(),
            wrap: true,
            whitespace: false,
            guides: true,
            words: true,
            _observe: observe,
        }
    }

    fn go_to_line(&mut self, _: &GoToLine, window: &mut Window, cx: &mut Context<Self>) {
        if window.has_active_dialog(cx) || window.has_active_sheet(cx) {
            return;
        }
        let editor = self.editor.clone();
        let cursor = editor.read(cx).cursor_position();
        let lines = editor.read(cx).text().lines_len();
        let input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("Line:column")
                .default_value(format!("{}:{}", cursor.line + 1, cursor.character + 1))
        });
        let focus_input = input.clone();
        window.open_alert_dialog(cx, move |dialog, _, _| {
            let editor = editor.clone();
            let input_for_ok = input.clone();
            dialog
                .title("Go to line")
                .child(format!(
                    "Enter a line from 1 to {lines}, optionally followed by :column."
                ))
                .child(Input::new(&input).aria_label("Line and optional column"))
                .on_ok(move |_, window, cx| {
                    let value = input_for_ok.read(cx).value();
                    let Some(position) = parse_location(&value, editor.read(cx).text()) else {
                        return false;
                    };
                    editor.update(cx, |state, cx| {
                        state.set_cursor_position(position, window, cx)
                    });
                    true
                })
        });
        window.defer(cx, move |window, cx| {
            focus_input.update(cx, |state, cx| state.focus(window, cx));
        });
    }

    fn toggle_wrap(&mut self, _: &ToggleWrap, window: &mut Window, cx: &mut Context<Self>) {
        self.wrap = !self.wrap;
        self.editor
            .update(cx, |state, cx| state.set_soft_wrap(self.wrap, window, cx));
        cx.notify();
    }

    fn toggle_whitespace(
        &mut self,
        _: &ToggleWhitespace,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.whitespace = !self.whitespace;
        self.editor.update(cx, |state, cx| {
            state.set_show_whitespaces(self.whitespace, window, cx)
        });
        cx.notify();
    }

    fn toggle_guides(&mut self, _: &ToggleGuides, window: &mut Window, cx: &mut Context<Self>) {
        self.guides = !self.guides;
        self.editor.update(cx, |state, cx| {
            state.set_indent_guides(self.guides, window, cx)
        });
        cx.notify();
    }

    fn toggle_words(&mut self, _: &ToggleWords, _: &mut Window, cx: &mut Context<Self>) {
        self.words = !self.words;
        self.editor.update(cx, |state, cx| {
            state.lsp_mut().completion_provider = if self.words {
                Some(Rc::new(BufferWords))
            } else {
                None
            };
            cx.notify();
        });
        cx.notify();
    }
}

fn parse_location(query: &str, text: &Rope) -> Option<Position> {
    let mut parts = query.trim().split(':');
    let line = parts.next()?.trim().parse::<usize>().ok()?.checked_sub(1)?;
    let column = parts
        .next()
        .map_or(Some(1), |part| part.trim().parse::<usize>().ok())?
        .checked_sub(1)?;
    if parts.next().is_some() || line >= text.lines_len() {
        return None;
    }
    let line_text = text.slice_line(line).to_string();
    let column = column.min(line_text.trim_end_matches(['\r', '\n']).chars().count());
    Some(Position::new(
        u32::try_from(line).ok()?,
        u32::try_from(column).ok()?,
    ))
}

impl Render for EditorWorkbench {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let position = self.editor.read(cx).cursor_position();
        let focus = self.editor.focus_handle(cx);
        let (wrap, whitespace, guides, words) =
            (self.wrap, self.whitespace, self.guides, self.words);
        let find_editor = self.editor.clone();
        let completion_editor = self.editor.clone();
        div()
            .key_context("EditorWorkbench")
            .flex()
            .flex_col()
            .size_full()
            .min_w_0()
            .min_h_0()
            .capture_action(move |action: &gpui_component::input::Enter, window, cx| {
                let handled = completion_editor.update(cx, |editor, cx| {
                    editor.route_overlay_action(Box::new(action.clone()), window, cx)
                });
                if handled {
                    cx.stop_propagation();
                }
            })
            .on_action(cx.listener(Self::go_to_line))
            .on_action(cx.listener(Self::toggle_wrap))
            .on_action(cx.listener(Self::toggle_whitespace))
            .on_action(cx.listener(Self::toggle_guides))
            .on_action(cx.listener(Self::toggle_words))
            .child(crate::editor_buffer(&self.editor))
            .child(
                div()
                    .flex_none()
                    .flex()
                    .flex_wrap()
                    .items_center()
                    .justify_between()
                    .gap_1()
                    .px_2()
                    .py_1()
                    .border_t_1()
                    .border_color(cx.theme().border)
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child(
                        div()
                            .min_w_0()
                            .overflow_hidden()
                            .text_ellipsis()
                            .child(self.language.clone()),
                    )
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_1()
                            .child(
                                Button::new("editor-position")
                                    .ghost()
                                    .xsmall()
                                    .label(format!(
                                        "Ln {}, Col {}",
                                        position.line + 1,
                                        position.character + 1
                                    ))
                                    .accessibility_label("Go to line")
                                    .tooltip("Go to line (Ctrl+G)")
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.go_to_line(&GoToLine, window, cx)
                                    })),
                            )
                            .child(
                                Button::new("editor-find")
                                    .ghost()
                                    .xsmall()
                                    .icon(IconName::Search)
                                    .accessibility_label("Find in file")
                                    .tooltip("Find in file")
                                    .on_click(move |_, window, cx| {
                                        find_editor.update(cx, |state, cx| state.focus(window, cx));
                                        window.dispatch_action(Box::new(Search), cx);
                                    }),
                            )
                            .child(
                                Button::new("editor-options")
                                    .ghost()
                                    .xsmall()
                                    .icon(IconName::Settings)
                                    .accessibility_label("Editor options")
                                    .tooltip("Editor options")
                                    .dropdown_menu(move |menu, _, _| {
                                        menu.action_context(focus.clone())
                                            .menu("Find in file", Box::new(Search))
                                            .menu("Replace in file", Box::new(Replace))
                                            .menu("Go to line…", Box::new(GoToLine))
                                            .separator()
                                            .menu_with_check(
                                                "Word wrap",
                                                wrap,
                                                Box::new(ToggleWrap),
                                            )
                                            .menu_with_check(
                                                "Show whitespace",
                                                whitespace,
                                                Box::new(ToggleWhitespace),
                                            )
                                            .menu_with_check(
                                                "Indent guides",
                                                guides,
                                                Box::new(ToggleGuides),
                                            )
                                            .separator()
                                            .menu_with_check(
                                                "Word suggestions (this file)",
                                                words,
                                                Box::new(ToggleWords),
                                            )
                                    }),
                            ),
                    ),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::{parse_location, EditorWorkbench};
    use gpui::{AppContext, TestAppContext};
    use gpui_component::input::{EditorState, Position, Rope};

    #[gpui::test]
    fn editor_workbench_wrap_shortcut_preserves_buffer_and_provider(cx: &mut TestAppContext) {
        cx.update(gpui_component::init);
        let (root, cx) = cx.add_window_view(|window, cx| {
            let editor = cx.new(|cx| {
                EditorState::new(window, cx)
                    .language("rust")
                    .default_value("fn main() {}")
            });
            let workbench = cx.new(|cx| EditorWorkbench::new(editor.clone(), "rust", window, cx));
            editor.update(cx, |state, cx| state.focus(window, cx));
            gpui_component::Root::new(workbench, window, cx)
        });
        let workbench = root.read_with(cx, |root, _| {
            root.view().clone().downcast::<EditorWorkbench>().unwrap()
        });
        cx.simulate_keystrokes("alt-z");
        workbench.read_with(cx, |view, cx| {
            assert!(!view.wrap);
            assert_eq!(view.editor.read(cx).value().as_str(), "fn main() {}");
            assert!(view.editor.read(cx).lsp().completion_provider.is_some());
        });
        cx.simulate_keystrokes("alt-z");
        assert!(workbench.read_with(cx, |view, _| view.wrap));
    }

    #[gpui::test]
    fn editor_completion_acceptance_replaces_whole_token_and_undoes(cx: &mut TestAppContext) {
        cx.update(gpui_component::init);
        let (root, cx) = cx.add_window_view(|window, cx| {
            let editor = cx.new(|cx| {
                EditorState::new(window, cx)
                    .language("rust")
                    .default_value("alpha_value\nalp_suffix")
            });
            let workbench = cx.new(|cx| EditorWorkbench::new(editor.clone(), "rust", window, cx));
            editor.update(cx, |state, cx| {
                state.set_cursor_position(Position::new(1, 3), window, cx);
                state.focus(window, cx);
            });
            gpui_component::Root::new(workbench, window, cx)
        });
        let workbench = root.read_with(cx, |root, _| {
            root.view().clone().downcast::<EditorWorkbench>().unwrap()
        });
        cx.simulate_keystrokes("h");
        cx.run_until_parked();
        cx.refresh().unwrap();
        cx.run_until_parked();
        workbench.read_with(cx, |view, cx| {
            let editor = view.editor.read(cx);
            assert_eq!(editor.value().as_str(), "alpha_value\nalph_suffix");
            assert_eq!(editor.cursor_position(), Position::new(1, 4));
            assert!(editor.has_overlay_action_handler());
            let menu = editor.completion_menu_state();
            assert!(menu.open);
            assert_eq!(menu.items[0].label, "alpha_value");
            let Some(lsp_types::CompletionTextEdit::Edit(edit)) = &menu.items[0].text_edit else {
                panic!("completion has no text edit");
            };
            assert_eq!(
                edit.range,
                lsp_types::Range::new(Position::new(1, 0), Position::new(1, 11))
            );
        });
        cx.simulate_keystrokes("enter");
        cx.run_until_parked();
        workbench.read_with(cx, |view, cx| {
            assert_eq!(
                view.editor.read(cx).value().as_str(),
                "alpha_value\nalpha_value"
            );
        });

        cx.simulate_keystrokes("secondary-z");
        workbench.read_with(cx, |view, cx| {
            assert_eq!(
                view.editor.read(cx).value().as_str(),
                "alpha_value\nalph_suffix"
            );
        });
        cx.simulate_keystrokes("secondary-z");
        workbench.read_with(cx, |view, cx| {
            assert_eq!(
                view.editor.read(cx).value().as_str(),
                "alpha_value\nalp_suffix"
            );
        });
    }

    #[test]
    fn editor_location_validates_and_clamps_unicode_columns() {
        let text = Rope::from("alpha\r\n😀 café\n");
        assert_eq!(parse_location(" 2 : 3 ", &text), Some(Position::new(1, 2)));
        assert_eq!(parse_location("2:999", &text), Some(Position::new(1, 6)));
        assert_eq!(parse_location("1:999", &text), Some(Position::new(0, 5)));
        assert_eq!(parse_location("3", &text), Some(Position::new(2, 0)));
        for query in [
            "",
            "0",
            "1:0",
            "4",
            "1:2:3",
            "1:",
            "no",
            "-1",
            "999999999999999999999999",
        ] {
            assert_eq!(parse_location(query, &text), None, "{query}");
        }
    }
}
