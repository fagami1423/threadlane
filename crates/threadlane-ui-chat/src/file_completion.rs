use super::*;

use threadlane_git::{list_project_files, FileInventoryError, GitFileInventory};

actions!(
    threadlane_file_completion,
    [
        CompleteFileCompletion,
        SelectPreviousFileCompletion,
        SelectNextFileCompletion,
        DismissFileCompletion,
    ]
);

pub(super) fn init_file_completion(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new(
            "tab",
            CompleteFileCompletion,
            Some(FILE_COMPLETION_BINDING_CONTEXT),
        ),
        KeyBinding::new(
            "up",
            SelectPreviousFileCompletion,
            Some(FILE_COMPLETION_BINDING_CONTEXT),
        ),
        KeyBinding::new(
            "down",
            SelectNextFileCompletion,
            Some(FILE_COMPLETION_BINDING_CONTEXT),
        ),
        KeyBinding::new(
            "escape",
            DismissFileCompletion,
            Some(FILE_COMPLETION_BINDING_CONTEXT),
        ),
    ]);
}

pub(super) enum FileCompletionStatus {
    /// `git ls-files` is still running for the resolved root.
    Loading,
    /// The resolved root is not usable for completion; `String` explains why
    /// (no project, preparing/missing worktree, or not a Git workspace).
    Unsupported(String),
    /// Git ran but failed — never rendered as "No matching files".
    Failed(String),
    Ready(GitFileInventory),
}

/// The picker's transient state for one resolved root under one composer.
/// Root and composer key are captured at open time; a session, project, or
/// draft switch clears the whole state instead of reusing stale results.
pub(super) struct FileCompletionState {
    /// The Git checkout `paths` were enumerated from. `None` when the root is
    /// unsupported — never silently retargeted to another checkout.
    pub(super) root: Option<PathBuf>,
    pub(super) composer_key: ComposerKey,
    pub(super) status: FileCompletionStatus,
}

impl ChatListView {
    /// The caret-local `@` trigger, or `None` while completion is suppressed:
    /// away from the chat tab, on a non-collapsed selection, or inside IME
    /// composition (`cursor()` then reports the marked range end, which the
    /// collapsed selection does not match).
    pub(super) fn current_file_trigger(&self, cx: &App) -> Option<FileQueryTrigger> {
        if self.current_tab != CentralTab::Chat {
            return None;
        }
        let input = self.input_state.read(cx);
        let selection = input.selected_range();
        if selection.start != selection.end {
            return None;
        }
        let caret = input.cursor();
        if caret != selection.end {
            return None;
        }
        active_file_query(&input.value(), caret)
    }

    /// Whether the picker is open — an active trigger that was not dismissed.
    /// Open in any state (loading/empty/error/unsupported) means Enter/Tab are
    /// owned by the picker and must never reach Send/Queue/Steer.
    pub(super) fn file_menu_open(&self, cx: &App) -> bool {
        !self.dismiss_file_menu && self.current_file_trigger(cx).is_some()
    }

    /// Open, refresh, or close the picker for the current trigger. Called from
    /// the composer `Change` subscription and deferred when a trigger exists
    /// without state (e.g. a restored draft ending in `@`).
    pub(super) fn sync_file_completion(&mut self, cx: &mut Context<Self>) {
        if self.current_file_trigger(cx).is_none() {
            if self.file_completion.is_some() {
                self.clear_file_completion();
                cx.notify();
            }
            return;
        }
        let composer_key = self.composer_key.clone();
        let root = self.model.read(cx).active_git_work_dir();
        let keep = self.file_completion.as_ref().is_some_and(|state| {
            state.composer_key == composer_key && state.root == root
        });
        if keep {
            return;
        }
        match root {
            Some(root) => self.request_file_inventory(root, cx),
            None => {
                self.file_completion_generation =
                    self.file_completion_generation.wrapping_add(1);
                self.file_completion_task = None;
                self.file_completion = Some(FileCompletionState {
                    root: None,
                    composer_key,
                    status: FileCompletionStatus::Unsupported(
                        self.file_scope_unavailable_reason(cx),
                    ),
                });
                self.selected_file_index = 0;
                cx.notify();
            }
        }
    }

