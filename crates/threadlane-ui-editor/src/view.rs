use std::path::{Path, PathBuf};

use gpui::*;
use gpui_component::input::{EditorState, InputEvent, TabSize};
use gpui_component::menu::ContextMenuExt;
use gpui_component::text::TextViewState;
use gpui_component::WindowExt;

use threadlane_ui_state::AppState;

actions!(editor, [SaveFile]);

/// How long a save/open status message stays visible before auto-expiring.
const STATUS_MSG_TTL: std::time::Duration = std::time::Duration::from_secs(3);

pub use threadlane_ui_kit::tool_preview::detect_language;


pub struct EditorTab {
    project_dir: PathBuf,
    relative_path: String,
    file_name: String,
    _language: &'static str,
    saved_content: String,
    is_dirty: bool,
    is_diff: bool,
    /// File content loaded off the UI thread, awaiting application to the
    /// editor on the next render (which owns the `Window` that `set_value`
    /// requires). Applied once by `sync_pending_content`, then cleared.
    pending_content: Option<String>,
    pending_line: Option<usize>,
    loading: bool,
    editor_state: Option<Entity<EditorState>>,
    text_view_state: Option<Entity<TextViewState>>,
    markdown_preview: threadlane_ui_kit::MarkdownPreview,
    _subscription: Option<Subscription>,
    /// Re-renders the host whenever the buffer notifies (selection moves
    /// included — the editor emits no `InputEvent` for selection-only
    /// changes, and the add-selection control must track them).
    _observe: Option<Subscription>,
}

#[derive(Clone, Debug)]
enum PendingOpen {
    File {
        project: PathBuf,
        path: String,
        line: Option<usize>,
    },
    Diff { path: String, content: String },
}

pub struct EditorView {
    model: Entity<AppState>,
    tabs: Vec<EditorTab>,
    active_tab_index: Option<usize>,
    pending_open: Option<PendingOpen>,
    status_msg: Option<(String, bool, std::time::Instant)>,
    _subscriptions: Vec<Subscription>,
}

impl EditorView {
    pub fn new(
        model: Entity<AppState>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let model_clone = model.clone();
        let sub = cx.observe(&model_clone, |_this, _model, cx| {
            cx.notify();
        });

        Self {
            model,
            tabs: Vec::new(),
            active_tab_index: None,
            pending_open: None,
            status_msg: None,
            _subscriptions: vec![sub],
        }
    }

    fn has_tabs(&self) -> bool {
        !self.tabs.is_empty()
    }

    pub fn tab_count(&self) -> usize {
        self.tabs.len()
    }

    fn is_active_dirty(&self) -> bool {
        self.active_tab_index
            .and_then(|idx| self.tabs.get(idx))
            .map(|tab| tab.is_dirty && !tab.is_diff)
            .unwrap_or(false)
    }

    fn is_active_diff(&self) -> bool {
        self.active_tab_index
            .and_then(|idx| self.tabs.get(idx))
            .map(|tab| tab.is_diff)
            .unwrap_or(false)
    }

    /// The active tab's buffer entity — `None` for diff/loading documents.
    /// Selection guards still apply; this is the underlying buffer for
    /// command surfaces and tests.
    pub fn active_editor(&self) -> Option<Entity<EditorState>> {
        self.active_tab_index
            .and_then(|index| self.tabs.get(index))
            .and_then(|tab| tab.editor_state.clone())
    }

    /// Reason the **Add selection to chat** command is unavailable for the
    /// active tab — `None` when ready. Diff documents, loading buffers,
    /// files outside the active checkout, and empty or oversized selections
    /// each carry a textual reason.
    pub fn selection_block_reason(&self, cx: &App) -> Option<SharedString> {
        let Some(tab) = self
            .active_tab_index
            .and_then(|index| self.tabs.get(index))
        else {
            return Some("Open a file first".into());
        };
        if tab.markdown_preview.is_active() {
            return Some(threadlane_ui_kit::PREVIEW_SELECTION_REASON.into());
        }
        if tab.is_diff {
            return Some("Diffs can't be added to chat — open the file itself".into());
        }
        if tab.loading || tab.pending_content.is_some() {
            return Some("The file is still loading".into());
        }
        let Some(editor) = tab.editor_state.as_ref() else {
            return Some("The document is not editable".into());
        };
        if self.model.read(cx).active_git_work_dir().as_ref() != Some(&tab.project_dir) {
            return Some("The file is not in the active checkout".into());
        }
        threadlane_ui_kit::editor_excerpt_block_reason(
            threadlane_ui_kit::editor_selection_snapshot(editor.read(cx)).as_ref(),
        )
        .map(SharedString::from)
    }

    /// Activates **Add selection to chat** for the active tab: validates the
    /// selection, captures buffer identity plus the composer destination,
    /// and emits `EditorSelectionRequest` for the parent surface to append.
    /// On any guard failure the reason is shown and nothing is appended.
    pub fn request_add_selection_to_chat(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(reason) = self.selection_block_reason(cx) {
            window.push_notification(reason.to_string(), cx);
            return;
        }
        let Some(tab) = self
            .active_tab_index
            .and_then(|index| self.tabs.get(index))
        else {
            return;
        };
        let Some(editor) = tab.editor_state.clone() else {
            return;
        };
        let Some(snapshot) =
            threadlane_ui_kit::editor_selection_snapshot(editor.read(cx))
        else {
            return;
        };
        let destination = {
            let state = self.model.read(cx);
            (
                state.active_work_dir.clone(),
                state.active_session_id.clone(),
            )
        };
        cx.emit(threadlane_ui_kit::EditorSelectionRequest {
            editor,
            checkout: tab.project_dir.clone(),
            relative_path: tab.relative_path.clone(),
            dirty: tab.is_dirty,
            snapshot,
            destination,
        });
    }

