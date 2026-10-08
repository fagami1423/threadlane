//! Host-owned patch document: no filesystem access, Git operations, or persistence.
use std::{cell::Cell, ops::Range, rc::Rc, time::Duration};

use gpui::{prelude::*, *};
use gpui_component::button::{Button, ButtonVariants};
use gpui_component::input::{Enter, Escape, Input, InputEvent, InputState};
use gpui_component::text::TextViewState;
use gpui_component::{ActiveTheme, Disableable, Sizable};
use gpui_kit::base::text::{RangeHighlight, RenderedText};

use crate::{ReviewDiffAction, ReviewDiffContent};

const MATCH_LIMIT: usize = 10_000;
actions!(threadlane_review_find, [FindInDiff]);
struct FindBindings;
impl Global for FindBindings {}

#[derive(Default, Debug)]
struct Matches {
    ranges: Vec<Range<usize>>,
    capped: bool,
}

fn find_matches(text: &str, query: &str) -> Result<Matches, String> {
    if query.is_empty() {
        return Ok(Matches::default());
    }
    // Escaping keeps the UI literal; regex supplies Unicode simple case folding
    // and returns byte ranges in the original (not lowercased) rendered text.
    let pattern = regex::RegexBuilder::new(&regex::escape(query))
        .case_insensitive(true)
        .build()
        .map_err(|_| "Query is too large; refine your search".to_string())?;
    let mut matches = Matches::default();
    for found in pattern.find_iter(text) {
        if matches.ranges.len() == MATCH_LIMIT {
            matches.capped = true;
            break;
        }
        matches.ranges.push(found.range());
    }
    Ok(matches)
}

fn step_match(current: Option<usize>, count: usize, previous: bool) -> Option<usize> {
    if count == 0 {
        None
    } else {
        Some(match (current, previous) {
            (None, false) => 0,
            (None, true) => count - 1,
            (Some(index), false) => (index + 1) % count,
            (Some(index), true) => (index + count - 1) % count,
        })
    }
}

/// Fences longer than any run in the patch preserve literal backticks on copy.
fn patch_markdown(patch: &str) -> String {
    let fence = crate::safe_fence(patch);
    format!("{fence}diff\n{patch}\n{fence}")
}

enum DocumentState {
    Loading,
    Failed(String),
    Ready { empty: bool },
}

/// Both native Review and saved previews own this shared document entity.
/// Hosts must invalidate it before a refresh and reset it on identity changes.
pub struct ReviewDiffDocument {
    text: Entity<TextViewState>,
    input: Entity<InputState>,
    focus: FocusHandle,
    scroll: ScrollHandle,
    reveal_enabled: Rc<Cell<bool>>,
    reveal_after_search: bool,
    restore_offset: Option<Point<Pixels>>,
    state: DocumentState,
    ignore_whitespace: bool,
    open: bool,
    query: String,
    generation: u64,
    snapshot: Option<RenderedText>,
    waiting_for_parse: bool,
    pending: bool,
    matches: Matches,
    current: Option<usize>,
    error: Option<String>,
    task: Option<Task<()>>,
    text_subscription: Option<Subscription>,
    _input_subscription: Subscription,
}

impl EventEmitter<ReviewDiffAction> for ReviewDiffDocument {}