    /// Enumerate `root` off the UI thread. Results apply only if the
    /// generation, composer key, and resolved root are all still current.
    fn request_file_inventory(&mut self, root: PathBuf, cx: &mut Context<Self>) {
        self.file_completion_generation = self.file_completion_generation.wrapping_add(1);
        let generation = self.file_completion_generation;
        let composer_key = self.composer_key.clone();
        self.file_completion_task = None;
        self.file_completion = Some(FileCompletionState {
            root: Some(root.clone()),
            composer_key: composer_key.clone(),
            status: FileCompletionStatus::Loading,
        });
        self.selected_file_index = 0;
        self.file_scroll_handle.scroll_to_item(0);
        cx.notify();
        let task_root = root.clone();
        self.file_completion_task = Some(cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { list_project_files(&task_root) })
                .await;
            let _ = this.update(cx, |this, cx| {
                if this.file_completion_generation != generation
                    || this.composer_key != composer_key
                    || this.model.read(cx).active_git_work_dir().as_deref()
                        != Some(root.as_path())
                {
                    return;
                }
                this.file_completion_task = None;
                this.selected_file_index = 0;
                this.file_completion = Some(FileCompletionState {
                    root: Some(root),
                    composer_key,
                    status: match result {
                        Ok(inventory) => FileCompletionStatus::Ready(inventory),
                        Err(FileInventoryError::NotARepository) => {
                            FileCompletionStatus::Unsupported(
                                "File completion requires a Git workspace".to_string(),
                            )
                        }
                        Err(FileInventoryError::Failed(error)) => {
                            FileCompletionStatus::Failed(error.message)
                        }
                    },
                });
                cx.notify();
            });
        }));
    }

    pub(super) fn clear_file_completion(&mut self) {
        self.file_completion = None;
        self.file_completion_generation = self.file_completion_generation.wrapping_add(1);
        self.file_completion_task = None;
        self.selected_file_index = 0;
        self.file_scroll_handle.scroll_to_item(0);
        self.dismiss_file_menu = false;
    }

    /// Why `@` completion cannot search right now. Each case is named so the
    /// user can act (attach a project, wait for the worktree, use a Git
    /// workspace); never claims "no matches".
    fn file_scope_unavailable_reason(&self, cx: &App) -> String {
        let state = self.model.read(cx);
        if state.active_work_dir.is_none() {
            "Attach a project to search files".to_string()
        } else if state.active_worktree_setup().is_some() {
            "Preparing the session workspace…".to_string()
        } else if state.active_worktree_unavailable() {
            "This session's worktree is unavailable; file completion is off".to_string()
        } else {
            "File completion requires a Git workspace".to_string()
        }
    }

    /// Rows for `query` from the current inventory, plus whether more matches
    /// exist beyond the cap. Empty for any non-Ready status.
    fn file_completion_matches(&self, query: &str) -> (Vec<String>, bool) {
        let Some(FileCompletionState {
            status: FileCompletionStatus::Ready(inventory),
            ..
        }) = &self.file_completion
        else {
            return (Vec::new(), false);
        };
        let (matches, has_more) =
            filter_file_matches(query, &inventory.paths, FILE_COMPLETION_RESULT_LIMIT);
        (matches.into_iter().cloned().collect(), has_more)
    }

    /// Replace the live trigger range with `path` as a Markdown code span.
    /// Draft, composer key, resolved root, live trigger, membership in the
    /// current match list, and on-disk existence are all re-checked at apply
    /// time so a late click or session switch can never edit unexpected text.
    fn apply_file_completion(&mut self, path: &str, window: &mut Window, cx: &mut Context<Self>) {
        let Some(state) = &self.file_completion else {
            return;
        };
        if state.composer_key != self.composer_key {
            self.clear_file_completion();
            return;
        }
        let Some(root) = state.root.clone() else {
            return;
        };
        if self.model.read(cx).active_git_work_dir().as_deref() != Some(root.as_path()) {
            self.clear_file_completion();
            cx.notify();
            return;
        }
        let Some(trigger) = self.current_file_trigger(cx) else {
            return;
        };
        let (matches, _) = self.file_completion_matches(&trigger.query);
        if !matches.iter().any(|candidate| candidate == path) {
            return;
        }
        // `symlink_metadata` does not follow links; it only checks that the
        // repo-relative name still exists inside the resolved root. A file
        // deleted since enumeration refreshes the list instead of inserting.
        let exists = is_safe_relative_path(path) && std::fs::symlink_metadata(root.join(path)).is_ok();
        if !exists {
            self.request_file_inventory(root, cx);
            return;
        }
        let insertion = format_path_insertion(path);
        self.input_state.update(cx, |input, cx| {
            input.set_selected_range(trigger.range.clone(), cx);
            input.replace(insertion, window, cx);
            input.focus(window, cx);
        });
        self.clear_file_completion();
        cx.notify();
    }

    /// Enter/Tab: insert the current result if one is valid. `true` means the
    /// picker consumed the key — callers must not fall through to submission.
    /// A `false` return with the picker open means there is no valid result:
    /// Enter stays swallowed by the caller, Tab may leave the list.
    pub(super) fn apply_selected_file_completion(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(trigger) = self.current_file_trigger(cx) else {
            return false;
        };
        let (matches, _) = self.file_completion_matches(&trigger.query);
        if matches.is_empty() {
            return false;
        }
        let index = self.selected_file_index.min(matches.len() - 1);
        let path = matches[index].clone();
        self.apply_file_completion(&path, window, cx);
        true
    }

    fn file_match_count(&self, cx: &App) -> usize {
        let Some(trigger) = self.current_file_trigger(cx) else {
            return 0;
        };
        self.file_completion_matches(&trigger.query).0.len()
    }

    pub(super) fn select_previous_file_completion_action(
        &mut self,
        _: &SelectPreviousFileCompletion,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let count = self.file_match_count(cx);
        if count == 0 {
            return;
        }
        self.selected_file_index = if self.selected_file_index == 0 {
            count - 1
        } else {
            self.selected_file_index - 1
        };
        self.file_scroll_handle
            .scroll_to_item(self.selected_file_index);
        cx.notify();
    }

    pub(super) fn select_next_file_completion_action(
        &mut self,
        _: &SelectNextFileCompletion,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let count = self.file_match_count(cx);
        if count == 0 {
            return;
        }
        self.selected_file_index = (self.selected_file_index + 1) % count;
        self.file_scroll_handle
            .scroll_to_item(self.selected_file_index);
        cx.notify();
    }

    pub(super) fn complete_file_completion_action(
        &mut self,
        _: &CompleteFileCompletion,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.dismiss_file_menu {
            return;
        }
        if !self.apply_selected_file_completion(window, cx) {
            // Tab may leave the list without inserting when nothing is valid.
            cx.propagate();
        }
    }

    pub(super) fn dismiss_file_completion_action(
        &mut self,
        _: &DismissFileCompletion,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.dismiss_file_menu = true;
        cx.notify();
    }

    /// Same keys as the `FileCompletionMenu > Input` bindings for events that
    /// bubble to the composer container instead of dispatching an action.
    pub(super) fn handle_file_completion_key_down(
        &mut self,
        key: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if !self.file_menu_open(cx) {
            return false;
        }
        match key {
            "escape" => {
                // Keep the typed `@query`; only close the list. Stopping
                // propagation keeps Escape away from global cancel handlers.
                self.dismiss_file_menu = true;
                cx.stop_propagation();
                cx.notify();
            }
            "up" => {
                self.select_previous_file_completion_action(
                    &SelectPreviousFileCompletion,
                    window,
                    cx,
                );
                cx.stop_propagation();
            }
            "down" => {
                self.select_next_file_completion_action(
                    &SelectNextFileCompletion,
                    window,
                    cx,
                );
                cx.stop_propagation();
            }
            "tab" => {
                if self.apply_selected_file_completion(window, cx) {
                    cx.stop_propagation();
                }
            }
            "enter" => {
                // Never fall through to Send/Queue/Steer while the list is
                // open, in any state.
                cx.stop_propagation();
            }
            _ => return false,
        }
        true
    }

    fn render_file_menu_row(
        &self,
        theme: &gpui_component::theme::ThemeColor,
        path: &str,
        index: usize,
        is_active: bool,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let basename = path.rsplit('/').next().unwrap_or(path);
        let parent = path.strip_suffix(basename).unwrap_or("");
        let selected_path = path.to_owned();
        div()
            .id(SharedString::from(format!("composer-file-{index}")))
            .role(Role::Button)
            .aria_label(format!("{path} — insert this path at the caret"))
            .h(rems(1.875))
            .flex()
            .items_center()
            .gap_2()
            .rounded_md()
            .px_2()
            .text_sm()
            .bg(if is_active {
                theme.accent.opacity(0.16)
            } else {
                gpui::transparent_black()
            })
            .hover(|style| style.bg(theme.list_hover))
            .child(
                Icon::default()
                    .path("icons/file.svg")
                    .small()
                    .text_color(if is_active {
                        theme.primary
                    } else {
                        theme.muted_foreground
                    }),
            )
            .child(
                div()
                    .flex_none()
                    .truncate()
                    .font_weight(if is_active {
                        FontWeight::BOLD
                    } else {
                        FontWeight::SEMIBOLD
                    })
                    .text_color(if is_active {
                        theme.primary
                    } else {
                        theme.foreground
                    })
                    .child(basename.to_owned()),
            )
            .child(
                div()
                    .min_w_0()
                    .overflow_hidden()
                    .text_color(theme.muted_foreground)
                    .child(parent.to_owned()),
            )
            .on_click(cx.listener(move |this, _event, window, cx| {
                this.apply_file_completion(&selected_path, window, cx);
            }))
    }

    /// The `@` picker popup: header names the workspace scope and the hint row
    /// counts the active result; the list is `Role::List` with an aria label
    /// that announces scope plus the selected path, and every status state
    /// (loading / empty / failed / unsupported / capped) is visible text.
    pub(super) fn render_file_menu(
        &mut self,
        trigger: &FileQueryTrigger,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = cx.theme().colors;
        let (matches, has_more) = self.file_completion_matches(&trigger.query);
        let match_count = matches.len();
        let selected_idx = self.selected_file_index.min(match_count.saturating_sub(1));
        let scope = self
            .file_completion
            .as_ref()
            .and_then(|state| state.root.clone())
            .and_then(|root| root.file_name().map(|name| name.to_string_lossy().into_owned()))
            .unwrap_or_else(|| "workspace".to_string());
        let selected = matches
            .get(selected_idx)
            .cloned()
            .unwrap_or_else(|| "none".to_string());
        let non_utf8_note = self
            .file_completion
            .as_ref()
            .and_then(|state| match &state.status {
                FileCompletionStatus::Ready(inventory) if inventory.non_utf8_skipped > 0 => {
                    Some(format!(
                        ", {} non-UTF-8 names skipped",
                        inventory.non_utf8_skipped
                    ))
                }
                _ => None,
            })
            .unwrap_or_default();
        let list_label = match self.file_completion.as_ref().map(|state| &state.status) {
            None | Some(FileCompletionStatus::Loading) => {
                format!("Searching files in {scope}")
            }
            Some(FileCompletionStatus::Unsupported(reason)) => {
                format!("File completion unavailable: {reason}")
            }
            Some(FileCompletionStatus::Failed(error)) => {
                format!("File search failed in {scope}: {error}")
            }
            Some(FileCompletionStatus::Ready(_)) => format!(
                "Files in {scope}, {} of {match_count}, {selected} selected{non_utf8_note}",
                if match_count == 0 {
                    0
                } else {
                    selected_idx + 1
                }
            ),
        };

        let body: AnyElement = match self.file_completion.as_ref().map(|state| &state.status) {
            None | Some(FileCompletionStatus::Loading) => {
                file_menu_status_row(&theme, "Searching files…").into_any_element()
            }
            Some(FileCompletionStatus::Unsupported(reason)) => {
                file_menu_status_row(&theme, reason.clone()).into_any_element()
            }
            Some(FileCompletionStatus::Failed(error)) => div()
                .flex()
                .items_center()
                .gap_2()
                .h(rems(1.875))
                .px_2()
                .child(
                    div()
                        .min_w_0()
                        .flex_1()
                        .truncate()
                        .text_sm()
                        .text_color(theme.muted_foreground)
                        .child(format!("Could not list files: {error}")),
                )
                .child(
                    Button::new("file-completion-retry")
                        .debug_selector(|| "file-completion-retry".into())
                        .label("Retry")
                        .small()
                        .ghost()
                        .accessibility_label("Retry listing workspace files")
                        .on_click(cx.listener(|this, _event, _window, cx| {
                            let root = this
                                .file_completion
                                .as_ref()
                                .and_then(|state| state.root.clone());
                            if let Some(root) = root {
                                this.request_file_inventory(root, cx);
                            }
                        })),
                )
                .into_any_element(),
            Some(FileCompletionStatus::Ready(_)) => {
                if match_count == 0 {
                    file_menu_status_row(&theme, "No matching files".to_string())
                        .into_any_element()
                } else {
                    div()
                        .children(matches.into_iter().enumerate().map(|(index, path)| {
                            self.render_file_menu_row(
                                &theme,
                                &path,
                                index,
                                index == selected_idx,
                                cx,
                            )
                        }))
                        .into_any_element()
                }
            }
        };

        div()
            .absolute()
            .bottom_full()
            .left(rems(0.0))
            .mb_2()
            .w_full()
            .max_w(rems(40.0))
            .max_h(rems(20.0))
            .flex()
            .flex_col()
            .rounded_lg()
            .border_1()
            .border_color(theme.border)
            .bg(theme.title_bar)
            .shadow_xl()
            .p_1p5()
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .h_7()
                    .px_2()
                    .border_b_1()
                    .border_color(theme.border.opacity(0.4))
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .child(
                                div()
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .child(format!("Files · {scope}")),
                            )
                            .child(
                                div()
                                    .text_color(theme.muted_foreground)
                                    .child("↑↓ navigate · Tab/Enter insert · Esc dismiss"),
                            ),
                    )
                    .child(if match_count > 0 {
                        format!("{}/{}", selected_idx + 1, match_count)
                    } else {
                        "0/0".to_string()
                    }),
            )
            .child(
                div()
                    .id("file-completion-list")
                    .debug_selector(|| "file-completion-list".to_string())
                    .role(Role::List)
                    .aria_label(list_label)
                    .relative()
                    .mt_1()
                    .track_scroll(&self.file_scroll_handle)
                    .overflow_y_scroll()
                    .vertical_scrollbar(&self.file_scroll_handle)
                    .max_h(rems(16.25))
                    .child(body)
                    .when(has_more, |list| {
                        list.child(
                            div()
                                .h(rems(1.875))
                                .flex()
                                .items_center()
                                .px_2()
                                .text_sm()
                                .text_color(theme.muted_foreground)
                                .child("More matches — keep typing"),
                        )
                    }),
            )
            .into_any_element()
    }
}

fn file_menu_status_row(
    theme: &gpui_component::theme::ThemeColor,
    text: impl Into<SharedString>,
) -> impl IntoElement {
    div()
        .h(rems(1.875))
        .flex()
        .items_center()
        .px_2()
        .text_sm()
        .text_color(theme.muted_foreground)
        .child(text.into())
}