    /// Whether `request` still names the active tab's buffer and the
    /// buffer's live selection is the captured one.
    pub fn selection_request_is_current(
        &self,
        request: &threadlane_ui_kit::EditorSelectionRequest,
        cx: &App,
    ) -> bool {
        let Some(tab) = self
            .active_tab_index
            .and_then(|index| self.tabs.get(index))
        else {
            return false;
        };
        if tab.markdown_preview.is_active()
            || tab.is_diff
            || tab.loading
            || tab.pending_content.is_some()
            || tab.project_dir != request.checkout
            || tab.relative_path != request.relative_path
        {
            return false;
        }
        let Some(editor) = tab.editor_state.as_ref() else {
            return false;
        };
        editor.entity_id() == request.editor.entity_id()
            && editor.read(cx).selected_range() == request.snapshot.byte_range
    }

    fn sync_pending_file(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(pending) = self.pending_open.take() else {
            return;
        };
        match pending {
            PendingOpen::File {
                project,
                path,
                line,
            } => {
                self.open_file_internal(&project, &path, window, cx);
                if let Some(tab) = self
                    .tabs
                    .iter_mut()
                    .find(|tab| tab.project_dir == project && tab.relative_path == path)
                {
                    tab.pending_line = line;
                    if line.is_some() {
                        tab.markdown_preview.show_source();
                    }
                    if let Some(editor) = &tab.editor_state {
                        if tab.markdown_preview.is_active() {
                            return;
                        }
                        editor.update(cx, |editor, cx| editor.focus(window, cx));
                    }
                }
            }
            PendingOpen::Diff { path, content } => {
                self.open_diff_internal(&path, &content, window, cx)
            }
        }
    }

    pub fn open_file(&mut self, project: PathBuf, relative_path: &str, cx: &mut Context<Self>) {
        self.open_file_at_line(project, relative_path, None, cx);
    }

    pub fn open_file_at_line(
        &mut self,
        project: PathBuf,
        relative_path: &str,
        line: Option<usize>,
        cx: &mut Context<Self>,
    ) {
        self.pending_open = Some(PendingOpen::File {
            project,
            line,
            path: relative_path.to_string(),
        });
        cx.notify();
    }

    pub fn open_diff(&mut self, relative_path: &str, content: &str, cx: &mut Context<Self>) {
        self.pending_open = Some(PendingOpen::Diff {
            path: relative_path.to_string(),
            content: content.to_string(),
        });
        cx.notify();
    }

    fn open_diff_internal(
        &mut self,
        relative_path: &str,
        content: &str,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let tab_key = format!("diff:{relative_path}");
        let markdown = format!("```diff\n{}\n```", content.replace("```", "` ` `"));

        if let Some(existing_idx) = self.tabs.iter().position(|t| t.relative_path == tab_key) {
            if let Some(tab) = self.tabs.get_mut(existing_idx) {
                tab.saved_content = content.to_string();
                if let Some(ref text_view) = tab.text_view_state {
                    text_view.update(cx, |state, cx| {
                        state.set_text(&markdown, cx);
                    });
                }
            }
            self.active_tab_index = Some(existing_idx);
            cx.notify();
            return;
        }

        let markdown_state = cx.new(|cx| TextViewState::markdown(&markdown, cx));
        let tab_title = threadlane_ui_kit::editor_tab_title(relative_path, true);

        self.tabs.push(EditorTab {
            project_dir: self
                .model
                .read(cx)
                .active_work_dir
                .clone()
                .unwrap_or_else(|| PathBuf::from(".")),
            relative_path: tab_key,
            file_name: tab_title,
            _language: "diff",
            saved_content: content.to_string(),
            is_dirty: false,
            is_diff: true,
            pending_content: None,
            pending_line: None,
            loading: false,
            editor_state: None,
            markdown_preview: threadlane_ui_kit::MarkdownPreview::new(cx),
            text_view_state: Some(markdown_state),
            _subscription: None,
            _observe: None,
        });

        self.active_tab_index = Some(self.tabs.len() - 1);
        self.status_msg = None;
        cx.notify();
    }