impl ReviewDiffDocument {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        if !cx.has_global::<FindBindings>() {
            cx.set_global(FindBindings);
            cx.bind_keys([KeyBinding::new(
                if cfg!(target_os = "macos") {
                    "cmd-f"
                } else {
                    "ctrl-f"
                },
                FindInDiff,
                Some("ReviewDiff"),
            )]);
            #[cfg(target_family = "wasm")]
            cx.bind_keys([KeyBinding::new("cmd-f", FindInDiff, Some("ReviewDiff"))]);
            cx.bind_keys([KeyBinding::new("escape", Escape, Some("ReviewDiff"))]);
        }
        let input = cx.new(|cx| InputState::new(window, cx).placeholder("Find in diff…"));
        let subscription = cx.subscribe(&input, |this, input, event, cx| {
            if matches!(event, InputEvent::Change) {
                this.query = input.read(cx).value().to_string();
                this.reveal_after_search = true;
                this.schedule_search(cx);
            }
        });
        Self {
            text: cx.new(|cx| TextViewState::markdown("", cx)),
            input,
            focus: cx.focus_handle(),
            scroll: ScrollHandle::new(),
            reveal_enabled: Rc::new(Cell::new(false)),
            reveal_after_search: false,
            restore_offset: None,
            state: DocumentState::Loading,
            ignore_whitespace: false,
            open: false,
            query: String::new(),
            generation: 0,
            snapshot: None,
            waiting_for_parse: false,
            pending: false,
            matches: Matches::default(),
            current: None,
            error: None,
            task: None,
            text_subscription: None,
            _input_subscription: subscription,
        }
    }

    fn invalidate(&mut self, cx: &mut Context<Self>) {
        self.reveal_enabled.set(false);
        self.generation = self.generation.wrapping_add(1);
        self.task = None;
        self.pending = false;
        self.matches = Matches::default();
        self.current = None;
        self.error = None;
        self.text
            .update(cx, |text, cx| text.clear_range_highlights(cx));
    }

    pub fn reset(&mut self, cx: &mut Context<Self>) {
        self.invalidate(cx);
        self.open = false;
        self.query.clear();
        self.snapshot = None;
        self.text_subscription = None;
        self.text = cx.new(|cx| TextViewState::markdown("", cx));
        self.waiting_for_parse = false;
        self.scroll = ScrollHandle::new();
        self.restore_offset = None;
        self.reveal_after_search = false;
        self.state = DocumentState::Loading;
        cx.notify();
    }

    pub fn loading(&mut self, ignore_whitespace: bool, cx: &mut Context<Self>) {
        if matches!(self.state, DocumentState::Ready { .. }) {
            self.restore_offset = Some(self.scroll.offset());
        }
        self.reveal_after_search = false;
        self.invalidate(cx);
        self.snapshot = None;
        self.text_subscription = None;
        self.waiting_for_parse = false;
        self.state = DocumentState::Loading;
        self.ignore_whitespace = ignore_whitespace;
        cx.notify();
    }

    pub fn failed(&mut self, error: String, cx: &mut Context<Self>) {
        self.loading(self.ignore_whitespace, cx);
        self.state = DocumentState::Failed(error);
        cx.notify();
    }

    pub fn set_patch(&mut self, patch: &str, cx: &mut Context<Self>) {
        self.invalidate(cx);
        self.text_subscription = None;
        let markdown = patch_markdown(patch);
        self.text = cx.new(|cx| TextViewState::markdown("", cx));
        let baseline = self.text.read(cx).rendered_text();
        self.text
            .update(cx, |text, cx| text.set_text(&markdown, cx));
        self.state = DocumentState::Ready {
            empty: patch.is_empty(),
        };
        self.snapshot = Some(self.text.read(cx).rendered_text());
        self.waiting_for_parse = !patch.is_empty() && self.snapshot.as_ref() == Some(&baseline);
        self.text_subscription = Some(cx.observe(&self.text, |this, text, cx| {
            let snapshot = text.read(cx).rendered_text();
            if this.snapshot.as_ref() != Some(&snapshot) {
                this.snapshot = Some(snapshot);
                this.waiting_for_parse = false;
                this.schedule_search(cx);
            }
        }));
        if !self.waiting_for_parse {
            self.schedule_search(cx);
        }
        cx.notify();
    }

    fn schedule_search(&mut self, cx: &mut Context<Self>) {
        self.invalidate(cx);
        if !self.open
            || self.query.is_empty()
            || self.waiting_for_parse
            || !matches!(self.state, DocumentState::Ready { .. })
        {
            cx.notify();
            return;
        }
        let Some(snapshot) = self.snapshot.clone() else {
            return;
        };
        let generation = self.generation;
        let query = self.query.clone();
        self.pending = true;
        self.task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(120))
                .await;
            let searched = snapshot.clone();
            let result = cx
                .background_executor()
                .spawn(async move { find_matches(searched.as_str(), &query) })
                .await;
            let _ = this.update(cx, |this, cx| {
                if this.generation != generation
                    || !this.open
                    || this.snapshot.as_ref() != Some(&snapshot)
                    || this.text.read(cx).rendered_text() != snapshot
                {
                    return;
                }
                this.pending = false;
                match result {
                    Ok(matches) => {
                        this.current = (!matches.ranges.is_empty()).then_some(0);
                        this.matches = matches;
                    }
                    Err(error) => this.error = Some(error),
                }
                // A refresh publishes counts but never moves the reader.
                this.paint_matches(cx);
                if this.reveal_after_search {
                    this.reveal_current(cx);
                }
                cx.notify();
            });
        }));
        cx.notify();
    }

    fn paint_matches(&self, cx: &mut Context<Self>) {
        let muted = cx.theme().primary.opacity(0.16);
        let active = cx.theme().primary.opacity(0.38);
        let highlights = self
            .matches
            .ranges
            .iter()
            .enumerate()
            .map(|(index, range)| {
                RangeHighlight::new(
                    range.clone(),
                    if self.current == Some(index) {
                        active
                    } else {
                        muted
                    },
                )
            })
            .collect::<Vec<_>>();
        self.text.update(cx, |text, cx| {
            let _ = text.set_range_highlights(highlights, cx);
        });
    }

    fn navigate(&mut self, previous: bool, cx: &mut Context<Self>) {
        if self.pending
            || self.waiting_for_parse
            || self.snapshot.as_ref() != Some(&self.text.read(cx).rendered_text())
        {
            return;
        }
        self.current = step_match(self.current, self.matches.ranges.len(), previous);
        self.paint_matches(cx);
        self.reveal_current(cx);
        cx.notify();
    }

    fn reveal_current(&mut self, cx: &mut Context<Self>) {
        if let Some(index) = self.current {
            self.reveal_enabled.set(true);
            let range = self.matches.ranges[index].clone();
            self.text.update(cx, |text, cx| {
                let _ = text.reveal_range(range, cx);
            });
        }
    }

    fn open_find(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.open {
            self.open = true;
            self.input
                .update(cx, |input, cx| input.set_value(&self.query, window, cx));
            self.schedule_search(cx);
        }
        self.input.update(cx, |input, cx| input.focus(window, cx));
        cx.notify();
    }

    fn close_find(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.invalidate(cx);
        self.open = false;
        self.query.clear();
        window.focus(&self.focus, cx);
        cx.notify();
    }

    fn status(&self) -> String {
        if matches!(self.state, DocumentState::Loading) || self.waiting_for_parse {
            return "Updating diff…".into();
        }
        if matches!(self.state, DocumentState::Failed(_)) {
            return "Could not load diff".into();
        }
        if self.query.is_empty() {
            return "Type to find in this diff".into();
        }
        if self.pending {
            return "Searching…".into();
        }
        if let Some(error) = &self.error {
            return error.clone();
        }
        let count = self.matches.ranges.len();
        if count == 0 {
            return "No matches in this diff".into();
        }
        let status = match self.current {
            Some(index) => format!("{} of {count} matches", index + 1),
            None => format!("{count} matches · Choose Previous or Next"),
        };
        if self.matches.capped {
            format!("{status} · First 10,000 matches; refine your search")
        } else {
            status
        }
    }
}

