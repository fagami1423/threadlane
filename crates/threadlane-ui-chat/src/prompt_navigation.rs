//! Shared "earlier prompts" machinery: composer prompt recall (unmodified
//! Up/Down browses previous user messages) and the conversation outline
//! popover (jump to an earlier prompt without a search query). Both consume
//! the same chronological `PromptLandmark` list derived from the projected
//! transcript in `transcript.rs`.

use super::*;

impl ChatListView {
    // ---- Composer prompt recall -----------------------------------------

    /// Prompt landmarks derived once per messages `Arc` + generation flag.
    /// The view's held `Arc` clone forces `Arc::make_mut` on the model to
    /// reallocate, so `ptr_eq` is a sound invalidation key — streaming
    /// updates replace the pointer and rebuild the list exactly once.
    fn prompt_landmark_entries(&mut self, cx: &App) -> Arc<Vec<PromptLandmark>> {
        let (messages, generating) = {
            let state = self.model.read(cx);
            (state.messages.clone(), state.is_generating)
        };
        let stale = self
            .prompt_landmarks_cache
            .as_ref()
            .is_none_or(|(cached, gen, _)| {
                *gen != generating || !Arc::ptr_eq(cached, &messages)
            });
        if stale {
            self.prompt_landmarks_cache = Some((
                messages.clone(),
                generating,
                Arc::new(prompt_landmarks(&messages, generating)),
            ));
        }
        self.prompt_landmarks_cache
            .as_ref()
            .map(|(_, _, landmarks)| landmarks.clone())
            .expect("cache populated above")
    }

    /// User prompts eligible for recall: nonblank text, excluding pending
    /// optimistic queue/steer echoes.
    fn recallable_prompts(&mut self, cx: &App) -> Vec<PromptLandmark> {
        self.prompt_landmark_entries(cx)
            .iter()
            .filter(|landmark| !landmark.pending_echo && !landmark.text.trim().is_empty())
            .cloned()
            .collect()
    }