    fn open_file_internal(
        &mut self,
        project_dir: &Path,
        relative_path: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Reuse the tab, but refresh saved content before navigating a clean buffer.
        if let Some(existing_idx) = self
            .tabs
            .iter()
            .position(|t| t.project_dir == project_dir && t.relative_path == relative_path)
        {
            self.active_tab_index = Some(existing_idx);
            if self.tabs[existing_idx].is_dirty {
                self.set_status("Unsaved buffer preserved; saved-file line numbers may differ.".into(), false);
            } else if !self.tabs[existing_idx].loading
                && self.tabs[existing_idx].pending_content.is_none()
            {
                self.start_file_read(existing_idx, cx);
            }
            cx.notify();
            return;
        }

        let lang = detect_language(relative_path);
        // The tab (and its editor entity, which needs a Window) is created
        // synchronously with a loading placeholder; only the filesystem read
        // moves to the background executor so a large file never stalls the
        // UI thread. Content fills in when the read completes.
        let editor = cx.new(|cx| {
            EditorState::new(window, cx)
                .language(lang)
                .line_number(true)
                .folding(true)
                .show_whitespaces(false)
                .tab_size(TabSize {
                    tab_size: 4,
                    hard_tabs: false,
                })
                .default_value("Loading…")
        });

        let target_path = relative_path.to_string();
        let target_project = project_dir.to_path_buf();
        let subscription = cx.subscribe(&editor, move |this, editor, event: &InputEvent, cx| {
            if matches!(event, InputEvent::Change) {
                let current = editor.read(cx).value();
                if let Some(tab) = this
                    .tabs
                    .iter_mut()
                    .find(|t| t.project_dir == target_project && t.relative_path == target_path)
                {
                    // An edit after a read completed must also cancel its queued replacement.
                    tab.pending_content = None;
                    tab.markdown_preview.refresh(current.clone(), cx);
                    let dirty = current.as_str() != tab.saved_content.as_str();
                    if tab.is_dirty != dirty {
                        tab.is_dirty = dirty;
                        cx.notify();
                    }
                }
            }
        });

        let tab_title = threadlane_ui_kit::editor_tab_title(relative_path, false);

        // Cursor/selection notifications only update controls. Content changes
        // refresh preview in the Change subscription (or explicit reload path).
        let observe = cx.observe(&editor, |_this, _editor, cx| cx.notify());

        self.tabs.push(EditorTab {
            project_dir: project_dir.to_path_buf(),
            relative_path: relative_path.to_string(),
            file_name: tab_title,
            _language: lang,
            saved_content: String::new(),
            is_dirty: false,
            is_diff: false,
            pending_content: None,
            pending_line: None,
            loading: false,
            editor_state: Some(editor.clone()),
            markdown_preview: threadlane_ui_kit::MarkdownPreview::new(cx),
            text_view_state: None,
            _subscription: Some(subscription),
            _observe: Some(observe),
        });

        self.active_tab_index = Some(self.tabs.len() - 1);
        self.status_msg = None;
        cx.notify();

        self.start_file_read(self.tabs.len() - 1, cx);
    }

    fn start_file_read(&mut self, index: usize, cx: &mut Context<Self>) {
        let tab = &mut self.tabs[index];
        let Some(load_editor) = tab.editor_state.clone() else { return; };
        tab.loading = true;
        tab.pending_content = None;
        let load_project = tab.project_dir.clone();
        let load_path = tab.relative_path.clone();
        let read_client = self.model.read(cx).daemon_client.clone();
        let read_project = load_project.clone();
        let read_path = load_path.clone();
        let read = cx.background_executor().spawn(async move {
            threadlane_ui_state::project_io::read_file(&read_client, &read_project, read_path)
                .await
        });
        cx.spawn(async move |this, cx| {
            let result = read.await;
            let _ = this.update(cx, |this, cx| {
                this.finish_file_open(&load_project, &load_path, &load_editor, result, cx);
            });
        })
        .detach();
    }

    /// Completes an asynchronous file open started by `open_file_internal`.
    ///
    /// Window-free: the loaded bytes land on the tab as `pending_content`
    /// (plus the saved baseline); the next render applies them to the editor
    /// via `sync_pending_content`, which owns the `Window` that `set_value`
    /// requires.
    ///
    /// Never clobbers user input: if the user typed into the loading tab
    /// while the read was in flight, their text stays and the file content
    /// becomes the saved baseline (marking the tab dirty, correctly).
    fn finish_file_open(
        &mut self,
        project_dir: &Path,
        relative_path: &str,
        editor: &Entity<EditorState>,
        result: Result<String, String>,
        cx: &mut Context<Self>,
    ) {
        let Some(tab) = self.tabs.iter_mut().find(|t| {
            t.project_dir == project_dir
                && t.relative_path == relative_path
                && !t.is_diff
                && t.editor_state.as_ref() == Some(editor)
        }) else {
            return;
        };
        tab.loading = false;
        match result {
            Ok(content) => {
                let current = editor.read(cx).value();
                if !tab.is_dirty && (current.as_str() == tab.saved_content || current.as_str() == "Loading…") {
                    tab.pending_content = Some(content.clone());
                    // Matches once `sync_pending_content` applies it.
                    tab.is_dirty = false;
                } else {
                    tab.is_dirty = current.as_str() != content.as_str();
                }
                tab.saved_content = content;
            }
            Err(error) => {
                tab.pending_line = None;
                tracing::error!(
                    "Failed to open file {}: {}",
                    project_dir.join(relative_path).display(),
                    error
                );
                self.set_status(format!("Couldn't open {relative_path}: {error}. Check that the file still exists and is readable, then open it again from Files."), true);
            }
        }
        cx.notify();
    }

    /// Applies background-loaded file content to editors. Called from
    /// `render` (which owns the `Window`), mirroring `sync_pending_file`.
    /// Each tab applies at most once: content is taken, never re-read.
    fn sync_pending_content(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let mut applied = false;
        for (ix, tab) in self
            .tabs
            .iter_mut()
            .enumerate()
            .filter(|(_, tab)| !tab.is_diff)
        {
            if let Some(editor) = tab.editor_state.clone() {
                if let Some(content) = tab.pending_content.take() {
                    let content: SharedString = content.into();
                    tab.markdown_preview.refresh(content.clone(), cx);
                    editor.update(cx, |editor, cx| editor.set_value(content, window, cx));
                    tab.is_dirty = false;
                    applied = true;
                }
                if Some(ix) == self.active_tab_index
                    && editor.read(cx).value().as_str() != "Loading…"
                    && tab.pending_line.is_some()
                    && !tab.loading
                {
                    // Cursor scrolling needs the loaded document's completed layout.
                    // Keep the request pending if the user switches tabs before then.
                    cx.on_next_frame(window, move |this, window, cx| {
                        let Some(tab) = this.active_tab_index.and_then(|ix| this.tabs.get_mut(ix))
                        else {
                            return;
                        };
                        if tab.editor_state.as_ref() != Some(&editor)
                            || tab.pending_content.is_some()
                            || tab.loading
                        {
                            return;
                        }
                        let Some(line) = tab.pending_line.take() else {
                            return;
                        };
                        editor.update(cx, |editor, cx| {
                            editor.set_cursor_position(
                                gpui_component::input::Position::new(
                                    line.saturating_sub(1).min(u32::MAX as usize) as u32,
                                    0,
                                ),
                                window,
                                cx,
                            )
                        });
                        cx.notify();
                    });
                }
            }
        }
        if applied {
            cx.notify();
        }
    }