impl Render for ReviewDiffDocument {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if matches!(self.state, DocumentState::Ready { .. }) && !self.waiting_for_parse {
            if let Some(offset) = self.restore_offset.take() {
                let snapshot = self.snapshot.clone();
                cx.on_next_frame(window, move |this, _, cx| {
                    if this.snapshot == snapshot && !this.reveal_after_search {
                        this.scroll.set_offset(offset);
                        cx.notify();
                    }
                });
            }
        }
        let can_navigate =
            !self.pending && !self.waiting_for_parse && !self.matches.ranges.is_empty();
        let content = match &self.state {
            DocumentState::Loading => ReviewDiffContent::Loading,
            DocumentState::Failed(error) => ReviewDiffContent::Failed(error),
            DocumentState::Ready { empty: true } => ReviewDiffContent::Empty,
            DocumentState::Ready { empty: false } => ReviewDiffContent::Ready(&self.text),
        };
        let hint = if cfg!(target_os = "macos") {
            "Find in diff (⌘F)"
        } else {
            "Find in diff (Ctrl+F)"
        };
        div()
            .id("review-diff-document")
            .key_context("ReviewDiff")
            .track_focus(&self.focus)
            .role(Role::Group)
            .aria_label("Review diff")
            .flex()
            .flex_col()
            .flex_1()
            .min_h_0()
            .min_w_0()
            .on_action(cx.listener(|this, _: &FindInDiff, window, cx| this.open_find(window, cx)))
            .on_action(cx.listener(|this, _: &Escape, window, cx| {
                if this.open {
                    this.close_find(window, cx);
                } else {
                    cx.propagate();
                }
            }))
            .child(
                div()
                    .flex()
                    .items_center()
                    .flex_wrap()
                    .gap_2()
                    .px_3()
                    .py_1()
                    .child(
                        Button::new("review-find-open")
                            .debug_selector(|| "review-find-open".into())
                            .label("Find in diff")
                            .ghost()
                            .small()
                            .tooltip(hint)
                            .accessibility_label(hint)
                            .on_click(
                                cx.listener(|this, _, window, cx| this.open_find(window, cx)),
                            ),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(cx.theme().muted_foreground)
                            .child("Current diff only"),
                    ),
            )
            .when(self.open, |body| {
                body.child(
                    div()
                        .id("review-find-strip")
                        .debug_selector(|| "review-find-strip".into())
                        .flex()
                        .flex_col()
                        .flex_none()
                        .min_w_0()
                        .gap_2()
                        .px_3()
                        .py_2()
                        .border_b_1()
                        .border_color(cx.theme().border)
                        .on_action(cx.listener(|this, action: &Enter, window, cx| {
                            let composing = this.input.update(cx, |input, cx| {
                                input.marked_text_range(window, cx).is_some()
                            });
                            if !composing {
                                this.navigate(action.shift, cx);
                            }
                        }))
                        .child(
                            div()
                                .flex()
                                .flex_wrap()
                                .items_center()
                                .gap_2()
                                .child(div().flex_1().min_w(rems(9.)).child(
                                    Input::new(&self.input).small().aria_label("Find in diff"),
                                ))
                                .child(
                                    Button::new("review-find-previous")
                                        .debug_selector(|| "review-find-previous".into())
                                        .label("Previous")
                                        .ghost()
                                        .small()
                                        .disabled(!can_navigate)
                                        .tooltip("Previous match (Shift+Enter)")
                                        .accessibility_label("Previous match (Shift+Enter)")
                                        .on_click(
                                            cx.listener(|this, _, _, cx| this.navigate(true, cx)),
                                        ),
                                )
                                .child(
                                    Button::new("review-find-next")
                                        .debug_selector(|| "review-find-next".into())
                                        .label("Next")
                                        .ghost()
                                        .small()
                                        .disabled(!can_navigate)
                                        .tooltip("Next match (Enter)")
                                        .accessibility_label("Next match (Enter)")
                                        .on_click(
                                            cx.listener(|this, _, _, cx| this.navigate(false, cx)),
                                        ),
                                )
                                .child(
                                    Button::new("review-find-close")
                                        .debug_selector(|| "review-find-close".into())
                                        .label("Close")
                                        .ghost()
                                        .small()
                                        .tooltip("Close find (Escape)")
                                        .accessibility_label("Close find (Escape)")
                                        .on_click(cx.listener(|this, _, window, cx| {
                                            this.close_find(window, cx)
                                        })),
                                ),
                        )
                        .child(
                            div()
                                .id("review-find-status")
                                .role(Role::Status)
                                .aria_label(self.status())
                                .text_sm()
                                .child(self.status()),
                        ),
                )
            })
            .child(crate::review::review_diff_body_scrolled(
                content,
                self.ignore_whitespace,
                "review-diff-scroll".into(),
                Some(&self.scroll),
                self.reveal_enabled.clone(),
                cx.listener(|_, action: &ReviewDiffAction, _, cx| cx.emit(*action)),
                cx,
            ))
    }
}