    /// Why recall is unavailable, if it is. While browsing, the composer
    /// necessarily holds the recalled text, so `has_text` only blocks when
    /// the text is a genuine draft.
    pub(super) fn prompt_recall_block_reason(
        &mut self,
        has_text: bool,
        cx: &App,
    ) -> Option<&'static str> {
        if self.current_tab != CentralTab::Chat {
            return Some("Switch to Chat to recall a prompt");
        }
        let state = self.model.read(cx);
        if state.is_new_task || state.active_session_id.is_none() {
            return Some("Start a task before recalling a prompt");
        }
        if state.is_generating {
            return Some("Wait for the current turn to finish");
        }
        if let Some(session_id) = state.active_session_id.as_ref() {
            if state.pending_permissions.contains_key(session_id)
                || state.pending_questions.contains_key(session_id)
            {
                return Some("Answer the pending request first");
            }
        }
        if !self.pasted_images.is_empty() {
            return Some("Remove attached images to recall a prompt");
        }
        if has_text && self.prompt_recall.is_none() {
            return Some("Clear the composer to recall a prompt");
        }
        if self.recallable_prompts(cx).is_empty() {
            return Some("No earlier prompts yet");
        }
        None
    }

    /// Whether session state permits recall, independent of the composer's
    /// contents and whether any prompts exist.
    fn prompt_recall_gate_open(&self, cx: &App) -> bool {
        let state = self.model.read(cx);
        if state.is_new_task || state.active_session_id.is_none() || state.is_generating {
            return false;
        }
        let blocked = state
            .active_session_id
            .as_ref()
            .is_some_and(|session_id| {
                state.pending_permissions.contains_key(session_id)
                    || state.pending_questions.contains_key(session_id)
            });
        !blocked
    }

    /// Model-notify guard: end browsing when a gate opens (generation,
    /// permission/question, staged images) or the loaded message's identity
    /// is reconciled away. The loaded text is always kept.
    pub(super) fn retain_prompt_recall(&mut self, cx: &App) {
        let Some(recall) = self.prompt_recall.clone() else {
            return;
        };
        let untouched = self.input_state.read(cx).value() == recall.applied_text.as_str();
        let gone = !untouched
            || !self.prompt_recall_gate_open(cx)
            || !self.pasted_images.is_empty()
            || !self
                .recallable_prompts(cx)
                .iter()
                .any(|entry| entry.message_id == recall.landmark_id);
        if gone {
            self.prompt_recall = None;
        }
    }

    /// `up` under the ComposerPromptRecall keymap context: browse older, or
    /// fall through to the Textarea's native caret movement.
    pub(super) fn recall_older_prompt_action(
        &mut self,
        _: &RecallOlderPrompt,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.recall_arrow_action(true, window, cx);
    }

    /// `down` under the ComposerPromptRecall keymap context.
    pub(super) fn recall_newer_prompt_action(
        &mut self,
        _: &RecallNewerPrompt,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.recall_arrow_action(false, window, cx);
    }

    /// The binding preempted the Textarea's MoveUp/MoveDown, so when the
    /// recall path declines the press, re-dispatch the native action.
    fn recall_arrow_action(
        &mut self,
        older: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.prompt_recall_key(older, window, cx) {
            cx.notify();
        } else {
            let focus = self.input_state.read(cx).focus_handle(cx);
            if older {
                focus.dispatch_action(&MoveUp, window, cx);
            } else {
                focus.dispatch_action(&MoveDown, window, cx);
            }
        }
    }

    /// Steps the recall cursor; shared by the Up/Down interception and the
    /// strip's Older/Newer buttons. Returns whether a recall action ran.
    pub(super) fn step_prompt_recall(
        &mut self,
        older: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let entries = self.recallable_prompts(cx);
        let position = self.prompt_recall.as_ref().and_then(|recall| {
            entries
                .iter()
                .position(|entry| entry.message_id == recall.landmark_id)
        });
        match prompt_recall_step(position, entries.len(), older) {
            PromptRecallStep::Load(index) => {
                self.apply_recall_landmark(&entries[index], older, window, cx);
            }
            PromptRecallStep::Clear => {
                self.prompt_recall = None;
                self.input_state.update(cx, |input, cx| {
                    input.set_value("", window, cx);
                });
            }
            PromptRecallStep::PassThrough => return false,
        }
        true
    }

    /// Loads a landmark's text into the composer as the current recall
    /// state, placing the caret per the navigation direction.
    fn apply_recall_landmark(
        &mut self,
        landmark: &PromptLandmark,
        older: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.prompt_recall = Some(PromptRecallState {
            landmark_id: landmark.message_id.clone(),
            applied_text: landmark.text.clone(),
        });
        // Recalled slash text must not reopen command completion.
        self.dismiss_slash_menu = true;
        let caret_at_end = !older;
        let text = landmark.text.clone();
        let text_len = text.len();
        self.input_state.update(cx, |input, cx| {
            input.set_value(text, window, cx);
            // Multi-line `set_value` leaves the caret at the start; older
            // navigation wants it there, newer wants the end.
            if caret_at_end {
                input.set_selected_range(text_len..text_len, cx);
            }
        });
        self.focus_composer(window, cx);
    }

    /// Unmodified Up/Down entry point. Returns true when the arrow was
    /// consumed for recall; false keeps the native caret behavior.
    pub(super) fn prompt_recall_key(
        &mut self,
        older: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if self.current_tab != CentralTab::Chat || window.has_active_dialog(cx) {
            return false;
        }
        let (value, cursor, selection, focused) = {
            let input = self.input_state.read(cx);
            (
                input.value(),
                input.cursor(),
                input.selected_range(),
                input.focus_handle(cx).is_focused(window),
            )
        };
        if !focused {
            return false;
        }
        match self.prompt_recall.clone() {
            None => {
                // Entering browsing requires an idle, empty composer.
                if !older || !value.is_empty() {
                    return false;
                }
                if self.prompt_recall_block_reason(false, cx).is_some() {
                    return false;
                }
                self.step_prompt_recall(older, window, cx)
            }
            Some(recall) => {
                // Browsing continues only while the recalled text is
                // untouched and the selection is collapsed at a boundary.
                if value != recall.applied_text || !selection.is_empty() {
                    self.prompt_recall = None;
                    return false;
                }
                if (older && cursor != 0) || (!older && cursor != value.len()) {
                    return false;
                }
                if !self.prompt_recall_gate_open(cx) || !self.pasted_images.is_empty() {
                    // A gate opened while browsing: keep the loaded text.
                    self.prompt_recall = None;
                    return false;
                }
                if !self
                    .recallable_prompts(cx)
                    .iter()
                    .any(|entry| entry.message_id == recall.landmark_id)
                {
                    // The identity vanished during reconciliation; keep the
                    // loaded text and stop browsing.
                    self.prompt_recall = None;
                    return false;
                }
                self.step_prompt_recall(older, window, cx)
            }
        }
    }

    /// The compact status strip shown while browsing: `Earlier prompt · text
    /// only`, with named Older/Newer buttons that keep focus in the composer.
    pub(super) fn render_prompt_recall_strip(
        &mut self,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = cx.theme().colors;
        let entries = self.recallable_prompts(cx);
        let position = self
            .prompt_recall
            .as_ref()
            .and_then(|recall| {
                entries
                    .iter()
                    .position(|entry| entry.message_id == recall.landmark_id)
            })
            .unwrap_or(0);
        let at_oldest = position == 0;
        div()
            .id("prompt-recall-strip")
            .debug_selector(|| "prompt-recall-strip".into())
            .w_full()
            .mb_2()
            .px_3()
            .py_1p5()
            .rounded_lg()
            .border_1()
            .border_color(theme.border)
            .bg(theme.title_bar)
            .flex()
            .items_center()
            .gap_2()
            .role(Role::Status)
            .aria_label(
                "Earlier prompt recalled into the composer; text only, attachments are not restored",
            )
            .child(
                Icon::new(IconName::Undo2)
                    .xsmall()
                    .text_color(theme.muted_foreground),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child(format!(
                        "Earlier prompt · text only · {} of {}",
                        position + 1,
                        entries.len()
                    )),
            )
            .child(
                Button::new("prompt-recall-older")
                    .debug_selector(|| "prompt-recall-older".into())
                    .label("Older")
                    .ghost()
                    .xsmall()
                    .disabled(at_oldest)
                    .tooltip(if at_oldest {
                        "This is the oldest prompt"
                    } else {
                        "Recall an older prompt (Up)"
                    })
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.step_prompt_recall(true, window, cx);
                        this.focus_composer(window, cx);
                    })),
            )
            .child(
                Button::new("prompt-recall-newer")
                    .debug_selector(|| "prompt-recall-newer".into())
                    .label("Newer")
                    .ghost()
                    .xsmall()
                    .tooltip("Recall a newer prompt (Down); clears past the newest")
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.step_prompt_recall(false, window, cx);
                        this.focus_composer(window, cx);
                    })),
            )
            .into_any_element()
    }

    // ---- Prompt navigation rail -----------------------------------------

    /// Compact, virtualized landmarks; the outline remains the keyboard
    /// browser and exposes every excerpt when the conversation is long.
    pub(super) fn render_prompt_rail(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let entries = self.prompt_landmark_entries(cx);
        if entries.is_empty() {
            return div().into_any_element();
        }
        if self.prompt_rail_list_state.item_count() != entries.len() {
            self.prompt_rail_list_state.reset(entries.len());
        }
        if let Some(ix) = self.active_prompt_rail_index(&entries) {
            let id = &entries[ix].message_id;
            if self.prompt_rail_active_id.as_ref() != Some(id) {
                self.prompt_rail_list_state.scroll_to_reveal_item(ix);
                self.prompt_rail_active_id = Some(id.clone());
            }
        }
        div()
            .id("prompt-navigation-rail")
            .debug_selector(|| "prompt-navigation-rail".into())
            .absolute()
            .left_0()
            .top_0()
            .bottom_0()
            .w_8()
            .py_3()
            .flex()
            .flex_col()
            .justify_center()
            .items_center()
            .child(
                list(
                    self.prompt_rail_list_state.clone(),
                    cx.processor(Self::render_prompt_rail_tick),
                )
                .w_8()
                .h(rems((entries.len() as f32 * 1.5).min(12.0)))
                .max_h_full()
                .min_h_0(),
            )
            .child(self.render_outline_popover(cx))
            .into_any_element()
    }

    fn active_prompt_rail_index(&self, entries: &[PromptLandmark]) -> Option<usize> {
        let top = self.transcript_list_state.logical_scroll_top().item_ix;
        if self.transcript_list_state.is_following_tail() {
            entries.len().checked_sub(1)
        } else {
            Some(
                entries
                    .partition_point(|entry| entry.row_index <= top)
                    .saturating_sub(1),
            )
        }
    }

    fn render_prompt_rail_tick(
        &mut self,
        index: usize,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let entries = self.prompt_landmark_entries(cx);
        let Some(landmark) = entries.get(index) else {
            return div().into_any_element();
        };
        let active = self.active_prompt_rail_index(&entries);
        let selected = active == Some(index);
        let theme = cx.theme().colors;
        let id = landmark.message_id.clone();
        let label = format!(
            "Prompt {} · {}",
            landmark.ordinal,
            if landmark.excerpt.is_empty() {
                "No text"
            } else {
                &landmark.excerpt
            }
        );
        Button::new(SharedString::from(format!("prompt-rail-{id}")))
            .debug_selector({
                let id = id.clone();
                move || format!("prompt-rail-{id}")
            })
            .ghost()
            .small()
            .w_8()
            .h_6()
            .accessibility_label(if selected {
                format!("{label} · Current prompt")
            } else {
                label.clone()
            })
            .tooltip(label)
            .tooltip_placement(gpui_component::Placement::Right)
            .child(
                div()
                    .h_0p5()
                    .rounded_full()
                    .when(selected, |el| el.w_4().bg(theme.foreground))
                    .when(!selected, |el| el.w_2().bg(theme.muted_foreground)),
            )
            .on_click(cx.listener(move |this, _, window, cx| {
                this.clear_conversation_find();
                this.outline_focus_id = Some(id.clone());
                this.activate_outline_prompt(window, cx);
                cx.notify();
            }))
            .into_any_element()
    }

    // ---- Conversation outline --------------------------------------------

    /// Re-derive landmarks while the popover is open. Focus follows the
    /// focused message's identity; a vanished id falls back to the newest
    /// entry and can never jump to the wrong row.
    pub(super) fn refresh_conversation_outline(&mut self, cx: &App) {
        if !self.outline_open || self.current_tab != CentralTab::Chat {
            return;
        }
        let landmarks = self.prompt_landmark_entries(cx).as_ref().clone();
        if landmarks.len() != self.outline_landmarks.len() {
            // Rebuilding the list state on every render would fight the
            // user's scroll; only resize when the count actually changed.
            self.outline_list_state.reset(landmarks.len());
        }
        self.outline_landmarks = landmarks;
        if !self
            .outline_landmarks
            .iter()
            .any(|landmark| Some(&landmark.message_id) == self.outline_focus_id.as_ref())
        {
            self.outline_focus_id = self
                .outline_landmarks
                .last()
                .map(|landmark| landmark.message_id.clone());
        }
    }

    /// List index of the currently focused landmark id, if still present.
    fn outline_focus_index(&self) -> Option<usize> {
        self.outline_focus_id.as_ref().and_then(|id| {
            self.outline_landmarks
                .iter()
                .position(|landmark| &landmark.message_id == id)
        })
    }

    /// Opens the outline popover: supersedes Find, focuses the previously
    /// jumped-to prompt when still listed (else the newest), and hands
    /// keyboard focus to the list.
    pub(super) fn open_conversation_outline(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.outline_open || window.has_active_dialog(cx) {
            return;
        }
        // Outline supersedes Find without moving the viewport.
        self.clear_conversation_find();
        self.prompt_recall = None;
        // Focus restoration on close is owned by the kit Popover: it
        // captures the previously focused handle (the trigger, via
        // pointer auto-focus or keyboard focus) before moving focus here.
        self.outline_open = true;
        self.refresh_conversation_outline(cx);
        // Prefer the last jumped-to prompt when it is still listed,
        // otherwise focus the newest prompt.
        self.outline_focus_id = self
            .outline_selected_id
            .as_ref()
            .filter(|id| {
                self.outline_landmarks
                    .iter()
                    .any(|landmark| &landmark.message_id == *id)
            })
            .cloned()
            .or_else(|| {
                self.outline_landmarks
                    .last()
                    .map(|landmark| landmark.message_id.clone())
            });
        if let Some(index) = self.outline_focus_index() {
            self.outline_list_state.scroll_to_reveal_item(index);
        }
        window.focus(&self.outline_focus, cx);
        // The trigger button claims focus as part of its click handling;
        // re-focus the list on the next frame so keyboard navigation lands
        // inside the popover.
        let view = cx.entity();
        window.on_next_frame(move |window, cx| {
            view.update(cx, |this, cx| {
                if this.outline_open {
                    window.focus(&this.outline_focus, cx);
                }
            });
        });
        cx.notify();
    }

    /// Closes the popover; the kit Popover restores the focus it captured
    /// before opening once `.open(false)` syncs on the next render.
    fn close_conversation_outline(&mut self, cx: &mut Context<Self>) {
        if !self.outline_open {
            return;
        }
        self.outline_open = false;
        cx.notify();
    }

    /// Key handling while the outline list owns focus: Escape cancels,
    /// arrows/Home/End move list focus, Enter/Space jump.
    pub(super) fn handle_outline_key_down(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if window.has_active_dialog(cx) {
            return;
        }
        let key = event.keystroke.key.as_str();
        match key {
            "escape" => {
                self.close_conversation_outline(cx);
                cx.stop_propagation();
            }
            "up" | "down" | "home" | "end" if !event.keystroke.modifiers.modified() => {
                if let Some(index) = step_prompt_focus(
                    self.outline_focus_index(),
                    self.outline_landmarks.len(),
                    key,
                ) {
                    self.outline_focus_id = self
                        .outline_landmarks
                        .get(index)
                        .map(|landmark| landmark.message_id.clone());
                    self.outline_list_state.scroll_to_reveal_item(index);
                    cx.notify();
                }
                cx.stop_propagation();
            }
            "enter" | "space" if !event.keystroke.modifiers.modified() => {
                self.activate_outline_prompt(window, cx);
                cx.stop_propagation();
            }
            _ => {}
        }
    }

    /// Jump the transcript to the focused landmark and close the outline.
    /// The row index is re-validated against the freshly synced transcript so
    /// a reconciled message can never select a different prompt.
    fn activate_outline_prompt(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(focus_id) = self.outline_focus_id.clone() else {
            return;
        };
        let landmarks = self.prompt_landmark_entries(cx).as_ref().clone();
        let (messages, generating) = {
            let state = self.model.read(cx);
            (state.messages.clone(), state.is_generating)
        };
        if landmarks.len() != self.outline_landmarks.len() {
            self.outline_list_state.reset(landmarks.len());
        }
        self.outline_landmarks = landmarks;
        let Some(landmark) = self
            .outline_landmarks
            .iter()
            .find(|landmark| landmark.message_id == focus_id)
            .cloned()
        else {
            // The entry vanished mid-navigation; keep browsing the list.
            self.outline_focus_id = self
                .outline_landmarks
                .last()
                .map(|landmark| landmark.message_id.clone());
            cx.notify();
            return;
        };
        self.sync_transcript_rows(messages, generating, false);
        let row = landmark.row_index;
        if !matches!(self.transcript_rows.get(row),
            Some(TranscriptRow::Message(index)) if self.transcript_messages[*index].id == landmark.message_id)
        {
            return;
        }
        self.outline_selected_id = Some(landmark.message_id);
        self.initial_scroll_frames = 0;
        self.transcript_list_state.pause_following_tail();
        self.transcript_list_state.scroll_to(ListOffset {
            item_ix: row,
            offset_in_item: px(0.),
        });
        self.close_conversation_outline(cx);
    }

    /// One outline list row: `Prompt N · excerpt`, with the focused and
    /// last-jumped-to landmarks highlighted.
    fn render_outline_row(
        &mut self,
        index: usize,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = cx.theme().colors;
        let Some(landmark) = self.outline_landmarks.get(index).cloned() else {
            return div().into_any_element();
        };
        let focused = self.outline_focus_id.as_ref() == Some(&landmark.message_id);
        let selected = self.outline_selected_id.as_ref() == Some(&landmark.message_id);
        let label = if landmark.excerpt.is_empty() {
            format!("Prompt {} · No text", landmark.ordinal)
        } else {
            format!("Prompt {} · {}", landmark.ordinal, landmark.excerpt)
        };
        let message_id = landmark.message_id.clone();
        div()
            .id(("conversation-outline-row", index))
            .flex()
            .flex_row()
            .w_full()
            .items_center()
            .gap_2()
            .px_3()
            .py_1p5()
            .cursor_pointer()
            .role(Role::ListBoxOption)
            .aria_label(label.clone())
            .when(focused, |el| el.bg(theme.secondary))
            .when(!focused, |el| {
                el.hover(|style| style.bg(theme.list_hover))
            })
            .child(
                div()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .flex_shrink_0()
                    .child(format!("Prompt {}", landmark.ordinal)),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_sm()
                    .text_color(if landmark.excerpt.is_empty() {
                        theme.muted_foreground
                    } else {
                        theme.foreground
                    })
                    .child(if landmark.excerpt.is_empty() {
                        SharedString::from("No text")
                    } else {
                        SharedString::from(landmark.excerpt.clone())
                    }),
            )
            .when(selected, |el| {
                el.child(
                    Icon::new(IconName::Check)
                        .xsmall()
                        .text_color(theme.primary),
                )
            })
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, _, window, cx| {
                    this.outline_focus_id = Some(message_id.clone());
                    this.activate_outline_prompt(window, cx);
                }),
            )
            .into_any_element()
    }

    /// The rail trigger plus the bounded popover hosting the prompt
    /// landmark list.
    pub(super) fn render_outline_popover(
        &self,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let outline_chat = cx.entity();
        let outline_chat_content = cx.entity();
        // The popover toggles itself on pointer mousedown and on the
        // Enter/Space Confirm binding it registers under its "Popover"
        // context; `on_open_change` is the single driver of view state so
        // no click handler can double-toggle it.
        Popover::new("conversation-outline")
            .anchor(Anchor::TopLeft)
            .appearance(false)
            .open(self.outline_open)
            .track_focus(&self.outline_focus)
            .on_open_change(move |open, window, cx| {
                outline_chat.update(cx, |this, cx| {
                    if *open {
                        this.open_conversation_outline(window, cx);
                    } else {
                        this.close_conversation_outline(cx);
                    }
                });
            })
            .trigger(
                Button::new("conversation-outline-open")
                    .debug_selector(|| "conversation-outline-open".into())
                    .label("…")
                    .ghost()
                    .small()
                    .accessibility_label("Conversation outline")
                    .tooltip("Conversation outline — jump to an earlier prompt"),
            )
            .content(move |_, _window, cx| {
                outline_chat_content.update(cx, |this, cx| this.render_outline_content(cx))
            })
    }

    /// The popover body: header, status line for loading/error/empty,
    /// and the virtualized landmark list.
    fn render_outline_content(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme().colors;
        let (loading, load_error) = {
            let state = self.model.read(cx);
            let error = state
                .session_status
                .clone()
                .filter(|status| status.starts_with("Could not load session:"));
            (state.active_session_is_loading(), error)
        };
        let count = self.outline_landmarks.len();
        let status_message: Option<SharedString> = load_error
            .map(SharedString::from)
            .or_else(|| (loading && count == 0).then(|| SharedString::from("Loading conversation…")))
            .or_else(|| (count == 0).then(|| SharedString::from("No prompts yet")));

        div()
            .id("conversation-outline-content")
            .flex()
            .flex_col()
            .role(Role::ListBox)
            .aria_label("Conversation outline prompts")
            .track_focus(&self.outline_focus)
            .key_context("ConversationOutline")
            .on_key_down(cx.listener(Self::handle_outline_key_down))
            // The kit Popover binds Enter/Space to Confirm under its "Popover"
            // context and toggles closed on that action. Intercept it here —
            // deeper on the dispatch path — so activation jumps instead.
            .on_action(cx.listener(
                |this: &mut Self, _: &gpui_component::dialog::Confirm, window, cx| {
                    this.activate_outline_prompt(window, cx);
                    cx.stop_propagation();
                },
            ))
            .w_80()
            .h(rems(18.75))
            .rounded_lg()
            .border_1()
            .border_color(theme.border)
            .bg(theme.popover)
            .shadow_lg()
            .occlude()
            .child(
                div()
                    .flex()
                    .flex_row()
                    .w_full()
                    .items_center()
                    .justify_between()
                    .px_3()
                    .py_2()
                    .border_b_1()
                    .border_color(theme.border)
                    .child(
                        div()
                            .text_xs()
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(theme.muted_foreground)
                            .child("Prompts in this conversation"),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child(format!("{count}")),
                    ),
            )
            .child(
                div().flex_1().min_h_0().child(match status_message {
                    Some(message) => div()
                        .w_full()
                        .h_full()
                        .flex()
                        .items_center()
                        .justify_center()
                        .px_3()
                        .text_sm()
                        .text_color(theme.muted_foreground)
                        .child(message)
                        .into_any_element(),
                    None => list(
                        self.outline_list_state.clone(),
                        cx.processor(Self::render_outline_row),
                    )
                    .w_full()
                    .h_full()
                    .into_any_element(),
                }),
            )
            .child(
                div()
                    .w_full()
                    .px_3()
                    .py_1p5()
                    .border_t_1()
                    .border_color(theme.border)
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child("Enter to jump · Esc to close"),
            )
            .into_any_element()
    }
}