    fn set_status(&mut self, msg: String, is_error: bool) {
        self.status_msg = Some((msg, is_error, std::time::Instant::now()));
    }

    fn visible_status(&self) -> Option<(String, bool)> {
        match &self.status_msg {
            Some((msg, is_error, at)) if at.elapsed() < STATUS_MSG_TTL => {
                Some((msg.clone(), *is_error))
            }
            _ => None,
        }
    }

    fn select_tab(&mut self, index: usize, cx: &mut Context<Self>) {
        if index < self.tabs.len() {
            self.active_tab_index = Some(index);
            self.status_msg = None;
            cx.notify();
        }
    }

    fn remove_tab_at(&mut self, index: usize, cx: &mut Context<Self>) {
        if index < self.tabs.len() {
            self.tabs.remove(index);
            if self.tabs.is_empty() {
                self.active_tab_index = None;
            } else if let Some(active) = self.active_tab_index {
                if active >= self.tabs.len() {
                    self.active_tab_index = Some(self.tabs.len() - 1);
                } else if active > index {
                    self.active_tab_index = Some(active - 1);
                }
            }
            self.status_msg = None;
            cx.notify();
        }
    }

    fn close_tab(&mut self, index: usize, cx: &mut Context<Self>) {
        if index >= self.tabs.len() {
            return;
        }

        let tab = &self.tabs[index];
        if tab.is_dirty && !tab.is_diff {
            let file_name = tab.file_name.clone();
            let target_path = tab.relative_path.clone();
            let target_project = tab.project_dir.clone();

            cx.spawn(async move |this, cx| {
                let result = rfd::AsyncMessageDialog::new()
                    .set_title("Discard unsaved changes?")
                    .set_description(format!(
                        "Do you want to discard unsaved changes to \"{file_name}\"?"
                    ))
                    .set_buttons(rfd::MessageButtons::YesNo)
                    .show()
                    .await;
                if matches!(result, rfd::MessageDialogResult::Yes) {
                    let _ = this.update(cx, |this, cx| {
                        if let Some(pos) = this.tabs.iter().position(|t| {
                            t.project_dir == target_project && t.relative_path == target_path
                        }) {
                            this.remove_tab_at(pos, cx);
                        }
                    });
                }
            })
            .detach();
        } else {
            self.remove_tab_at(index, cx);
        }
    }

    fn close_other_tabs(&mut self, keep_index: usize, cx: &mut Context<Self>) {
        if keep_index >= self.tabs.len() {
            return;
        }

        let dirty_names: Vec<String> = self
            .tabs
            .iter()
            .enumerate()
            .filter(|(i, t)| *i != keep_index && t.is_dirty && !t.is_diff)
            .map(|(_, t)| t.file_name.clone())
            .collect();

        if dirty_names.is_empty() {
            let kept = self.tabs.remove(keep_index);
            self.tabs = vec![kept];
            self.active_tab_index = Some(0);
            self.status_msg = None;
            cx.notify();
        } else {
            let keep_path = self.tabs[keep_index].relative_path.clone();
            let keep_project = self.tabs[keep_index].project_dir.clone();
            let description = if dirty_names.len() == 1 {
                format!(
                    "Do you want to discard unsaved changes to \"{}\"?",
                    dirty_names[0]
                )
            } else {
                format!(
                    "Do you want to discard unsaved changes to {} files ({})?",
                    dirty_names.len(),
                    dirty_names.join(", ")
                )
            };

            cx.spawn(async move |this, cx| {
                let result = rfd::AsyncMessageDialog::new()
                    .set_title("Discard unsaved changes?")
                    .set_description(description)
                    .set_buttons(rfd::MessageButtons::YesNo)
                    .show()
                    .await;
                if matches!(result, rfd::MessageDialogResult::Yes) {
                    let _ = this.update(cx, |this, cx| {
                        if let Some(pos) = this.tabs.iter().position(|t| {
                            t.project_dir == keep_project && t.relative_path == keep_path
                        }) {
                            let kept = this.tabs.remove(pos);
                            this.tabs = vec![kept];
                            this.active_tab_index = Some(0);
                            this.status_msg = None;
                            cx.notify();
                        }
                    });
                }
            })
            .detach();
        }
    }

    fn close_all_tabs(&mut self, cx: &mut Context<Self>) {
        let dirty_names: Vec<String> = self
            .tabs
            .iter()
            .filter(|t| t.is_dirty && !t.is_diff)
            .map(|t| t.file_name.clone())
            .collect();

        if dirty_names.is_empty() {
            self.tabs.clear();
            self.active_tab_index = None;
            self.status_msg = None;
            cx.notify();
        } else {
            let description = if dirty_names.len() == 1 {
                format!(
                    "Do you want to discard unsaved changes to \"{}\"?",
                    dirty_names[0]
                )
            } else {
                format!(
                    "Do you want to discard unsaved changes to {} files ({})?",
                    dirty_names.len(),
                    dirty_names.join(", ")
                )
            };

            cx.spawn(async move |this, cx| {
                let result = rfd::AsyncMessageDialog::new()
                    .set_title("Discard unsaved changes?")
                    .set_description(description)
                    .set_buttons(rfd::MessageButtons::YesNo)
                    .show()
                    .await;
                if matches!(result, rfd::MessageDialogResult::Yes) {
                    let _ = this.update(cx, |this, cx| {
                        this.tabs.clear();
                        this.active_tab_index = None;
                        this.status_msg = None;
                        cx.notify();
                    });
                }
            })
            .detach();
        }
    }