#[cfg(test)]
mod tests {
    use super::{find_matches, patch_markdown, step_match, ReviewDiffDocument, MATCH_LIMIT};
    use gpui::{
        div, AppContext, Context, Entity, IntoElement, ParentElement, Render, Styled,
        TestAppContext, VisualTestContext, Window,
    };

    struct ReviewFindHarness(Entity<ReviewDiffDocument>);

    impl Render for ReviewFindHarness {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div().size_full().flex().flex_col().child(self.0.clone())
        }
    }

    fn settle(cx: &mut VisualTestContext) {
        cx.run_until_parked();
        cx.executor()
            .advance_clock(std::time::Duration::from_secs(1));
        cx.executor().tick();
        cx.run_until_parked();
        cx.update(|window, cx| {
            window.refresh();
            window.draw(cx).clear(cx);
            window.simulate_next_frame(cx);
        });
    }

    #[gpui::test]
    fn review_find_reveals_offscreen_and_preserves_scroll_on_refresh(cx: &mut TestAppContext) {
        cx.update(gpui_component::init);
        let patch = format!(
            "diff --git a/long.rs b/long.rs\n{}-__offscreen__ café\n",
            " context text\n".repeat(600)
        );
        let initial = patch.clone();
        let saved = std::rc::Rc::new(std::cell::RefCell::new(None));
        let capture = saved.clone();
        let (_, cx) = cx.add_window_view(move |window, cx| {
            let document = cx.new(|cx| ReviewDiffDocument::new(window, cx));
            document.update(cx, |document, cx| {
                document.set_patch(&initial, cx);
                document.open_find(window, cx);
            });
            *capture.borrow_mut() = Some(document.clone());
            let harness = cx.new(|_| ReviewFindHarness(document));
            gpui_component::Root::new(harness, window, cx)
        });
        let document = saved.borrow_mut().take().unwrap();
        cx.simulate_resize(gpui::size(gpui::px(400.0), gpui::px(500.0)));
        settle(cx);
        cx.simulate_input("__offscreen__");
        settle(cx);
        settle(cx);
        let offset = document.read_with(cx, |document, _| {
            assert_eq!(document.status(), "1 of 1 matches");
            document.scroll.offset()
        });
        assert!(
            offset.y < gpui::px(-100.0),
            "active match should move the enclosing scroll container: {offset:?}"
        );
        document.update(cx, |document, cx| document.loading(false, cx));
        settle(cx);
        document.update(cx, |document, cx| document.set_patch(&patch, cx));
        settle(cx);
        settle(cx);
        document.read_with(cx, |document, _| {
            assert_eq!(document.scroll.offset(), offset);
            assert!(!document.reveal_enabled.get());
        });
    }

    #[gpui::test]
    fn review_find_rendered_revision_keyboard_refresh_and_reset(cx: &mut TestAppContext) {
        cx.update(gpui_component::init);
        let saved = std::rc::Rc::new(std::cell::RefCell::new(None));
        let capture = saved.clone();
        let (_, cx) = cx.add_window_view(move |window, cx| {
            let document = cx.new(|cx| ReviewDiffDocument::new(window, cx));
            document.update(cx, |document, cx| {
                document.set_patch(
                    "diff --git a/a.rs b/a.rs\n@@ -1 +1 @@\n-café CAFÉ\n+café\n+```literal```",
                    cx,
                );
                document.open_find(window, cx);
            });
            *capture.borrow_mut() = Some(document.clone());
            gpui_component::Root::new(document, window, cx)
        });
        let document = saved.borrow_mut().take().unwrap();
        settle(cx);
        cx.simulate_input("café");
        settle(cx);
        document.read_with(cx, |document, cx| {
            assert_eq!(document.status(), "1 of 3 matches");
            let rendered = document.text.read(cx).rendered_text();
            assert!(rendered.as_str().contains("-café CAFÉ"));
            assert!(rendered.as_str().contains("+```literal```"));
            assert!(!rendered.as_str().contains("````diff"));
            assert_eq!(document.snapshot.as_ref(), Some(&rendered));
        });
        cx.simulate_keystrokes("shift-enter");
        assert_eq!(
            document.read_with(cx, |document, _| document.current),
            Some(2)
        );
        cx.simulate_keystrokes("enter");
        assert_eq!(
            document.read_with(cx, |document, _| document.current),
            Some(0)
        );
        document.update(cx, |document, cx| {
            document.loading(true, cx);
            assert_eq!(document.query, "café");
            assert_eq!(document.status(), "Updating diff…");
            assert!(document.matches.ranges.is_empty());
            assert!(!document.reveal_enabled.get());
            document.set_patch("-café\n+other", cx);
        });
        settle(cx);
        document.read_with(cx, |document, _| {
            assert_eq!(document.status(), "1 of 1 matches");
            assert!(
                !document.reveal_enabled.get(),
                "background refresh must not reveal"
            );
        });
        // Replacing a query before its debounce fires cannot publish its ranges.
        cx.simulate_input("missing");
        document.update(cx, |document, cx| document.reset(cx));
        settle(cx);
        document.read_with(cx, |document, _| {
            assert!(!document.open);
            assert!(document.query.is_empty());
            assert!(document.matches.ranges.is_empty());
        });
        cx.update(|window, cx| {
            document.update(cx, |document, cx| {
                document.set_patch("Binary files a/logo.png and b/logo.png differ", cx);
                document.open_find(window, cx);
            })
        });
        cx.simulate_input("missing");
        settle(cx);
        assert_eq!(
            document.read_with(cx, |document, _| document.status()),
            "No matches in this diff"
        );
        document.update(cx, |document, cx| document.failed("offline".into(), cx));
        assert_eq!(
            document.read_with(cx, |document, _| document.status()),
            "Could not load diff"
        );
        cx.simulate_keystrokes("escape");
        cx.update(|window, cx| {
            assert!(!document.read(cx).open);
            assert!(document.read(cx).focus.is_focused(window));
        });
        #[cfg(target_os = "macos")]
        cx.simulate_keystrokes("cmd-f");
        #[cfg(not(target_os = "macos"))]
        cx.simulate_keystrokes("ctrl-f");
        assert!(document.read_with(cx, |document, _| document.open));
    }

    #[test]
    fn review_find_literal_unicode_and_occurrences() {
        let text = "diff --git a/Σ.rs b/ς.rs\r\n-old café CAFÉ\r\n+new café\n café\n+[a.b] 🦀\n";
        let matches = find_matches(text, "CAFÉ").unwrap();
        assert_eq!(matches.ranges.len(), 4);
        for range in matches.ranges {
            assert!(text.is_char_boundary(range.start) && text.is_char_boundary(range.end));
        }
        assert_eq!(find_matches(text, "σ").unwrap().ranges.len(), 2);
        assert_eq!(find_matches(text, "[a.b]").unwrap().ranges.len(), 1);
        assert_eq!(find_matches(text, "🦀").unwrap().ranges.len(), 1);
        assert_eq!(find_matches(text, "\r\n").unwrap().ranges.len(), 2);
        assert!(find_matches(text, "missing").unwrap().ranges.is_empty());
        assert!(find_matches(text, "").unwrap().ranges.is_empty());
    }

    #[test]
    fn review_find_cap_and_wrap() {
        let found = find_matches(&"x ".repeat(MATCH_LIMIT + 1), "x").unwrap();
        assert!(found.capped);
        assert_eq!(found.ranges.len(), MATCH_LIMIT);
        assert!(!find_matches(&"x ".repeat(MATCH_LIMIT), "x").unwrap().capped);
        assert_eq!(step_match(None, 0, false), None);
        assert_eq!(step_match(None, 3, true), Some(2));
        assert_eq!(step_match(Some(2), 3, false), Some(0));
        assert_eq!(step_match(Some(0), 3, true), Some(2));
        assert!(patch_markdown("+```literal```").starts_with("````diff\n+```literal```"));
    }
}