    fn save_active_file(&mut self, cx: &mut Context<Self>) {
        let Some(idx) = self.active_tab_index else {
            return;
        };
        self.save_tab_at(idx, cx);
    }

    fn save_file_action(&mut self, _: &SaveFile, _window: &mut Window, cx: &mut Context<Self>) {
        self.save_active_file(cx);
    }

    fn save_tab_at(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some(tab) = self.tabs.get_mut(index) else {
            return;
        };

        if tab.is_diff {
            return;
        }

        let Some(ref editor) = tab.editor_state else {
            return;
        };

        let project_dir = tab.project_dir.clone();
        let relative_path = tab.relative_path.clone();
        let file_path = project_dir.join(&relative_path);
        let content = editor.read(cx).value().to_string();
        let file_name = tab.file_name.clone();
        let editor = editor.clone();
        let client = self.model.read(cx).daemon_client.clone();

        // The file lives on the daemon host; save through project-io and
        // only settle the tab once the daemon confirms the write.
        cx.spawn(async move |this, cx| {
            let write_dir = project_dir.clone();
            let write_path = relative_path.clone();
            let write_content = content.clone();
            let result = cx
                .background_executor()
                .spawn(async move {
                    threadlane_ui_state::project_io::write_file(
                        &client,
                        &write_dir,
                        write_path,
                        write_content,
                    )
                    .await
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                match result {
                    Ok(()) => {
                        if let Some(tab) = this.tabs.iter_mut().find(|t| {
                            t.project_dir == project_dir
                                && t.relative_path == relative_path
                                && !t.is_diff
                        }) {
                            let current = editor.read(cx).value();
                            tab.is_dirty = current.as_str() != content.as_str();
                            tab.saved_content = content;
                        }
                        this.set_status(format!("Saved {file_name}"), false);
                    }
                    Err(err) => {
                        tracing::error!(
                            "Failed to save file {}: {}",
                            file_path.display(),
                            err
                        );
                        this.set_status(format!("Couldn't save {file_name}: {err}. Your edits are kept — fix the problem and save again."), true);
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn render_tab_bar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let owner = cx.entity();
        let selection_reason = self.selection_block_reason(cx);
        threadlane_ui_kit::editor_tab_bar(cx)
            .child(
                threadlane_ui_kit::editor_tabs().children(self.tabs.iter().enumerate().map(
                    |(index, tab)| {
                        let project = tab.project_dir.clone();
                        let path = tab.relative_path.clone();
                        let select_owner = owner.clone();
                        let select_project = project.clone();
                        let select_path = path.clone();
                        let close_owner = owner.clone();
                        let close_project = project.clone();
                        let close_path = path.clone();
                        let menu_owner = owner.clone();
                        let raw_path = path.strip_prefix("diff:").unwrap_or(&path).to_owned();
                        threadlane_ui_kit::editor_tab(
                            format!("editor-tab-{project:?}:{path}"),
                            tab.file_name.clone(),
                            raw_path,
                            self.active_tab_index == Some(index),
                            tab.is_dirty,
                            tab.is_diff,
                            move |_, _, cx| {
                                select_owner.update(cx, |view, cx| {
                                    if let Some(index) = view.tabs.iter().position(|tab| {
                                        tab.project_dir == select_project
                                            && tab.relative_path == select_path
                                    }) {
                                        view.select_tab(index, cx);
                                    }
                                });
                            },
                            move |_, _, cx| {
                                close_owner.update(cx, |view, cx| {
                                    if let Some(index) = view.tabs.iter().position(|tab| {
                                        tab.project_dir == close_project
                                            && tab.relative_path == close_path
                                    }) {
                                        view.close_tab(index, cx);
                                    }
                                });
                            },
                            cx,
                        )
                        .context_menu(move |menu, _, _| {
                            let owner = menu_owner.clone();
                            let project = project.clone();
                            let path = path.clone();
                            threadlane_ui_kit::editor_tab_menu(menu, move |action, _, cx| {
                                owner.update(cx, |view, cx| {
                                    let Some(index) = view.tabs.iter().position(|tab| {
                                        tab.project_dir == project && tab.relative_path == path
                                    }) else {
                                        return;
                                    };
                                    match action {
                                        threadlane_ui_kit::EditorTabAction::Close => {
                                            view.close_tab(index, cx)
                                        }
                                        threadlane_ui_kit::EditorTabAction::CloseOthers => {
                                            view.close_other_tabs(index, cx)
                                        }
                                        threadlane_ui_kit::EditorTabAction::CloseAll => {
                                            view.close_all_tabs(cx)
                                        }
                                    }
                                });
                            })
                        })
                    },
                )),
            )
            .child(threadlane_ui_kit::editor_actions(
                self.active_tab_index
                    .and_then(|ix| self.tabs.get(ix))
                    .filter(|tab| {
                        !tab.is_diff
                            && threadlane_ui_kit::markdown_preview_eligible(&tab.relative_path)
                    })
                    .map(|tab| {
                        tab.markdown_preview
                            .control(tab.loading || tab.pending_content.is_some(), cx)
                    }),
                self.visible_status(),
                Some(
                    threadlane_ui_kit::editor_add_selection_button(
                        "editor-add-selection",
                        &threadlane_ui_kit::AddSelectionControl {
                            enabled: selection_reason.is_none(),
                            reason: selection_reason,
                        },
                    )
                    .on_click(cx.listener(|view, _, window, cx| {
                        view.request_add_selection_to_chat(window, cx)
                    })),
                ),
                threadlane_ui_kit::editor_save_button(
                    self.is_active_dirty(),
                    self.is_active_diff(),
                )
                .on_click(cx.listener(|view, _, _, cx| view.save_active_file(cx))),
                cx,
            ))
    }

    fn toggle_markdown_preview(
        &mut self,
        _: &threadlane_ui_kit::ToggleMarkdownPreview,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(tab) = self.active_tab_index.and_then(|ix| self.tabs.get_mut(ix)) else {
            return;
        };
        if tab.markdown_preview.is_active() {
            tab.markdown_preview.show_source();
            tab.markdown_preview.focus_control(window, cx);
            cx.notify();
            return;
        }
        if tab.is_diff
            || tab.loading
            || tab.pending_content.is_some()
            || !threadlane_ui_kit::markdown_preview_eligible(&tab.relative_path)
        {
            return;
        }
        if let Some(editor) = &tab.editor_state {
            tab.markdown_preview.toggle(editor.read(cx).value(), cx);
            tab.markdown_preview.focus_control(window, cx);
            cx.notify();
        }
    }

    fn render_empty_state(&self, cx: &mut Context<Self>) -> impl IntoElement {
        threadlane_ui_kit::editor_empty_state(cx)
    }

}

impl EventEmitter<threadlane_ui_kit::EditorSelectionRequest> for EditorView {}

impl Render for EditorView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.sync_pending_file(window, cx);
        self.sync_pending_content(window, cx);
        threadlane_ui_kit::editor_surface(cx)
            .on_action(cx.listener(Self::save_file_action))
            .on_action(cx.listener(Self::toggle_markdown_preview))
            .on_action(cx.listener(|view, _: &threadlane_ui_kit::AddSelectionToChat, window, cx| {
                view.request_add_selection_to_chat(window, cx)
            }))
            .children(self.has_tabs().then(|| self.render_tab_bar(cx)))
            .children(
                self.active_tab_index
                    .and_then(|ix| self.tabs.get(ix))
                    .and_then(|tab| tab.markdown_preview.notice(tab.is_dirty))
                    .map(|notice| threadlane_ui_kit::markdown_preview_notice(notice, cx)),
            )
            .child(if let Some(idx) = self.active_tab_index {
                if let Some(active_tab) = self.tabs.get(idx) {
                    if active_tab.is_diff {
                        if let Some(ref text_view) = active_tab.text_view_state {
                            threadlane_ui_kit::editor_diff(text_view, cx)
                                .into_any_element()
                        } else {
                            self.render_empty_state(cx).into_any_element()
                        }
                    } else if active_tab.markdown_preview.is_active() {
                        active_tab.markdown_preview.body(cx)
                    } else if let Some(ref editor) = active_tab.editor_state {
                        threadlane_ui_kit::editor_buffer(editor)
                            .into_any_element()
                    } else {
                        self.render_empty_state(cx).into_any_element()
                    }
                } else {
                    self.render_empty_state(cx).into_any_element()
                }
            } else {
                self.render_empty_state(cx).into_any_element()
            })
    }
}

#[cfg(test)]
mod navigation_tests {
    use super::EditorView;
    use gpui::AppContext as _;

    #[gpui::test]
    fn markdown_preview_preserves_buffer_and_forces_source_for_line(cx: &mut gpui::TestAppContext) {
        cx.update(gpui_component::init);
        let model = cx.new(|_| threadlane_ui_state::AppState::for_tests());
        let (root, cx) = cx.add_window_view(|window, cx| {
            let view = cx.new(|cx| EditorView::new(model, window, cx));
            gpui_component::Root::new(view, window, cx)
        });
        let view = root.read_with(cx, |root, _| {
            root.view().clone().downcast::<EditorView>().unwrap()
        });
        cx.update(|window, cx| {
            view.update(cx, |view, cx| {
                view.open_diff_internal("README.md", "", window, cx);
                let editor = cx.new(|cx| {
                    gpui_component::input::EditorState::new(window, cx).default_value("# Unsaved")
                });
                let tab = &mut view.tabs[0];
                tab.is_diff = false;
                tab.relative_path = "README.md".into();
                tab.is_dirty = true;
                tab.editor_state = Some(editor);
            })
        });
        cx.update(|window, cx| window.draw(cx).clear(cx));
        let original = view.read_with(cx, |view, _| view.active_editor().unwrap());
        original.update(cx, |editor, cx| editor.set_selected_range(2..5, cx));
        let request = view.read_with(cx, |view, cx| threadlane_ui_kit::EditorSelectionRequest {
            editor: original.clone(),
            checkout: view.tabs[0].project_dir.clone(),
            relative_path: "README.md".into(),
            dirty: true,
            snapshot: threadlane_ui_kit::editor_selection_snapshot(original.read(cx)).unwrap(),
            destination: (None, None),
        });
        assert!(view.read_with(cx, |view, cx| view
            .selection_request_is_current(&request, cx)));
        let bounds = cx.debug_bounds("markdown-preview-mode").unwrap();
        cx.simulate_click(bounds.center(), gpui::Modifiers::default());
        cx.update(|window, cx| window.draw(cx).clear(cx));
        view.read_with(cx, |view, cx| {
            assert!(view.tabs[0].markdown_preview.is_active());
            assert_eq!(view.active_editor().unwrap(), original);
            assert!(view.tabs[0].is_dirty);
            assert_eq!(original.read(cx).selected_range(), 2..5);
            assert!(!view.selection_request_is_current(&request, cx));
            assert_eq!(
                view.selection_block_reason(cx).as_deref(),
                Some(threadlane_ui_kit::PREVIEW_SELECTION_REASON)
            );
        });
        // The document-scoped action also works from the focused native mode control.
        cx.update(|window, cx| {
            window.dispatch_action(Box::new(threadlane_ui_kit::ToggleMarkdownPreview), cx)
        });
        cx.run_until_parked();
        view.read_with(cx, |view, _| {
            assert!(!view.tabs[0].markdown_preview.is_active())
        });
        cx.update(|window, cx| {
            view.update(cx, |view, cx| {
                view.toggle_markdown_preview(&threadlane_ui_kit::ToggleMarkdownPreview, window, cx);
                let project = view.tabs[0].project_dir.clone();
                view.open_file_at_line(project, "README.md", Some(1), cx);
                view.sync_pending_file(window, cx);
                assert!(!view.tabs[0].markdown_preview.is_active());
                assert_eq!(view.active_editor().unwrap(), original);
            })
        });
    }

    #[gpui::test]
    fn shared_tabs_support_keyboard_selection_and_close_without_reselecting(
        cx: &mut gpui::TestAppContext,
    ) {
        cx.update(gpui_component::init);
        let project = std::path::PathBuf::from("/editor-preview-test");
        let model = cx.new(|_| {
            let mut state = threadlane_ui_state::AppState::default();
            state.projects.clear();
            state.pending_hydrations.clear();
            state.active_session_id = None;
            state.active_work_dir = Some(project.clone());
            state
        });
        let (root, cx) = cx.add_window_view(|window, cx| {
            let editor = cx.new(|cx| {
                let mut view = EditorView::new(model, window, cx);
                view.open_diff("one.rs", "+one", cx);
                view
            });
            gpui_component::Root::new(editor, window, cx)
        });
        let editor = root.read_with(cx, |root, _| {
            root.view().clone().downcast::<EditorView>().unwrap()
        });
        for name in ["two.rs", "three.rs"] {
            cx.update(|window, cx| window.draw(cx).clear(cx));
            editor.update(cx, |view, cx| view.open_diff(name, "+sample", cx));
        }
        cx.update(|window, cx| window.draw(cx).clear(cx));
        editor.update(cx, |view, cx| view.select_tab(0, cx));
        cx.update(|window, cx| window.draw(cx).clear(cx));
        let close = cx
            .debug_bounds(r#"editor-tab-"/editor-preview-test":diff:two.rs-close"#)
            .unwrap();
        cx.simulate_click(close.center(), gpui::Modifiers::default());
        editor.read_with(cx, |view, _| {
            assert_eq!(view.tabs.len(), 2);
            assert_eq!(
                view.tabs[view.active_tab_index.unwrap()].relative_path,
                "diff:one.rs"
            );
        });
        cx.update(|window, cx| {
            window.draw(cx).clear(cx);
            window.blur(cx);
            window.focus_next(cx); // Select one.rs
            window.focus_next(cx); // Close one.rs
            window.focus_next(cx); // Select three.rs
            assert!(
                window.focused(cx).is_some(),
                "editor controls are tab stops"
            );
            window.draw(cx).clear(cx);
        });
        let keystroke = gpui::Keystroke::parse("enter").unwrap();
        cx.simulate_event(gpui::KeyDownEvent {
            keystroke: keystroke.clone(),
            is_held: false,
            prefer_character_input: false,
        });
        cx.simulate_event(gpui::KeyUpEvent { keystroke });
        editor.read_with(cx, |view, _| {
            assert_eq!(
                view.tabs[view.active_tab_index.unwrap()].relative_path,
                "diff:three.rs"
            );
        });
        // Domain-derived identity stays usable when an earlier tab is removed.
        cx.update(|window, cx| window.draw(cx).clear(cx));
        let close = cx
            .debug_bounds(r#"editor-tab-"/editor-preview-test":diff:one.rs-close"#)
            .unwrap();
        cx.simulate_click(close.center(), gpui::Modifiers::default());
        cx.update(|window, cx| window.draw(cx).clear(cx));
        let close = cx
            .debug_bounds(r#"editor-tab-"/editor-preview-test":diff:three.rs-close"#)
            .unwrap();
        cx.simulate_click(close.center(), gpui::Modifiers::default());
        editor.read_with(cx, |view, _| {
            assert!(view.tabs.is_empty());
            assert!(view.active_tab_index.is_none());
        });
        cx.update(|window, cx| window.draw(cx).clear(cx));
        assert!(cx.debug_bounds("editor-save-btn").is_none());
    }
    #[gpui::test]
    fn add_selection_reports_disabled_reasons(cx: &mut gpui::TestAppContext) {
        cx.update(gpui_component::init);
        let project = std::path::PathBuf::from("/editor-preview-test");
        let model = cx.new(|_| {
            let mut state = threadlane_ui_state::AppState::for_tests();
            state.active_session_id = None;
            state.active_work_dir = Some(project.clone());
            state
        });
        let (root, cx) = cx.add_window_view(|window, cx| {
            let editor = cx.new(|cx| {
                let mut view = EditorView::new(model, window, cx);
                view.open_diff("one.rs", "+one", cx);
                view
            });
            gpui_component::Root::new(editor, window, cx)
        });
        let editor = root.read_with(cx, |root, _| {
            root.view().clone().downcast::<EditorView>().unwrap()
        });
        // A diff document can never hand a selection to chat.
        editor.read_with(cx, |view, cx| {
            assert_eq!(
                view.selection_block_reason(cx).as_deref(),
                Some("Diffs can't be added to chat — open the file itself")
            );
        });
        editor.update(cx, |view, cx| view.close_tab(0, cx));
        // No open file at all.
        editor.read_with(cx, |view, cx| {
            assert_eq!(
                view.selection_block_reason(cx).as_deref(),
                Some("Open a file first")
            );
        });
    }

    #[gpui::test]
    fn opens_at_requested_line_after_loading_and_reuses_tab(cx: &mut gpui::TestAppContext) {
        cx.update(gpui_component::init);
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("sample.rs"), "fn sample() {}\n".repeat(400)).unwrap();
        let project = dir.path().to_path_buf();
        let model = cx.new(|_| threadlane_ui_state::AppState::default());
        let holder = std::rc::Rc::new(std::cell::RefCell::new(None));
        let holder_clone = holder.clone();
        let project_clone = project.clone();
        let (_, cx) = cx.add_window_view(move |window, cx| {
            let editor = cx.new(|cx| {
                let mut editor = EditorView::new(model, window, cx);
                editor.open_file_at_line(project_clone, "sample.rs", Some(200), cx);
                editor
            });
            holder_clone.borrow_mut().replace(editor.clone());
            gpui_component::Root::new(editor, window, cx)
        });
        cx.run_until_parked();
        for _ in 0..4 {
            cx.update(|window, cx| {
                window.simulate_next_frame(cx);
            });
            cx.update(|window, cx| window.draw(cx).clear(cx));
            cx.run_until_parked();
        }
        let editor = holder.borrow().as_ref().unwrap().clone();
        editor.read_with(cx, |editor, cx| {
            assert_eq!(editor.tabs.len(), 1);
            assert_eq!(
                editor.tabs[0]
                    .editor_state
                    .as_ref()
                    .unwrap()
                    .read(cx)
                    .cursor_position()
                    .line,
                199
            );
            let state = editor.tabs[0].editor_state.as_ref().unwrap().read(cx);
            let (mut caret, _) = state.cursor_layout().expect("caret laid out");
            let viewport = state.input_bounds();
            // cursor_layout reports unscrolled Y; painting adds the text's scroll offset.
            caret.origin.y += state.text_bounds().unwrap().top() - viewport.top();
            assert!(
                caret.top() >= viewport.top() && caret.bottom() <= viewport.bottom(),
                "requested line must be visibly revealed: {caret:?} in {viewport:?}"
            );
        });
        let updated = "// saved externally\n".repeat(500);
        std::fs::write(project.join("sample.rs"), &updated).unwrap();
        editor.update(cx, |editor, cx| {
            editor.open_file_at_line(project.clone(), "sample.rs", Some(450), cx);
        });
        cx.run_until_parked();
        for _ in 0..4 {
            cx.update(|window, cx| window.simulate_next_frame(cx));
            cx.update(|window, cx| window.draw(cx).clear(cx));
            cx.run_until_parked();
        }
        editor.read_with(cx, |editor, cx| {
            assert_eq!(editor.tabs.len(), 1);
            let tab = &editor.tabs[0];
            assert!(!tab.is_dirty);
            let state = tab.editor_state.as_ref().unwrap().read(cx);
            assert_eq!(state.value().as_str(), updated);
            assert_eq!(state.cursor_position().line, 449);
        });
        cx.simulate_input("unsaved");
        let unsaved = editor.read_with(cx, |editor, cx| {
            assert!(editor.tabs[0].is_dirty);
            editor.tabs[0].editor_state.as_ref().unwrap().read(cx).value().to_string()
        });
        // A daemon read finishing after typing must retain those edits, too.
        editor.update(cx, |editor, cx| {
            let input = editor.tabs[0].editor_state.clone().unwrap();
            editor.tabs[0].loading = true;
            editor.finish_file_open(&project, "sample.rs", &input, Ok("new saved text\n".into()), cx);
            assert!(!editor.tabs[0].loading);
            assert!(editor.tabs[0].pending_content.is_none());
            assert_eq!(input.read(cx).value().as_str(), unsaved);
            editor.open_file_at_line(project.clone(), "sample.rs", Some(3), cx)
        });
        cx.run_until_parked();
        cx.update(|window, cx| window.draw(cx).clear(cx));
        cx.update(|window, cx| {
            window.simulate_next_frame(cx);
        });
        cx.update(|window, cx| window.draw(cx).clear(cx));
        editor.read_with(cx, |editor, cx| {
            assert_eq!(editor.tabs.len(), 1);
            assert_eq!(
                editor.tabs[0]
                    .editor_state
                    .as_ref()
                    .unwrap()
                    .read(cx)
                    .cursor_position()
                    .line,
                2
            );
            assert!(editor.tabs[0].is_dirty);
            assert_eq!(editor.tabs[0].editor_state.as_ref().unwrap().read(cx).value().as_str(), unsaved);
            assert!(editor.visible_status().unwrap().0.contains("Unsaved buffer preserved"));
        });
        // Failure must not jump into stale text or strand the tab as loading.
        editor.update(cx, |editor, cx| {
            let input = editor.tabs[0].editor_state.clone().unwrap();
            editor.tabs[0].loading = true;
            editor.tabs[0].pending_line = Some(99);
            editor.finish_file_open(&project, "sample.rs", &input, Err("disconnected".into()), cx);
            assert!(!editor.tabs[0].loading);
            assert!(editor.tabs[0].pending_line.is_none());
            assert_eq!(input.read(cx).value().as_str(), unsaved);
        });
    }
}
