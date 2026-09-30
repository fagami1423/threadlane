#[gpui::test]
fn composer_model_controls_fit_narrow_and_wide_panels(cx: &mut gpui::TestAppContext) {
    use gpui::AppContext as _;

    cx.update(gpui_component::init);
    let model = cx.new(|_| {
        let mut state = threadlane_ui_state::AppState::default();
        state.selected_model = "some-future/model-with-a-long-display-name".into();
        state.is_new_task = false;
        state
    });
    let (_, cx) = cx.add_window_view(move |window, cx| {
        let chat = cx.new(|cx| super::ChatListView::new(model, window, cx));
        gpui_component::Root::new(chat, window, cx)
    });
    for width in [320.0, 800.0] {
        cx.simulate_resize(gpui::size(gpui::px(width), gpui::px(800.0)));
        cx.run_until_parked();
        cx.update(|window, cx| window.draw(cx).clear(cx));
        for selector in ["composer-model-picker", "composer-reasoning-effort-picker"] {
            let bounds = cx
                .debug_bounds(selector)
                .expect("composer control is visible");
            assert!(bounds.size.width > gpui::px(0.0));
            assert!(
                bounds.left() >= gpui::px(0.0),
                "{selector} overflows left at {width}px"
            );
            assert!(
                bounds.right() <= gpui::px(width),
                "{selector} overflows right at {width}px: {bounds:?}"
            );
        }
    }
}

#[gpui::test]
fn reasoning_menu_reports_open_and_escape_dismissal(cx: &mut gpui::TestAppContext) {
    use gpui::AppContext as _;

    cx.update(gpui_component::init);
    let model = cx.new(|_| {
        let mut state = threadlane_ui_state::AppState::default();
        state.selected_model = "some-future/model-with-a-long-display-name".into();
        state.is_new_task = false;
        state
    });
    let (chat, cx) = cx.add_window_view(move |window, cx| {
        super::ChatListView::new(model, window, cx)
    });
    cx.run_until_parked();
    cx.update(|window, cx| window.draw(cx).clear(cx));
    let trigger = cx.debug_bounds("composer-reasoning-effort-picker").unwrap();
    cx.simulate_click(trigger.center(), gpui::Modifiers::default());
    cx.run_until_parked();
    cx.update(|window, cx| window.draw(cx).clear(cx));
    chat.read_with(cx, |chat, _| {
        assert!(chat.reasoning_menu_open.get(), "click must open menu");
    });
    cx.simulate_keystrokes("escape");
    cx.run_until_parked();
    chat.read_with(cx, |chat, _| {
        assert!(!chat.reasoning_menu_open.get(), "Escape must close menu");
    });
}

#[gpui::test]
fn composer_shift_enter_adds_a_line_without_sending(cx: &mut gpui::TestAppContext) {
    use gpui::AppContext as _;

    cx.update(gpui_component::init);
    let model = cx.new(|_| threadlane_ui_state::AppState::default());
    let retained_model = model.clone();
    let (chat, cx) =
        cx.add_window_view(move |window, cx| super::ChatListView::new(model, window, cx));
    chat.update_in(cx, |chat, window, cx| {
        chat.input_state.update(cx, |input, cx| {
            input.set_value("First line", window, cx);
        });
        chat.focus_composer(window, cx);
    });
    cx.run_until_parked();
    cx.update(|window, cx| window.draw(cx).clear(cx));
    cx.simulate_keystrokes("end shift-enter");
    cx.run_until_parked();
    chat.read_with(cx, |chat, cx| {
        assert_eq!(chat.input_state.read(cx).value().as_ref(), "First line\n");
    });
    retained_model.read_with(cx, |state, _| {
        assert!(state.messages.is_empty(), "newline must not submit the draft");
        assert!(!state.is_generating);
    });
}

#[gpui::test]
fn stashing_requires_a_task_and_preserves_existing_stashes_and_images(cx: &mut gpui::TestAppContext) {
    use gpui::AppContext as _;

    cx.update(gpui_component::init);
    let model = cx.new(|_| threadlane_ui_state::AppState::default());
    let retained_model = model.clone();
    let (chat, cx) =
        cx.add_window_view(move |window, cx| super::ChatListView::new(model, window, cx));
    for (has_task, has_stash, has_image, generating) in [
        (false, false, false, false),
        (true, true, false, false),
        (true, false, true, false),
        (true, false, false, true),
        (true, false, false, false),
    ] {
        retained_model.update(cx, |state, cx| {
            state.active_session_id = has_task.then(|| "stash-task".into());
            state.is_generating = generating;
            state.clear_stashed_prompt("stash-task");
            if has_stash {
                state.stash_prompt("stash-task", "Previous stash".into());
            }
            cx.notify();
        });
        cx.run_until_parked();
        chat.update_in(cx, |chat, window, cx| {
            chat.input_state.update(cx, |input, cx| {
                input.set_value("Current draft", window, cx);
            });
            chat.pasted_images.clear();
            if has_image {
                chat.pasted_images.push(super::ImageAttachment {
                    display_name: "draft.png".into(),
                    data_url: "data:image/png;base64,test".into(),
                });
            }
            cx.notify();
        });
        cx.run_until_parked();
        cx.update(|window, cx| window.draw(cx).clear(cx));
        let stash = cx.debug_bounds("stash-prompt-btn").unwrap();
        cx.simulate_click(stash.center(), gpui::Modifiers::default());
        cx.run_until_parked();
        let allowed = has_task && !has_stash && !has_image && !generating;
        chat.read_with(cx, |chat, cx| {
            assert_eq!(
                chat.input_state.read(cx).value().as_ref(),
                if allowed { "" } else { "Current draft" }
            );
            assert_eq!(chat.pasted_images.len(), usize::from(has_image));
        });
        retained_model.read_with(cx, |state, _| {
            let expected = if has_stash {
                Some("Previous stash")
            } else if allowed {
                Some("Current draft")
            } else {
                None
            };
            assert_eq!(
                state.get_stashed_prompt("stash-task").map(String::as_str),
                expected
            );
        });
    }
}

#[gpui::test]
fn restoring_a_stash_preserves_unsent_text_and_images(cx: &mut gpui::TestAppContext) {
    use gpui::{AppContext as _, Focusable as _};

    cx.update(gpui_component::init);
    let model = cx.new(|_| {
        let mut state = threadlane_ui_state::AppState::default();
        state.active_session_id = Some("draft-session".into());
        state.is_new_task = false;
        state.stash_prompt("draft-session", "Saved request".into());
        state
    });
    let retained_model = model.clone();
    let holder = std::rc::Rc::new(std::cell::RefCell::new(None));
    let holder_clone = holder.clone();
    let (_, cx) = cx.add_window_view(move |window, cx| {
        let chat = cx.new(|cx| super::ChatListView::new(model, window, cx));
        holder_clone.borrow_mut().replace(chat.clone());
        struct DialogHost(gpui::Entity<super::ChatListView>);
        impl gpui::Render for DialogHost {
            fn render(&mut self, _window: &mut gpui::Window, _cx: &mut gpui::Context<Self>) -> impl gpui::IntoElement {
                use gpui::{ParentElement as _, Styled as _};
                gpui::div().size_full().child(self.0.clone())
            }
        }
        let host = cx.new(|_| DialogHost(chat));
        gpui_component::Root::new(host, window, cx)
    });
    let chat = holder.borrow().as_ref().unwrap().clone();
    cx.run_until_parked();
    cx.update(|window, cx| window.draw(cx).clear(cx));
    let discard = cx.debug_bounds("discard-stashed-draft").unwrap();
    cx.simulate_click(discard.center(), gpui::Modifiers::default());
    cx.run_until_parked();
    cx.update(|window, cx| window.draw(cx).clear(cx));
    assert!(cx.debug_bounds("dialog-0").is_some());
    retained_model.read_with(cx, |state, _| {
        assert_eq!(
            state.get_stashed_prompt("draft-session").map(String::as_str),
            Some("Saved request")
        );
    });
    cx.simulate_keystrokes("escape");
    cx.run_until_parked();
    cx.update(|window, cx| window.draw(cx).clear(cx));
    assert!(cx.debug_bounds("dialog-0").is_none());
    for (draft, image) in [("Newer request", false), ("", true), ("", false)] {
        chat.update_in(cx, |chat, window, cx| {
            chat.input_state.update(cx, |input, cx| {
                input.set_value(draft, window, cx);
            });
            chat.pasted_images.clear();
            if image {
                chat.pasted_images.push(super::ImageAttachment {
                    display_name: "new.png".into(),
                    data_url: "data:image/png;base64,test".into(),
                });
            }
            cx.notify();
        });
        cx.run_until_parked();
        cx.update(|window, cx| window.draw(cx).clear(cx));
        let restore = cx.debug_bounds("restore-stashed-draft").unwrap();
        cx.simulate_click(restore.center(), gpui::Modifiers::default());
        cx.run_until_parked();
        let blocked = !draft.is_empty() || image;
        chat.read_with(cx, |chat, cx| {
            assert_eq!(
                chat.input_state.read(cx).value().as_ref(),
                if blocked { draft } else { "Saved request" }
            );
            assert_eq!(chat.pasted_images.len(), usize::from(image));
        });
        retained_model.read_with(cx, |state, _| {
            assert_eq!(state.get_stashed_prompt("draft-session").is_some(), blocked);
        });
        cx.update(|window, cx| {
            let focus = chat.read(cx).input_state.read(cx).focus_handle(cx);
            assert_eq!(window.focused(cx), Some(focus));
        });
    }
    chat.update_in(cx, |chat, window, cx| {
        chat.input_state.update(cx, |input, cx| {
            input.set_value("Keep this draft", window, cx);
        });
        chat.pasted_images.push(super::ImageAttachment {
            display_name: "keep.png".into(),
            data_url: "data:image/png;base64,keep".into(),
        });
        cx.notify();
    });
    for replaced in [false, true] {
        retained_model.update(cx, |state, cx| {
            state.stash_prompt("draft-session", "Discard this stash".into());
            cx.notify();
        });
        cx.run_until_parked();
        cx.update(|window, cx| window.draw(cx).clear(cx));
        let discard = cx.debug_bounds("discard-stashed-draft").unwrap();
        cx.simulate_click(discard.center(), gpui::Modifiers::default());
        cx.run_until_parked();
        cx.update(|window, cx| window.draw(cx).clear(cx));
        assert!(cx.debug_bounds("dialog-0").is_some());
        if replaced {
            retained_model.update(cx, |state, cx| {
                state.stash_prompt("draft-session", "New saved draft".into());
                cx.notify();
            });
        }
        cx.update(|window, cx| {
            window.dispatch_action(
                Box::new(gpui_component::dialog::Confirm { secondary: false }),
                cx,
            );
        });
        cx.run_until_parked();
        cx.update(|window, cx| window.draw(cx).clear(cx));
        assert!(cx.debug_bounds("dialog-0").is_none());
        retained_model.read_with(cx, |state, _| {
            assert_eq!(
                state.get_stashed_prompt("draft-session").map(String::as_str),
                replaced.then_some("New saved draft")
            );
        });
        chat.read_with(cx, |chat, cx| {
            assert_eq!(chat.input_state.read(cx).value().as_ref(), "Keep this draft");
            assert_eq!(chat.pasted_images.len(), 1);
            assert_eq!(chat.pasted_images[0].data_url, "data:image/png;base64,keep");
        });
    }
}

#[gpui::test]
fn unsupported_acp_steer_keeps_the_composer_text_and_images(cx: &mut gpui::TestAppContext) {
    use gpui::AppContext as _;

    cx.update(gpui_component::init);
    let model = cx.new(|_| {
        let mut state = threadlane_ui_state::AppState::default();
        state.selected_model = "acp/test".into();
        state.is_generating = true;
        state
    });
    let retained_model = model.clone();
    let (chat, cx) =
        cx.add_window_view(move |window, cx| super::ChatListView::new(model, window, cx));
    chat.update_in(cx, |chat, window, cx| {
        chat.pasted_images.push(super::ImageAttachment {
            display_name: "draft.png".into(),
            data_url: "data:image/png;base64,test".into(),
        });
        chat.input_state.update(cx, |input, cx| {
            input.set_value("Keep this draft", window, cx);
            cx.emit(super::InputEvent::PressEnter {
                secondary: true,
                shift: false,
            });
        });
    });
    cx.run_until_parked();
    chat.update(cx, |chat, cx| {
        assert_eq!(
            chat.input_state.read(cx).value().as_ref(),
            "Keep this draft"
        );
        assert_eq!(chat.pasted_images.len(), 1);
    });
    retained_model.read_with(cx, |state, _| {
        assert!(state
            .session_status
            .as_deref()
            .unwrap()
            .contains("Use Queue"));
        assert!(state.active_pending_composer_message().is_none());
    });
}

#[gpui::test]
fn unsent_composer_drafts_and_images_follow_their_task(cx: &mut gpui::TestAppContext) {
    use gpui::AppContext as _;

    cx.update(gpui_component::init);
    let model = cx.new(|_| {
        let mut state = threadlane_ui_state::AppState::default();
        state.active_work_dir = Some("/projects/one".into());
        state.active_session_id = Some("first".into());
        state
    });
    let retained_model = model.clone();
    let (chat, cx) =
        cx.add_window_view(move |window, cx| super::ChatListView::new(model, window, cx));
    chat.update_in(cx, |chat, window, cx| {
        chat.input_state.update(cx, |input, cx| {
            input.set_value("First task draft", window, cx);
        });
        chat.pasted_images.push(super::ImageAttachment {
            display_name: "first.png".into(),
            data_url: "data:image/png;base64,test".into(),
        });
    });
    retained_model.update(cx, |state, cx| {
        state.active_session_id = Some("second".into());
        cx.notify();
    });
    cx.run_until_parked();
    chat.update_in(cx, |chat, window, cx| {
        assert!(chat.input_state.read(cx).value().is_empty());
        assert!(chat.pasted_images.is_empty());
        chat.input_state.update(cx, |input, cx| {
            input.set_value("Second task draft", window, cx);
        });
    });
    retained_model.update(cx, |state, cx| {
        state.active_session_id = Some("first".into());
        cx.notify();
    });
    cx.run_until_parked();
    chat.update(cx, |chat, cx| {
        assert_eq!(
            chat.input_state.read(cx).value().as_ref(),
            "First task draft"
        );
        assert_eq!(chat.pasted_images[0].display_name, "first.png");
    });
    retained_model.update(cx, |state, cx| {
        state.active_session_id = None;
        cx.notify();
    });
    cx.run_until_parked();
    chat.update_in(cx, |chat, window, cx| {
        assert!(chat.input_state.read(cx).value().is_empty());
        assert!(chat.pasted_images.is_empty());
        chat.input_state.update(cx, |input, cx| {
            input.set_value("New task draft", window, cx);
        });
    });
    retained_model.update(cx, |state, cx| {
        state.active_work_dir = Some("/projects/two".into());
        cx.notify();
    });
    cx.run_until_parked();
    chat.update(cx, |chat, cx| {
        assert!(chat.input_state.read(cx).value().is_empty())
    });
    retained_model.update(cx, |state, cx| {
        state.active_work_dir = Some("/projects/one".into());
        cx.notify();
    });
    cx.run_until_parked();
    chat.update(cx, |chat, cx| {
        assert_eq!(chat.input_state.read(cx).value().as_ref(), "New task draft");
    });
}

#[gpui::test]
fn terminal_excerpt_append_targets_the_current_draft_only(cx: &mut gpui::TestAppContext) {
    use gpui::{AppContext as _, Focusable};

    cx.update(gpui_component::init);
    let model = cx.new(|_| {
        let mut state = threadlane_ui_state::AppState::default();
        state.active_work_dir = Some("/projects/one".into());
        state.active_session_id = Some("task".into());
        state
    });
    let retained_model = model.clone();
    let (chat, cx) =
        cx.add_window_view(move |window, cx| super::ChatListView::new(model, window, cx));
    chat.update_in(cx, |chat, window, cx| {
        chat.input_state.update(cx, |input, cx| {
            input.set_value("Question:", window, cx);
        });
        chat.pasted_images.push(super::ImageAttachment {
            display_name: "shot.png".into(),
            data_url: "data:image/png;base64,x".into(),
        });
        chat.set_tab(super::CentralTab::Editor, cx);

        // A stale destination is rejected and leaves the draft untouched.
        let stale = (Some("/projects/other".into()), Some("task".into()));
        assert!(!chat.append_draft_text_for(stale, "excerpt", window, cx));
        assert_eq!(chat.input_state.read(cx).value().as_ref(), "Question:");

        let destination = (Some("/projects/one".into()), Some("task".into()));
        let excerpt = "Terminal · Shell 1 · launched in /projects/one\n```\nbuild failed\n```";
        assert!(chat.append_draft_text_for(destination, excerpt, window, cx));
        assert_eq!(
            chat.input_state.read(cx).value().as_ref(),
            "Question:\nTerminal · Shell 1 · launched in /projects/one\n```\nbuild failed\n```"
        );
        assert_eq!(
            chat.pasted_images.len(),
            1,
            "staged images survive the append"
        );
        assert_eq!(chat.current_tab, super::CentralTab::Chat);
        assert!(chat
            .input_state
            .read(cx)
            .focus_handle(cx)
            .is_focused(window));
    });
    retained_model.read_with(cx, |state, _| {
        assert!(state.messages.is_empty(), "the handoff never sends");
    });
}

use super::{
    active_slash_command_query, build_trajectory_rows, build_transcript_rows, classify_chat_link,
    classify_markdown_update, contains_case_insensitive, context_meter_view_model,
    editor_target_matches_active_work_dir, extend_trajectory_facets, extend_trajectory_previews,
    extend_trajectory_rows, extend_trajectory_summary, extract_markdown_segments,
    format_trajectory_raw_json, grouped_tool_activities, is_terminal_runnable_language,
    markdown_cache_exceeded, next_chat_stream_batch, normalize_terminal_command,
    reconcile_trajectory_entries, reconcile_trajectory_entries_by_epoch, subagent_popover_counts,
    summarize_trajectory, ChatLinkTarget, ContextMeterContext, ContextMeterMetrics,
    MarkdownSegment, MarkdownUpdate, PromptRecallStep, TrajectoryCacheKey, TrajectoryInspectorTab,
    TrajectoryMode, TrajectoryRenderCache, TrajectoryRow, TrajectorySummary,
    TranscriptRow, INPUT_KEY_CONTEXT, MARKDOWN_CACHE_ENTRY_LIMIT, SLASH_COMMAND_BINDING_CONTEXT,
    SLASH_COMMAND_KEY_CONTEXT,
};

#[gpui::test]
fn chat_errors_are_bounded_deduplicated_and_keep_recovery_details(cx: &mut gpui::TestAppContext) {
    use gpui::AppContext as _;

    struct ErrorHarness {
        model: gpui::Entity<threadlane_ui_state::AppState>,
        error: String,
    }

    impl gpui::Render for ErrorHarness {
        fn render(
            &mut self,
            _: &mut gpui::Window,
            cx: &mut gpui::Context<Self>,
        ) -> impl gpui::IntoElement {
            super::render_chat_error("test", &self.error, &self.model, cx)
        }
    }

    let error = format!(
        "Provider HTTP 401: {{\"error\":{{\"code\":\"token_expired\",\"detail\":\"{}\"}}}}",
        "diagnostic ".repeat(1_000)
    );
    let (summary, needs_settings) = super::chat_error_summary(&error);
    assert!(needs_settings);
    assert!(summary.contains("Sign in again"));
    assert!(!summary.contains("token_expired"));
    assert!(summary.chars().count() < 240);
    assert_eq!(
        super::chat_error_summary(&"界".repeat(500))
            .0
            .chars()
            .count(),
        241
    );
    assert_eq!(
        super::chat_error_summary("\nNetwork failed\nraw detail").0,
        "Network failed"
    );

    let mut message = threadlane_ui_state::ChatMessageInfo {
        id: "error".into(),
        role: threadlane_ui_state::MessageRole::Error,
        content: error.clone(),
        tool_activities: Vec::new(),
        streaming: false,
        reasoning_content: None,
        reasoning_expanded: false,
    };
    assert!(super::visible_session_status(Some(&error), Some(&message)).is_none());
    assert_eq!(
        super::visible_session_status(Some("Message queued…"), Some(&message)),
        Some("Message queued…")
    );
    message.role = threadlane_ui_state::MessageRole::Assistant;
    assert_eq!(
        super::visible_session_status(Some(&error), Some(&message)),
        Some(error.as_str())
    );

    cx.update(gpui_component::init);
    let model = cx.new(|_| threadlane_ui_state::AppState::default());
    let expected_error = error.clone();
    let retained_model = model.clone();
    let (_, cx) = cx.add_window_view(move |window, cx| {
        let view = cx.new(|_| ErrorHarness { model, error });
        gpui_component::Root::new(view, window, cx)
    });
    cx.update(|window, cx| window.draw(cx).clear(cx));
    let copy = cx.debug_bounds("chat-error-copy").unwrap();
    cx.simulate_click(copy.center(), gpui::Modifiers::default());
    assert_eq!(
        cx.read_from_clipboard().and_then(|item| item.text()),
        Some(expected_error)
    );

    let settings = cx.debug_bounds("chat-error-settings").unwrap();
    cx.simulate_click(settings.center(), gpui::Modifiers::default());
    assert_eq!(
        retained_model.read_with(cx, |model, _| model.workspace_page),
        threadlane_ui_state::WorkspacePage::Settings
    );
}

#[test]
fn last_retryable_prompt_prefers_the_latest_user_message() {
    let user = |id: &str, content: &str| ChatMessageInfo {
        id: id.into(),
        role: MessageRole::User,
        content: content.into(),
        tool_activities: Vec::new(),
        streaming: false,
        reasoning_content: None,
        reasoning_expanded: false,
    };
    let assistant = |id: &str| ChatMessageInfo {
        id: id.into(),
        role: MessageRole::Assistant,
        content: "done".into(),
        tool_activities: Vec::new(),
        streaming: false,
        reasoning_content: None,
        reasoning_expanded: false,
    };
    assert_eq!(super::last_retryable_prompt(&[]), None);
    assert_eq!(super::last_retryable_prompt(&[assistant("a")]), None);
    assert_eq!(
        super::last_retryable_prompt(&[user("u", "   ")]),
        None,
        "whitespace-only prompts cannot be resent"
    );
    assert_eq!(
        super::last_retryable_prompt(&[user("u1", "first"), assistant("a"), user("u2", "second")]),
        Some("second".to_string()),
        "retry resends the latest user prompt, not an earlier one"
    );
}

#[gpui::test]
fn error_retry_appears_only_with_a_user_prompt_and_no_active_run(
    cx: &mut gpui::TestAppContext,
) {
    use gpui::AppContext as _;

    struct ErrorHarness {
        model: gpui::Entity<threadlane_ui_state::AppState>,
        error: String,
    }

    impl gpui::Render for ErrorHarness {
        fn render(
            &mut self,
            _: &mut gpui::Window,
            cx: &mut gpui::Context<Self>,
        ) -> impl gpui::IntoElement {
            super::render_chat_error("retry-test", &self.error, &self.model, cx)
        }
    }

    let message = |role: MessageRole, content: &str| ChatMessageInfo {
        id: format!("{role:?}-retry"),
        role,
        content: content.into(),
        tool_activities: Vec::new(),
        streaming: false,
        reasoning_content: None,
        reasoning_expanded: false,
    };
    let show_error = |cx: &mut gpui::TestAppContext,
                        model: gpui::Entity<threadlane_ui_state::AppState>| {
        let (_, cx) = cx.add_window_view(move |window, cx| {
            let view = cx.new(|_| ErrorHarness {
                model,
                error: "boom".into(),
            });
            gpui_component::Root::new(view, window, cx)
        });
        cx.update(|window, cx| window.draw(cx).clear(cx));
        cx.debug_bounds("chat-error-retry").is_some()
    };

    cx.update(gpui_component::init);
    let without_prompt = cx.new(|_| threadlane_ui_state::AppState::default());
    assert!(
        !show_error(cx, without_prompt),
        "no user prompt means nothing to resend"
    );

    let with_prompt = cx.new(|_| {
        let mut state = threadlane_ui_state::AppState::default();
        state.messages = vec![
            message(MessageRole::User, "try again"),
            message(MessageRole::Error, "boom"),
        ]
        .into();
        state
    });
    assert!(
        show_error(cx, with_prompt),
        "a failed turn with a prior user prompt offers Retry"
    );

    let generating = cx.new(|_| {
        let mut state = threadlane_ui_state::AppState::default();
        state.messages = vec![message(MessageRole::User, "try again")].into();
        state.is_generating = true;
        state
    });
    assert!(
        !show_error(cx, generating),
        "retry stays hidden while a generation is running"
    );
}

#[test]
fn editor_targets_only_open_for_the_active_git_checkout() {
    let worktree = std::path::Path::new("/projects/app/.threadlane/worktrees/session");
    let canonical = std::path::Path::new("/projects/app");

    assert!(editor_target_matches_active_work_dir(
        worktree,
        Some(worktree)
    ));
    assert!(!editor_target_matches_active_work_dir(
        worktree,
        Some(canonical)
    ));
    assert!(!editor_target_matches_active_work_dir(worktree, None));
}
use threadlane_ui_state::{
    reported_session_shape_state, ChatMessageInfo, ChatStreamEvent, MessageRole,
    SubagentActivityStatus, ToolActivityInfo, TrajectoryDiagnostics, TrajectoryEntry,
};

#[test]
fn markdown_cache_resets_only_after_its_limit() {
    assert!(!markdown_cache_exceeded(MARKDOWN_CACHE_ENTRY_LIMIT));
    assert!(markdown_cache_exceeded(MARKDOWN_CACHE_ENTRY_LIMIT + 1));
}

#[tokio::test]
async fn chat_stream_batch_waits_then_caps_ready_events() {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    assert!(tokio::time::timeout(
        std::time::Duration::from_millis(10),
        next_chat_stream_batch(&mut rx),
    )
    .await
    .is_err());
    for index in 0..130 {
        tx.send(ChatStreamEvent::Finished {
            session_id: index.to_string(),
            session_file: std::path::PathBuf::new(),
        })
        .unwrap();
    }
    assert_eq!(next_chat_stream_batch(&mut rx).await.unwrap().len(), 128);
    assert_eq!(next_chat_stream_batch(&mut rx).await.unwrap().len(), 2);
}

#[test]
fn trajectory_search_matches_ascii_without_case_sensitivity() {
    assert!(contains_case_insensitive("Read File", "read"));
    assert!(contains_case_insensitive("TOOL-CALL-42", "call-42"));
    assert!(!contains_case_insensitive("Write File", "read"));
}

#[test]
fn subagent_popover_counts_items_without_owning_them() {
    assert_eq!(subagent_popover_counts([]), None);
    assert_eq!(
        subagent_popover_counts([
            SubagentActivityStatus::Queued,
            SubagentActivityStatus::Running,
            SubagentActivityStatus::Completed,
        ]),
        Some((3, 2))
    );
}

#[test]
fn trajectory_search_preserves_unicode_lowercase_matching() {
    assert!(contains_case_insensitive("CAFÉ output", "café"));
    assert!(contains_case_insensitive("Kelvin", "kelvin"));
    assert!(!contains_case_insensitive("CAFÉ output", "résumé"));
}

#[test]
fn chat_link_classifies_web_urls_as_external() {
    assert_eq!(
        classify_chat_link("https://example.com/spec"),
        ChatLinkTarget::Web
    );
    assert_eq!(
        classify_chat_link("http://example.com/spec"),
        ChatLinkTarget::Web
    );
}

#[test]
fn chat_link_normalizes_safe_project_relative_paths() {
    assert_eq!(
        classify_chat_link("docs/spec.md"),
        ChatLinkTarget::ProjectFile("docs/spec.md".into())
    );
    assert_eq!(
        classify_chat_link("docs/design/../spec.md"),
        ChatLinkTarget::ProjectFile("docs/spec.md".into())
    );
}

#[test]
fn chat_link_rejects_absolute_and_escaping_paths() {
    assert_eq!(classify_chat_link("/tmp/spec.md"), ChatLinkTarget::Rejected);
    assert_eq!(
        classify_chat_link("../../outside.md"),
        ChatLinkTarget::Rejected
    );
}

#[test]
fn chat_link_does_not_parse_line_or_fragment_suffixes() {
    assert_eq!(
        classify_chat_link("src/main.rs:42"),
        ChatLinkTarget::ProjectFile("src/main.rs:42".into())
    );
    assert_eq!(
        classify_chat_link("src/main.rs#L42"),
        ChatLinkTarget::ProjectFile("src/main.rs#L42".into())
    );
}
fn metrics_with_usage(
    input_tokens: u64,
    output_tokens: u64,
    cache_read_tokens: u64,
    cache_write_tokens: u64,
) -> ContextMeterMetrics {
    let billed_input_tokens = input_tokens
        .saturating_add(cache_read_tokens)
        .saturating_add(cache_write_tokens);
    ContextMeterMetrics {
        billed_input_tokens,
        output_tokens,
        cache_hit_percent: (billed_input_tokens > 0).then(|| {
            (((cache_read_tokens as u128) * 100 + (billed_input_tokens as u128) / 2)
                / billed_input_tokens as u128) as u64
        }),
    }
}

fn estimating_context() -> ContextMeterContext {
    ContextMeterContext {
        current_tokens: 0,
        context_limit: 0,
        context_limit_is_estimate: false,
        effective_model: "new-model".into(),
        last_compaction_seq: None,
        provisional: false,
        estimating: true,
    }
}

#[test]
fn a_model_that_never_reports_usage_is_not_shown_as_estimating() {
    // An ACP agent runs its own loop and sends no token accounting, so the
    // meter has nothing to project. Rendering that as "Estimating…" both
    // promises a number that never arrives and leaves the badge animating
    // for the whole turn.
    let view = context_meter_view_model(None, &ContextMeterMetrics::default(), false);
    assert_eq!(view.current_label, "Not reported");
    assert_eq!(view.percent, None);
    assert_eq!(view.bar_percent, 0.0);

    // A model that does report usage keeps the pending label until a
    // context figure arrives.
    let estimating = context_meter_view_model(None, &ContextMeterMetrics::default(), true);
    assert_eq!(estimating.current_label, "Unavailable");
}

#[test]
fn an_unreported_context_still_shows_what_was_processed() {
    // Turn counts and any usage Threadlane did observe stay meaningful
    // even when the context window itself is unmeasurable.
    let view = context_meter_view_model(
        None,
        &ContextMeterMetrics {
            billed_input_tokens: 1_200,
            output_tokens: 800,
            cache_hit_percent: Some(40),
        },
        false,
    );
    assert_eq!(view.total_processed_label, "2.0k");
    assert_eq!(view.cache_hit_label.as_deref(), Some("40%"));
}

#[tokio::test]
async fn meter_separates_current_context_from_total_processed() {
    let (_path, state) = reported_session_shape_state().await;
    let _projected_context = state.active_context_window().unwrap();
    let projected_metrics = state.active_session_metrics();
    let view = context_meter_view_model(
        Some(&ContextMeterContext {
            current_tokens: 38_278,
            context_limit: 128_000,
            context_limit_is_estimate: false,
            effective_model: "gpt-4o".into(),
            last_compaction_seq: None,
            provisional: false,
            estimating: false,
        }),
        &ContextMeterMetrics {
            billed_input_tokens: projected_metrics.billed_input_tokens(),
            output_tokens: projected_metrics.output_tokens,
            cache_hit_percent: projected_metrics.cache_hit_percent(),
        },
        true,
    );
    let percent = view.percent.expect("known context percentage");
    assert!((percent - 29.904_687_5).abs() < 1e-12);
    assert!((view.bar_percent - 29.904_687_5).abs() < 1e-12);
    assert_eq!(view.current_label, "38.3k / 128.0k");
    assert!(view.total_processed_label.ends_with('M'));
    assert_ne!(view.total_processed_label, view.current_label);
    assert!(view.cache_hit_label.is_some());
    assert_eq!(view.detail_label, "Context usage details, 30% used");
}

#[test]
fn meter_estimating_context_has_no_false_percentage() {
    let view = context_meter_view_model(
        Some(&estimating_context()),
        &ContextMeterMetrics::default(),
        true,
    );
    assert_eq!(view.percent, None);
    assert_eq!(view.current_label, "Unavailable");
    assert_eq!(view.bar_percent, 0.0);
    assert_eq!(view.detail_label, "Context usage details, current usage unavailable");
}

#[test]
fn meter_treats_zero_context_limit_as_unknown_even_when_not_estimating() {
    let mut context = estimating_context();
    context.current_tokens = 42;
    context.estimating = false;

    let view = context_meter_view_model(Some(&context), &ContextMeterMetrics::default(), true);

    assert_eq!(view.percent, None);
    assert_eq!(view.current_label, "Unavailable");
    assert_eq!(view.bar_percent, 0.0);
    assert_eq!(view.detail_label, "Context usage details, current usage unavailable");
}

#[test]
fn meter_cache_hit_rounding_uses_wide_intermediates_at_u64_max() {
    let metrics = metrics_with_usage(0, 0, u64::MAX, 0);

    assert_eq!(metrics.billed_input_tokens, u64::MAX);
    assert_eq!(metrics.cache_hit_percent, Some(100));
}

#[test]
fn meter_labels_estimated_limit_and_clamps_only_bar() {
    let view = context_meter_view_model(
        Some(&ContextMeterContext {
            current_tokens: 120_000,
            context_limit: 100_000,
            context_limit_is_estimate: true,
            effective_model: "model".into(),
            last_compaction_seq: Some(42),
            provisional: true,
            estimating: false,
        }),
        &ContextMeterMetrics::default(),
        true,
    );
    assert_eq!(view.percent, Some(120.0));
    assert_eq!(view.bar_percent, 100.0);
    assert_eq!(view.current_label, "120.0k / ~100.0k");
    assert_eq!(view.last_compaction_seq, Some(42));
    assert!(view.provisional);
}

#[test]
fn markdown_update_appends_only_the_new_suffix() {
    assert_eq!(
        classify_markdown_update("Hello", "Hello **world**"),
        MarkdownUpdate::Append(" **world**")
    );
}

#[test]
fn markdown_update_skips_identical_content() {
    assert_eq!(
        classify_markdown_update("Hello", "Hello"),
        MarkdownUpdate::Unchanged
    );
}

#[test]
fn markdown_update_replaces_non_append_changes() {
    assert_eq!(
        classify_markdown_update("Hello", "Jello"),
        MarkdownUpdate::Replace
    );
    assert_eq!(
        classify_markdown_update("Hello", "Hello there"),
        MarkdownUpdate::Append(" there")
    );
    assert_eq!(
        classify_markdown_update("Hello there", "Hello"),
        MarkdownUpdate::Replace
    );
    assert_eq!(
        classify_markdown_update("Hello", "Hello!"),
        MarkdownUpdate::Append("!")
    );
    assert_eq!(
        classify_markdown_update("Hello", "Jello there"),
        MarkdownUpdate::Replace
    );
}

#[test]
fn grouped_tool_activities_borrows_in_order_and_hides_plan_updates() {
    let activity_message = |activities: &[(&str, &str)]| ChatMessageInfo {
        id: activities[0].0.into(),
        role: MessageRole::Assistant,
        content: String::new(),
        tool_activities: activities
            .iter()
            .map(|(id, title)| ToolActivityInfo {
                id: (*id).into(),
                category: "tool".into(),
                title: (*title).into(),
                display_summary: String::new(),
                detail: String::new(),
                is_expanded: false,
            })
            .collect(),
        streaming: false,
        reasoning_content: None,
        reasoning_expanded: false,
    };
    let messages = vec![
        activity_message(&[("tool-1", "read_file"), ("plan", "update_plan")]),
        activity_message(&[("tool-2", "write_file")]),
    ];

    let ids = grouped_tool_activities(&messages)
        .map(|activity| activity.id.as_str())
        .collect::<Vec<_>>();

    assert_eq!(ids, vec!["tool-1", "tool-2"]);
}

#[test]
fn progress_summary_never_reuses_a_previous_turns_tool() {
    let user = ChatMessageInfo {
        id: "prompt".into(),
        role: MessageRole::User,
        content: "Follow up".into(),
        tool_activities: vec![],
        streaming: false,
        reasoning_content: None,
        reasoning_expanded: false,
    };
    let mut activity = user.clone();
    activity.role = MessageRole::Assistant;
    activity.content.clear();
    activity.tool_activities.push(ToolActivityInfo {
        id: "read".into(),
        category: "Completed".into(),
        title: "read_file".into(),
        display_summary: "Read README.md".into(),
        detail: "Old turn output".into(),
        is_expanded: false,
    });
    let mut messages = vec![activity.clone(), user];
    assert!(super::current_turn_latest_tool(&messages).is_none());
    activity.tool_activities[0].id = "current".into();
    messages.push(activity);
    assert_eq!(
        super::current_turn_latest_tool(&messages).unwrap().id,
        "current"
    );
    for prefix in ["queued-user", "steered-user"] {
        let mut pending = messages[1].clone();
        pending.id = format!("{prefix}-session-3");
        messages.push(pending);
        assert_eq!(
            super::current_turn_latest_tool(&messages).unwrap().id,
            "current"
        );
    }
    assert!(super::current_turn_latest_tool(&[]).is_none());
}

#[test]
fn transcript_rows_group_consecutive_tool_only_messages() {
    let message = |id: &str, activity: bool| ChatMessageInfo {
        id: id.into(),
        role: if activity {
            MessageRole::Assistant
        } else {
            MessageRole::User
        },
        content: if activity { "" } else { id }.into(),
        tool_activities: activity
            .then(|| ToolActivityInfo {
                id: format!("tool-{id}"),
                category: "tool".into(),
                title: "read_file".into(),
                display_summary: String::new(),
                detail: String::new(),
                is_expanded: false,
            })
            .into_iter()
            .collect(),
        streaming: false,
        reasoning_content: None,
        reasoning_expanded: false,
    };
    let messages = vec![
        message("user", false),
        message("tool-1", true),
        message("tool-2", true),
        message("answer", false),
    ];

    assert_eq!(
        build_transcript_rows(&messages, true),
        vec![
            TranscriptRow::Message(0),
            TranscriptRow::Activities(1..3),
            TranscriptRow::Message(3),
            TranscriptRow::Working,
        ]
    );
}

#[test]
fn queued_messages_leave_transcript_only_while_generating() {
    let messages: Vec<_> = [
        "user",
        "queued-user-session-1",
        "steered-user-session-2",
        "queued-user-session-3",
    ]
    .into_iter()
    .map(|id| ChatMessageInfo {
        id: id.into(),
        role: MessageRole::User,
        content: id.into(),
        tool_activities: Vec::new(),
        streaming: false,
        reasoning_content: None,
        reasoning_expanded: false,
    })
    .collect();
    assert_eq!(
        build_transcript_rows(&messages, true),
        vec![
            TranscriptRow::Message(0),
            TranscriptRow::Message(2),
            TranscriptRow::Working
        ]
    );
    assert_eq!(
        build_transcript_rows(&messages, false),
        (0..4).map(TranscriptRow::Message).collect::<Vec<_>>()
    );
}

#[gpui::test]
fn queue_filter_transitions_keep_retained_list_in_sync(cx: &mut gpui::TestAppContext) {
    use gpui::AppContext as _;

    cx.update(gpui_component::init);
    let model = cx.new(|_| threadlane_ui_state::AppState::default());
    let (chat, cx) = cx.add_window_view(move |window, cx| {
        super::ChatListView::new(model, window, cx)
    });
    chat.update(cx, |chat, _| {
        for queued_count in 0..=3 {
            let messages = std::sync::Arc::new(
                (0..=queued_count)
                    .map(|index| ChatMessageInfo {
                        id: if index == 0 {
                            "user".into()
                        } else {
                            format!("queued-user-session-{index}")
                        },
                        role: MessageRole::User,
                        content: format!("Message {index}"),
                        tool_activities: Vec::new(),
                        streaming: false,
                        reasoning_content: None,
                        reasoning_expanded: false,
                    })
                    .collect::<Vec<_>>(),
            );
            chat.sync_transcript_rows(messages.clone(), true, true);
            for generating in [false, true, false] {
                chat.sync_transcript_rows(messages.clone(), generating, false);
                let expected = build_transcript_rows(&messages, generating);
                assert_eq!(chat.transcript_rows, expected);
                assert_eq!(
                    chat.transcript_list_state.item_count(),
                    expected.len(),
                    "queue size {queued_count}, generating {generating}"
                );
            }
        }
    });
}

#[gpui::test]
fn queued_panel_tracks_active_messages_and_generation(cx: &mut gpui::TestAppContext) {
    use gpui::AppContext as _;
    cx.update(gpui_component::init);
    let model = cx.new(|_| {
        let mut state = threadlane_ui_state::AppState::default();
        state.is_new_task = false;
        state.is_generating = true;
        threadlane_ui_state::activate_test_session(
            &mut state,
            "session-1",
            std::path::Path::new("/test-project/.threadlane/sessions/session-1.jsonl"),
        );
        state.messages = (0..2)
            .map(|index| ChatMessageInfo {
                id: format!("queued-user-session-1-{index}"),
                role: MessageRole::User,
                content: format!("Follow-up {index}\nKeep the full multiline message visible"),
                tool_activities: Vec::new(),
                streaming: false,
                reasoning_content: None,
                reasoning_expanded: false,
            })
            .collect::<Vec<_>>()
            .into();
        state
    });
    let retained_model = model.clone();
    let (_, cx) = cx.add_window_view(move |window, cx| {
        let chat = cx.new(|cx| super::ChatListView::new(model, window, cx));
        gpui_component::Root::new(chat, window, cx)
    });
    cx.run_until_parked();
    cx.update(|window, cx| window.draw(cx).clear(cx));
    assert!(cx.debug_bounds("queued-messages-panel").is_some());
    assert!(cx.debug_bounds("queued-message-row").is_some());
    retained_model.update(cx, |state, cx| {
        state.is_generating = false;
        cx.notify();
    });
    cx.run_until_parked();
    cx.update(|window, cx| window.draw(cx).clear(cx));
    assert!(cx.debug_bounds("queued-messages-panel").is_none());
    retained_model.update(cx, |state, cx| {
        state.is_generating = true;
        cx.notify();
    });
    cx.run_until_parked();
    cx.update(|window, cx| window.draw(cx).clear(cx));
    assert!(cx.debug_bounds("queued-messages-panel").is_some());
    retained_model.update(cx, |state, cx| {
        state.messages = Vec::new().into();
        cx.notify();
    });
    cx.run_until_parked();
    cx.update(|window, cx| window.draw(cx).clear(cx));
    assert!(cx.debug_bounds("queued-messages-panel").is_none());
}

#[test]
fn selected_trajectory_entry_formats_as_raw_json() {
    let entry = TrajectoryEntry {
        seq: Some(1),
        run_id: None,
        turn: None,
        request: None,
        category: "Tool".into(),
        summary: "Read file".into(),
        detail: "src/main.rs".into(),
        lane: None,
        correlation_id: None,
        diagnostics: TrajectoryDiagnostics::default(),
    };

    let raw = format_trajectory_raw_json(&entry);

    assert!(raw.contains("\"category\": \"Tool\""));
    assert!(raw.contains("\"summary\": \"Read file\""));
}

fn trajectory_entry(category: &str, request: Option<u32>, turn: Option<u32>) -> TrajectoryEntry {
    TrajectoryEntry {
        seq: None,
        run_id: None,
        turn,
        request,
        category: category.into(),
        summary: category.into(),
        detail: String::new(),
        lane: None,
        correlation_id: None,
        diagnostics: TrajectoryDiagnostics::default(),
    }
}

#[test]
fn trajectory_cache_reuses_entries_for_append_only_updates() {
    let cached = vec![trajectory_entry("Input", Some(1), Some(1))];
    let source = vec![
        cached[0].clone(),
        trajectory_entry("Tool", Some(1), Some(1)),
    ];

    assert_eq!(reconcile_trajectory_entries(cached, &source), source);
}

#[test]
fn trajectory_cache_replaces_entries_when_existing_data_changes() {
    let cached = vec![trajectory_entry("Input", Some(1), Some(1))];
    let source = vec![trajectory_entry("Assistant", Some(1), Some(1))];

    assert_eq!(reconcile_trajectory_entries(cached, &source), source);
}

#[test]
fn trajectory_epoch_distinguishes_append_from_replacement() {
    let cached = vec![trajectory_entry("Input", Some(1), Some(1))];
    let appended_source = vec![
        cached[0].clone(),
        trajectory_entry("Tool", Some(1), Some(1)),
    ];
    let (entries, appended) =
        reconcile_trajectory_entries_by_epoch(cached.clone(), &appended_source, 7, 7);
    assert!(appended);
    assert_eq!(entries, appended_source);

    let replacement = vec![trajectory_entry("Assistant", Some(1), Some(1))];
    let (entries, appended) = reconcile_trajectory_entries_by_epoch(cached, &replacement, 7, 8);
    assert!(!appended);
    assert_eq!(entries, replacement);
}

#[test]
fn trajectory_incremental_facets_match_full_rebuild() {
    let mut input = trajectory_entry("Input", Some(1), Some(1));
    input.lane = Some("main".into());
    let mut tool = trajectory_entry("Tool", Some(1), Some(1));
    tool.lane = Some("main".into());
    let mut anomaly = trajectory_entry("Anomaly", Some(1), Some(2));
    anomaly.lane = Some("child".into());
    let appended_tool = trajectory_entry("Tool", Some(1), Some(2));
    let entries = vec![input, tool, anomaly, appended_tool];
    let key = TrajectoryCacheKey {
        revision: 4,
        epoch: 1,
        mode: TrajectoryMode::Execution,
        query: "tool".into(),
        category: None,
        lane: None,
    };

    let mut incremental = (Vec::new(), std::collections::BTreeMap::new(), Vec::new());
    extend_trajectory_facets(
        &mut incremental.0,
        &mut incremental.1,
        &mut incremental.2,
        &entries[..2],
        0,
        &key,
    );
    extend_trajectory_facets(
        &mut incremental.0,
        &mut incremental.1,
        &mut incremental.2,
        &entries,
        2,
        &key,
    );

    let mut rebuilt = (Vec::new(), std::collections::BTreeMap::new(), Vec::new());
    extend_trajectory_facets(
        &mut rebuilt.0,
        &mut rebuilt.1,
        &mut rebuilt.2,
        &entries,
        0,
        &key,
    );
    assert_eq!(incremental, rebuilt);
    assert_eq!(incremental.0, vec!["Anomaly", "Input", "Tool"]);
    assert_eq!(incremental.2, vec![1, 3]);
}

#[test]
fn trajectory_previews_extend_without_reformatting_existing_entries() {
    let mut input = trajectory_entry("Input", Some(1), Some(1));
    input.summary = "Prompt".into();
    input.detail = "first\nsecond".into();
    let mut tool = trajectory_entry("Tool", Some(1), Some(1));
    tool.summary = "read_file".into();
    tool.detail.clear();
    let entries = vec![input, tool];
    let mut previews = Vec::new();

    extend_trajectory_previews(&mut previews, &entries[..1], 0);
    extend_trajectory_previews(&mut previews, &entries, 1);

    assert_eq!(previews, ["Prompt  first second", "read_file"]);
}

#[test]
fn trajectory_rows_preserve_request_headers_and_setup_boundaries() {
    let entries = vec![
        trajectory_entry("Provider", Some(1), Some(1)),
        trajectory_entry("Input", Some(1), Some(1)),
        trajectory_entry("Input", Some(2), Some(2)),
    ];

    assert_eq!(
        build_trajectory_rows(&entries, &[0, 1, 2], TrajectoryMode::Requests),
        vec![
            TrajectoryRow::RequestHeader(1),
            TrajectoryRow::Setup,
            TrajectoryRow::Entry(0),
            TrajectoryRow::Entry(1),
            TrajectoryRow::RequestHeader(2),
            TrajectoryRow::Entry(2),
        ]
    );
}

#[test]
fn trajectory_incremental_rows_match_full_rebuild() {
    let entries = vec![
        trajectory_entry("Provider", Some(1), Some(1)),
        trajectory_entry("Input", Some(1), Some(1)),
        trajectory_entry("Input", Some(2), Some(2)),
    ];
    let indices = [0, 1, 2];
    let mut incremental = Vec::new();
    extend_trajectory_rows(
        &mut incremental,
        &entries,
        &indices[..2],
        0,
        TrajectoryMode::Requests,
    );
    extend_trajectory_rows(
        &mut incremental,
        &entries,
        &indices,
        2,
        TrajectoryMode::Requests,
    );

    assert_eq!(
        incremental,
        build_trajectory_rows(&entries, &indices, TrajectoryMode::Requests)
    );
}

#[test]
fn trajectory_summary_is_computed_once_from_canonical_entries() {
    let mut tool = trajectory_entry("Tool", Some(1), Some(3));
    tool.diagnostics.duration_ms = Some(25);
    let mut anomaly = trajectory_entry("Anomaly", Some(1), Some(4));
    anomaly.diagnostics.duration_ms = Some(75);
    anomaly.diagnostics.is_anomaly = true;

    let summary = summarize_trajectory(&[tool, anomaly]);

    assert_eq!(summary.tool_count, 1);
    assert_eq!(summary.total_duration_ms, 100);
    assert_eq!(summary.anomaly_count, 1);
    assert_eq!(summary.max_turn, 4);
}

#[test]
fn trajectory_summary_append_matches_full_rebuild() {
    let initial = vec![trajectory_entry("Input", Some(1), Some(1))];
    let mut tool = trajectory_entry("Tool", Some(1), Some(1));
    tool.diagnostics.duration_ms = Some(25);
    let mut anomaly = trajectory_entry("Anomaly", Some(1), Some(2));
    anomaly.diagnostics.duration_ms = Some(75);
    let appended = vec![tool, anomaly];
    let mut incremental = summarize_trajectory(&initial);
    extend_trajectory_summary(&mut incremental, &appended);

    let rebuilt = summarize_trajectory(&initial.into_iter().chain(appended).collect::<Vec<_>>());
    assert_eq!(incremental.overview_positions, rebuilt.overview_positions);
    assert_eq!(incremental.tool_count, rebuilt.tool_count);
    assert_eq!(incremental.total_duration_ms, 100);
    assert_eq!(incremental.anomaly_count, rebuilt.anomaly_count);
    assert_eq!(incremental.max_turn, rebuilt.max_turn);
}
#[test]
fn trajectory_cache_key_changes_with_data_or_filter() {
    let base = TrajectoryCacheKey {
        revision: 7,
        epoch: 2,
        mode: TrajectoryMode::Execution,
        query: "tool".into(),
        category: None,
        lane: None,
    };
    let mut changed = base.clone();
    changed.revision += 1;
    assert_ne!(base, changed);

    let mut changed = base.clone();
    changed.query = "provider".into();
    assert_ne!(base, changed);
}

#[test]
fn extract_markdown_segments_parses_mixed_content_with_paths() {
    let input = "Here is the command:\n```bash\ncargo build --workspace\n```\nAnd code in file:\n```rust src/main.rs\n// src/main.rs\nfn main() {}\n```\nFinished!";
    let segments = extract_markdown_segments(input);
    assert_eq!(segments.len(), 5);
    assert_eq!(
        segments[0],
        MarkdownSegment::Markdown("Here is the command:\n".into())
    );
    assert_eq!(
        segments[1],
        MarkdownSegment::CodeBlock {
            language: "bash".into(),
            header_path: None,
            code: "cargo build --workspace\n".into(),
        }
    );
    assert_eq!(
        segments[2],
        MarkdownSegment::Markdown("And code in file:\n".into())
    );
    assert_eq!(
        segments[3],
        MarkdownSegment::CodeBlock {
            language: "rust".into(),
            header_path: Some("src/main.rs".into()),
            code: "// src/main.rs\nfn main() {}\n".into(),
        }
    );
    assert_eq!(segments[4], MarkdownSegment::Markdown("Finished!".into()));
}

#[test]
fn extract_markdown_segments_handles_comment_path_heuristic() {
    let input = "```typescript\n// app/routes/index.tsx\nexport default function Home() {}\n```";
    let segments = extract_markdown_segments(input);
    assert_eq!(segments.len(), 1);
    assert_eq!(
        segments[0],
        MarkdownSegment::CodeBlock {
            language: "typescript".into(),
            header_path: Some("app/routes/index.tsx".into()),
            code: "// app/routes/index.tsx\nexport default function Home() {}\n".into(),
        }
    );
}

#[test]
fn extract_markdown_segments_rejects_version_and_shebang_as_paths() {
    for input in [
        "```sh\n#!/bin/sh\necho hi\n```",
        "```rust\n// v1.2\nfn main() {}\n```",
    ] {
        let segments = extract_markdown_segments(input);
        assert!(matches!(
            segments.as_slice(),
            [MarkdownSegment::CodeBlock {
                header_path: None,
                ..
            }]
        ));
    }
}

#[test]
fn extract_markdown_segments_accepts_indented_closing_fence() {
    let segments = extract_markdown_segments("```rust\nlet x = 1;\n  ```\nAfter");
    assert!(
        matches!(segments.as_slice(), [MarkdownSegment::CodeBlock { .. }, MarkdownSegment::Markdown(text)] if text == "After")
    );
}

#[test]
fn extract_markdown_segments_keeps_text_before_mid_line_backticks() {
    let segments = extract_markdown_segments("Prefix ```inline\n```rust\ncode\n```");
    assert!(
        matches!(segments.as_slice(), [MarkdownSegment::Markdown(text), MarkdownSegment::CodeBlock { language, .. }] if text == "Prefix ```inline\n" && language == "rust")
    );
}

#[test]
fn normalize_terminal_command_removes_prompt_markers() {
    assert_eq!(
        normalize_terminal_command("$ echo one\n>>> echo two\n> echo three"),
        "echo one\necho two\necho three"
    );
}

#[test]
fn terminal_runnable_language_identifies_shell_flavors() {
    assert!(is_terminal_runnable_language("bash"));
    assert!(is_terminal_runnable_language("sh"));
    assert!(is_terminal_runnable_language("zsh"));
    assert!(is_terminal_runnable_language("shell"));
    assert!(!is_terminal_runnable_language("rust"));
    assert!(!is_terminal_runnable_language("python"));
    assert!(!is_terminal_runnable_language("json"));
}

#[test]
fn active_slash_command_query_identifies_autocomplete_prefixes() {
    assert_eq!(active_slash_command_query("/"), Some(""));
    assert_eq!(active_slash_command_query("/com"), Some("com"));
    assert_eq!(active_slash_command_query("  /help"), Some("help"));
    assert_eq!(active_slash_command_query("/commit message"), None);
    assert_eq!(active_slash_command_query("/commit "), None);
    assert_eq!(active_slash_command_query("hello /help"), None);
    assert_eq!(active_slash_command_query("plain text"), None);
}

#[test]
fn slash_command_binding_context_matches_nested_input() {
    use gpui::{KeyBindingContextPredicate, KeyContext};

    let predicate = KeyBindingContextPredicate::parse(SLASH_COMMAND_BINDING_CONTEXT).unwrap();
    let menu_ctx = KeyContext::try_from(SLASH_COMMAND_KEY_CONTEXT).unwrap();
    let input_ctx = KeyContext::try_from(INPUT_KEY_CONTEXT).unwrap();

    let active_contexts = vec![menu_ctx, input_ctx.clone()];
    assert_eq!(predicate.depth_of(&active_contexts), Some(2));
    assert!(predicate.eval(&active_contexts));

    let normal_contexts = vec![input_ctx];
    assert_eq!(predicate.depth_of(&normal_contexts), None);
    assert!(!predicate.eval(&normal_contexts));
}

#[gpui::test]
fn reading_history_survives_new_activity_and_jump_resumes_following(cx: &mut gpui::TestAppContext) {
    use gpui::AppContext as _;
    cx.update(gpui_component::init);
    let model = cx.new(|_| {
        let mut state = threadlane_ui_state::AppState::default();
        state.is_new_task = false;
        state.messages = (0..80)
            .map(|index| ChatMessageInfo {
                id: format!("message-{index}"),
                role: threadlane_ui_state::MessageRole::User,
                content: format!(
                    "Message {index}: {}",
                    "Long conversation content. ".repeat(10)
                ),
                tool_activities: Vec::new(),
                streaming: false,
                reasoning_content: None,
                reasoning_expanded: false,
            })
            .collect::<Vec<_>>()
            .into();
        state
    });
    let retained_model = model.clone();
    let holder: std::rc::Rc<
        std::cell::RefCell<Option<gpui::Entity<super::ChatListView>>>,
    > = Default::default();
    let holder_clone = holder.clone();
    let (_, cx) = cx.add_window_view(move |window, cx| {
        let chat = cx.new(|cx| super::ChatListView::new(model, window, cx));
        holder_clone.borrow_mut().replace(chat.clone());
        gpui_component::Root::new(chat, window, cx)
    });
    let chat = holder
        .borrow()
        .as_ref()
        .expect("chat view mounted")
        .clone();
    cx.run_until_parked();
    chat.update(cx, |chat, cx| {
        chat.initial_scroll_frames = 0;
        chat.transcript_list_state.scroll_to(gpui::ListOffset {
            item_ix: 10,
            offset_in_item: gpui::px(0.),
        });
        cx.notify();
    });
    cx.run_until_parked();
    let before = chat.read_with(cx, |chat, _| {
        chat.transcript_list_state.logical_scroll_top()
    });
    retained_model.update(cx, |state, cx| {
        let mut next = state.messages.last().unwrap().clone();
        next.id = "new-activity".into();
        std::sync::Arc::make_mut(&mut state.messages).push(next);
        cx.notify();
    });
    cx.run_until_parked();
    chat.read_with(cx, |chat, _| {
        assert!(!chat.transcript_list_state.is_following_tail());
        let after = chat.transcript_list_state.logical_scroll_top();
        assert_eq!(after.item_ix, before.item_ix);
        assert_eq!(after.offset_in_item, before.offset_in_item);
    });
    cx.update(|window, cx| window.draw(cx).clear(cx));
    let jump = cx
        .debug_bounds("jump-to-latest")
        .expect("reader can return to latest");
    cx.simulate_click(jump.center(), gpui::Modifiers::default());
    cx.run_until_parked();
    chat.read_with(cx, |chat, _| {
        assert!(chat.transcript_list_state.is_following_tail())
    });
}

#[gpui::test]
fn plan_disclosure_opens_on_click_and_does_not_leak_to_another_task(cx: &mut gpui::TestAppContext) {
    use gpui::AppContext as _;
    use threadlane_protocol::messages::{PlanItem, PlanItemStatus, SessionPlan};
    struct PlanColumn(gpui::Entity<super::ChatListView>);
    impl gpui::Render for PlanColumn {
        fn render(&mut self, _: &mut gpui::Window, _: &mut gpui::Context<Self>) -> impl gpui::IntoElement {
            use gpui::{ParentElement as _, Styled as _};
            gpui::div().w(gpui::px(448.)).h_full().child(self.0.clone())
        }
    }
    cx.update(gpui_component::init);
    let model = cx.new(|_| {
        let mut state = threadlane_ui_state::AppState::default();
        state.active_session_id = Some("planned-task".into());
        state.active_plan = SessionPlan {
            explanation: None,
            items: vec![PlanItem {
                step: "Verify the layout with a long task description. ".repeat(30),
                status: PlanItemStatus::InProgress,
            }],
        };
        state
    });
    let retained_model = model.clone();
    let (_, cx) = cx.add_window_view(move |window, cx| {
        let chat = cx.new(|cx| super::ChatListView::new(model, window, cx));
        gpui_component::Root::new(cx.new(|_| PlanColumn(chat)), window, cx)
    });
    cx.update(|window, cx| window.draw(cx).clear(cx));
    let tracker = cx.debug_bounds("session-plan-tracker").unwrap();
    assert!(tracker.right() <= gpui::px(448.), "long plan stays inside the chat pane: {tracker:?}");
    cx.simulate_click(tracker.center(), gpui::Modifiers::default());
    cx.run_until_parked();
    cx.update(|window, cx| window.draw(cx).clear(cx));
    assert!(cx.debug_bounds("session-plan-details").is_some());
    retained_model.update(cx, |state, cx| {
        state.active_session_id = Some("other-task".into());
        state.active_plan = SessionPlan::default();
        cx.notify();
    });
    cx.run_until_parked();
    cx.update(|window, cx| window.draw(cx).clear(cx));
    assert!(cx.debug_bounds("session-plan-details").is_none());
}

#[gpui::test]
fn permission_details_are_bound_to_the_request_that_opened_them(cx: &mut gpui::TestAppContext) {
    use gpui::AppContext as _;
    use gpui_component::WindowExt as _;
    struct DialogHost(gpui::Entity<super::ChatListView>);
    impl gpui::Render for DialogHost {
        fn render(&mut self, _window: &mut gpui::Window, _cx: &mut gpui::Context<Self>) -> impl gpui::IntoElement {
            use gpui::{ParentElement as _, Styled as _};
            gpui::div().size_full().child(self.0.clone())
        }
    }
    cx.update(gpui_component::init);
    let request = |id: &str| threadlane_protocol::PermissionRequest {
        id: id.into(), capability: "network".into(), title: "Connect to test host".into(),
        detail: "https://example.test".into(), scopes: vec![threadlane_protocol::PermissionScope::Once],
    };
    let model = cx.new(|_| {
        let mut state = threadlane_ui_state::AppState::default();
        state.active_session_id = Some("permission-task".into());
        state.pending_permissions.insert("permission-task".into(), request("first"));
        state
    });
    let retained_model = model.clone();
    let captured = std::rc::Rc::new(std::cell::RefCell::new(None));
    let capture = captured.clone();
    let (_, cx) = cx.add_window_view(move |window, cx| {
        let chat = cx.new(|cx| super::ChatListView::new(model, window, cx));
        *capture.borrow_mut() = Some(chat.clone());
        gpui_component::Root::new(cx.new(|_| DialogHost(chat)), window, cx)
    });
    let chat = captured.borrow_mut().take().unwrap();
    cx.run_until_parked();
    cx.update(|window, cx| chat.update(cx, |chat, cx| chat.open_permission_details("first", window, cx)));
    cx.run_until_parked();
    cx.update(|window, cx| { window.draw(cx).clear(cx); assert!(window.has_active_dialog(cx)); });
    assert!(cx.debug_bounds("permission-details-always").is_none());
    assert!(cx.debug_bounds("permission-inline-always").is_none());
    cx.simulate_keystrokes("escape");
    cx.run_until_parked();
    cx.update(|window, cx| assert!(!window.has_active_dialog(cx), "Escape closes details"));
    retained_model.read_with(cx, |state, _| assert_eq!(state.pending_permissions["permission-task"].id, "first"));
    cx.update(|window, cx| chat.update(cx, |chat, cx| chat.open_permission_details("first", window, cx)));
    cx.run_until_parked();
    retained_model.update(cx, |state, _| {
        state.pending_permissions.insert("permission-task".into(), request("replacement"));
    });
    // Even before a render synchronizes the modal, stale details cannot expose
    // the replacement request's actions.
    chat.update(cx, |chat, cx| assert!(chat.render_permission_details_dialog(cx).is_none()));
    retained_model.update(cx, |_, cx| cx.notify());
    cx.run_until_parked();
    chat.read_with(cx, |chat, _| assert!(chat.permission_details_request.is_none()));
    cx.update(|window, cx| assert!(!window.has_active_dialog(cx)));
    retained_model.read_with(cx, |state, _| {
        assert_eq!(state.pending_permissions["permission-task"].id, "replacement");
    });
}

#[gpui::test]
fn completed_activity_disclosure_renders_interactive_tool_rows(cx: &mut gpui::TestAppContext) {
    use gpui::AppContext as _;
    cx.update(gpui_component::init);
    let model = cx.new(|_| {
        let mut state = threadlane_ui_state::AppState::default();
        state.is_new_task = false;
        let tool_message = ChatMessageInfo {
            id: "tool-result".into(),
            role: MessageRole::Assistant,
            content: String::new(),
            tool_activities: vec![ToolActivityInfo {
                id: "read-file".into(),
                category: "Completed".into(),
                title: "read_file".into(),
                display_summary: "Read source file".into(),
                detail: "Large source file line\n".repeat(250),
                is_expanded: false,
            }],
            streaming: false,
            reasoning_content: None,
            reasoning_expanded: false,
        };
        let mut messages = (0..60).map(|index| ChatMessageInfo {
            id: format!("history-{index}"), role: MessageRole::User,
            content: format!("Historical message {index}. {}", "Keep this reading position. ".repeat(5)),
            tool_activities: Vec::new(), streaming: false, reasoning_content: None, reasoning_expanded: false,
        }).collect::<Vec<_>>();
        messages.insert(20, tool_message);
        state.messages = messages.into();
        state
    });
    let holder: std::rc::Rc<
        std::cell::RefCell<Option<gpui::Entity<super::ChatListView>>>,
    > = Default::default();
    let holder_clone = holder.clone();
    let (_, cx) = cx.add_window_view(move |window, cx| {
        let chat = cx.new(|cx| super::ChatListView::new(model, window, cx));
        holder_clone.borrow_mut().replace(chat.clone());
        gpui_component::Root::new(chat, window, cx)
    });
    let chat = holder
        .borrow()
        .as_ref()
        .expect("chat view mounted")
        .clone();
    cx.run_until_parked();
    chat.update(cx, |chat, cx| {
        chat.initial_scroll_frames = 0;
        chat.transcript_list_state.scroll_to(gpui::ListOffset { item_ix: 20, offset_in_item: gpui::px(0.) });
        cx.notify();
    });
    cx.run_until_parked();
    let before = chat.read_with(cx, |chat, _| chat.transcript_list_state.logical_scroll_top());
    cx.update(|window, cx| window.draw(cx).clear(cx));
    let disclosure = cx.debug_bounds("activity-group-disclosure").expect("completed group has a disclosure");
    cx.simulate_click(disclosure.center(), gpui::Modifiers::default());
    cx.run_until_parked();
    cx.update(|window, cx| window.draw(cx).clear(cx));
    chat.read_with(cx, |chat, _| {
        assert_eq!(chat.expanded_activity_groups.len(), 1);
        let after = chat.transcript_list_state.logical_scroll_top();
        assert_eq!((after.item_ix, after.offset_in_item), (before.item_ix, before.offset_in_item));
    });
    let tool = cx.debug_bounds("tool-activity-disclosure").expect("expanded group exposes tool");
    cx.simulate_click(tool.center(), gpui::Modifiers::default());
    cx.run_until_parked();
    cx.update(|window, cx| window.draw(cx).clear(cx));
    chat.read_with(cx, |chat, cx| {
        assert!(chat.model.read(cx).messages.iter().flat_map(|message| &message.tool_activities).any(|activity| activity.id == "read-file" && activity.is_expanded));
        assert!(!chat.transcript_list_state.is_following_tail());
        let after = chat.transcript_list_state.logical_scroll_top();
        assert_eq!((after.item_ix, after.offset_in_item), (before.item_ix, before.offset_in_item));
    });

}

// Check the painted content masks, not only the outer button bounds: Button's
// inner label clips children even when the disclosure itself has enough height.
#[gpui::test]
fn transcript_disclosure_badges_are_not_vertically_clipped(cx: &mut gpui::TestAppContext) {
    use gpui::{AppContext as _, ParentElement as _, Styled as _};
    use gpui_component::ActiveTheme as _;

    struct Harness {
        chat: gpui::Entity<super::ChatListView>,
        message: ChatMessageInfo,
    }
    impl gpui::Render for Harness {
        fn render(
            &mut self,
            _: &mut gpui::Window,
            cx: &mut gpui::Context<Self>,
        ) -> impl gpui::IntoElement {
            self.chat.update(cx, |chat, cx| {
                gpui::div()
                    .size_full()
                    .p_4()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .children(chat.render_reasoning_block(&self.message, cx))
                    .children(chat.render_tool_activities_block(
                        &self.message.id,
                        &self.message.tool_activities,
                        cx,
                    ))
            })
        }
    }

    cx.update(gpui_component::init);
    let model = cx.new(|_| threadlane_ui_state::AppState::default());
    let holder = std::rc::Rc::new(std::cell::RefCell::new(None));
    let capture = holder.clone();
    let (_, cx) = cx.add_window_view(move |window, cx| {
        let chat = cx.new(|cx| super::ChatListView::new(model, window, cx));
        let host = cx.new(|_| Harness {
            chat,
            message: ChatMessageInfo {
                id: "disclosure-layout".into(),
                role: MessageRole::Assistant,
                content: String::new(),
                tool_activities: (0..2)
                    .map(|index| ToolActivityInfo {
                        id: format!("tool-{index}"),
                        category: "Completed".into(),
                        title: "read_file".into(),
                        display_summary: "Read source file".into(),
                        detail: "Source content".into(),
                        is_expanded: false,
                    })
                    .collect(),
                streaming: false,
                reasoning_content: Some("Reasoning detail.".into()),
                reasoning_expanded: false,
            },
        });
        *capture.borrow_mut() = Some(host.clone());
        gpui_component::Root::new(host, window, cx)
    });
    let host = holder.borrow_mut().take().unwrap();

    for font_size in [14.0, 16.0, 20.0] {
        for width in [320.0, 800.0] {
            for expanded in [false, true] {
                cx.update(|_, cx| {
                    gpui_component::Theme::global_mut(cx).font_size = gpui::px(font_size);
                });
                host.update(cx, |host, cx| {
                    host.message.reasoning_expanded = expanded;
                    host.message.streaming = expanded;
                    cx.notify();
                });
                cx.simulate_resize(gpui::size(gpui::px(width), gpui::px(800.0)));
                cx.run_until_parked();
                cx.update(|window, cx| {
                    window.draw(cx).clear(cx);
                    let badges = window.painted_quads().into_iter()
                        .filter(|quad| quad.background == gpui::Background::from(cx.theme().secondary))
                        .collect::<Vec<_>>();
                    assert_eq!(badges.len(), 2, "both disclosure badges must be painted");
                    for badge in badges {
                        let visible = badge.bounds.intersect(&badge.content_mask.bounds);
                        assert_eq!(visible.size.height, badge.bounds.size.height,
                            "badge clipped at font={font_size}, width={width}, expanded={expanded}: {badge:?}");
                    }
                });
            }
        }
    }
}

#[gpui::test]
fn reasoning_disclosure_supports_keyboard_and_pauses_following(cx: &mut gpui::TestAppContext) {
    use gpui::AppContext as _;
    cx.update(gpui_component::init);
    let model = cx.new(|_| {
        let mut state = threadlane_ui_state::AppState::default();
        state.is_new_task = false;
        state.messages = vec![ChatMessageInfo {
            id: "reasoning-test".into(), role: MessageRole::Assistant,
            content: "Long response paragraph.\n\n".repeat(100), tool_activities: Vec::new(), streaming: false,
            reasoning_content: Some("Reasoning detail.\n".repeat(100)), reasoning_expanded: false,
        }].into();
        state
    });
    struct Harness(gpui::Entity<super::ChatListView>);
    impl gpui::Render for Harness {
        fn render(&mut self, _: &mut gpui::Window, _: &mut gpui::Context<Self>) -> impl gpui::IntoElement {
            use gpui::{InteractiveElement as _, ParentElement as _, Styled as _};
            gpui::div().id("workspace").tab_group().size_full().child(self.0.clone())
        }
    }
    let holder: std::rc::Rc<
        std::cell::RefCell<Option<gpui::Entity<super::ChatListView>>>,
    > = Default::default();
    let holder_clone = holder.clone();
    let (_, cx) = cx.add_window_view(move |window, cx| {
        let chat = cx.new(|cx| super::ChatListView::new(model, window, cx));
        holder_clone.borrow_mut().replace(chat.clone());
        let host = cx.new(|_| Harness(chat));
        gpui_component::Root::new(host, window, cx)
    });
    let chat = holder
        .borrow()
        .as_ref()
        .expect("chat view mounted")
        .clone();
    cx.run_until_parked();
    chat.update(cx, |chat, cx| {
        chat.initial_scroll_frames = 0;
        chat.transcript_list_state.scroll_to(gpui::ListOffset { item_ix: 0, offset_in_item: gpui::px(0.) });
        cx.notify();
    });
    cx.run_until_parked();
    cx.update(|window, cx| window.draw(cx).clear(cx));
    let reasoning = cx.debug_bounds("reasoning-disclosure").expect("reasoning has a native disclosure");
    cx.simulate_click(reasoning.center(), gpui::Modifiers::default());
    cx.run_until_parked();
    chat.read_with(cx, |chat, cx| {
        assert!(chat.model.read(cx).messages[0].reasoning_expanded);
        assert!(!chat.transcript_list_state.is_following_tail());
    });
    cx.update(|window, cx| {
        window.blur(cx);
        window.focus_next(cx); // Conversation outline
        window.focus_next(cx); // Find in conversation
        window.focus_next(cx); // Chat
        window.focus_next(cx); // Editor
        window.focus_next(cx); // Reasoning
        window.draw(cx).clear(cx);
    });
    let keystroke = gpui::Keystroke::parse("space").unwrap();
    cx.simulate_event(gpui::KeyDownEvent { keystroke: keystroke.clone(), is_held: false, prefer_character_input: false });
    cx.simulate_event(gpui::KeyUpEvent { keystroke });
    cx.run_until_parked();
    chat.read_with(cx, |chat, cx| {
        assert!(!chat.model.read(cx).messages[0].reasoning_expanded, "focused disclosure supports Space");
    });
}

#[gpui::test]
fn environment_tracks_checkout_and_yields_space_to_chat(cx: &mut gpui::TestAppContext) {
    use gpui::AppContext as _;
    cx.update(gpui_component::init);
    let model = cx.new(|_| {
        let mut state = threadlane_ui_state::AppState::default();
        state.projects.clear();
        threadlane_ui_state::activate_test_session(&mut state, "first", std::path::Path::new("/projects/one/first.jsonl"));
        state.is_new_task = false;
        state
    });
    let retained_model = model.clone();
    let (chat, cx) = cx.add_window_view(move |window, cx| super::ChatListView::new(model, window, cx));
    chat.update(cx, |chat, cx| chat.set_environment_width(gpui::px(1200.), gpui::px(16.), cx));
    cx.update(|window, cx| window.draw(cx).clear(cx));
    assert!(cx.debug_bounds("chat-environment").is_some());
    let terminal = cx.debug_bounds("environment-terminal").unwrap();
    cx.simulate_click(terminal.center(), gpui::Modifiers::default());
    retained_model.read_with(cx, |state, _| {
        assert_eq!(state.requested_terminal_work_dir, state.active_git_work_dir());
        assert!(state.requested_terminal_work_dir.is_some());
    });
    retained_model.update(cx, |state, cx| {
        threadlane_ui_state::activate_test_session(state, "second", std::path::Path::new("/projects/two/second.jsonl"));
        state.requested_terminal_work_dir = None;
        cx.notify();
    });
    cx.run_until_parked();
    cx.update(|window, cx| window.draw(cx).clear(cx));
    let terminal = cx.debug_bounds("environment-terminal").unwrap();
    cx.simulate_click(terminal.center(), gpui::Modifiers::default());
    retained_model.read_with(cx, |state, _| {
        assert_eq!(state.requested_terminal_work_dir, Some("/projects/two".into()));
    });
    chat.update(cx, |chat, cx| chat.set_environment_width(gpui::px(900.), gpui::px(16.), cx));
    cx.update(|window, cx| window.draw(cx).clear(cx));
    assert!(cx.debug_bounds("chat-environment").is_none());
}

#[gpui::test]
fn user_message_edit_loads_composer_for_resend(cx: &mut gpui::TestAppContext) {
    use gpui::AppContext as _;
    use gpui::Focusable as _;
    cx.update(gpui_component::init);
    let model = cx.new(|_| {
        let mut state = threadlane_ui_state::AppState::default();
        state.is_new_task = false;
        state.messages = vec![ChatMessageInfo {
            id: "user-1".into(),
            role: MessageRole::User,
            content: "User request".into(),
            tool_activities: Vec::new(),
            streaming: false,
            reasoning_content: None,
            reasoning_expanded: false,
        }]
        .into();
        state
    });
    let holder: std::rc::Rc<
        std::cell::RefCell<Option<gpui::Entity<super::ChatListView>>>,
    > = Default::default();
    let holder_clone = holder.clone();
    let (_, cx) = cx.add_window_view(move |window, cx| {
        let chat = cx.new(|cx| super::ChatListView::new(model, window, cx));
        holder_clone.borrow_mut().replace(chat.clone());
        gpui_component::Root::new(chat, window, cx)
    });
    cx.run_until_parked();
    cx.update(|window, cx| window.draw(cx).clear(cx));
    let chat = holder
        .borrow()
        .as_ref()
        .expect("chat view mounted")
        .clone();
    let edit = cx
        .debug_bounds("message-edit")
        .expect("settled user message exposes an edit action");
    cx.simulate_click(edit.center(), gpui::Modifiers::default());
    cx.run_until_parked();
    let composer_focus = chat.read_with(cx, |chat, cx| {
        chat.input_state
            .read(cx)
            .focus_handle(cx)
    });
    cx.update(|window, cx| {
        assert_eq!(
            window.focused(cx),
            Some(composer_focus),
            "edit hands focus to the composer"
        );
    });
    chat.update(cx, |chat, cx| {
        assert_eq!(
            chat.input_state.read(cx).value().as_ref(),
            "User request",
            "edit loads the message text into the composer"
        );
        assert_eq!(chat.current_tab, super::CentralTab::Chat);
    });

    // Editing history must never silently replace an unsent draft, including
    // image-only drafts whose text field is empty.
    for (draft, with_image) in [("Unsent follow-up", false), ("", true)] {
        cx.update(|window, cx| {
            chat.update(cx, |chat, cx| {
                chat.input_state.update(cx, |input, cx| {
                    input.set_value(draft, window, cx);
                });
                chat.pasted_images = vec![super::ImageAttachment {
                    display_name: "draft.png".into(),
                    data_url: "data:image/png;base64,test".into(),
                }];
                if !with_image {
                    chat.pasted_images.clear();
                }
                cx.notify();
            });
        });
        cx.run_until_parked();
        cx.update(|window, cx| window.draw(cx).clear(cx));
        let edit = cx
            .debug_bounds("message-edit")
            .expect("edit action remains available");
        cx.simulate_click(edit.center(), gpui::Modifiers::default());
        cx.run_until_parked();
        chat.read_with(cx, |chat, cx| {
            assert_eq!(chat.input_state.read(cx).value().as_ref(), draft);
            assert_eq!(chat.pasted_images.len(), usize::from(with_image));
            if with_image {
                assert_eq!(chat.pasted_images[0].display_name, "draft.png");
            }
        });
    }
}

#[gpui::test]
fn user_message_edit_hidden_while_generating(cx: &mut gpui::TestAppContext) {
    use gpui::AppContext as _;
    cx.update(gpui_component::init);
    let model = cx.new(|_| {
        let mut state = threadlane_ui_state::AppState::default();
        state.is_new_task = false;
        state.messages = vec![ChatMessageInfo {
            id: "user-1".into(),
            role: MessageRole::User,
            content: "User request".into(),
            tool_activities: Vec::new(),
            streaming: false,
            reasoning_content: None,
            reasoning_expanded: false,
        }]
        .into();
        state.is_generating = true;
        state
    });
    let (_, cx) = cx.add_window_view(move |window, cx| {
        let chat = cx.new(|cx| super::ChatListView::new(model, window, cx));
        gpui_component::Root::new(chat, window, cx)
    });
    cx.run_until_parked();
    cx.update(|window, cx| window.draw(cx).clear(cx));
    assert!(
        cx.debug_bounds("message-edit").is_none(),
        "edit stays hidden while a generation is running"
    );
    assert!(
        cx.debug_bounds("message-copy").is_some(),
        "copy remains available while generating"
    );
}

#[gpui::test]
fn user_message_exposes_copy_action(cx: &mut gpui::TestAppContext) {
    use gpui::AppContext as _;
    cx.update(gpui_component::init);
    let model = cx.new(|_| {
        let mut state = threadlane_ui_state::AppState::default();
        state.is_new_task = false;
        state.messages = vec![ChatMessageInfo {
            id: "user-1".into(),
            role: MessageRole::User,
            content: "User request".into(),
            tool_activities: Vec::new(),
            streaming: false,
            reasoning_content: None,
            reasoning_expanded: false,
        }]
        .into();
        state
    });
    let (_, cx) = cx.add_window_view(move |window, cx| {
        let chat = cx.new(|cx| super::ChatListView::new(model, window, cx));
        gpui_component::Root::new(chat, window, cx)
    });
    cx.run_until_parked();
    cx.update(|window, cx| window.draw(cx).clear(cx));
    let copy = cx
        .debug_bounds("message-copy")
        .expect("user message exposes a copy action");
    cx.simulate_click(copy.center(), gpui::Modifiers::default());
    assert_eq!(
        cx.read_from_clipboard().and_then(|item| item.text()),
        Some("User request".to_string())
    );
}

#[gpui::test]
fn completed_assistant_message_exposes_copy_action(cx: &mut gpui::TestAppContext) {
    use gpui::AppContext as _;
    cx.update(gpui_component::init);
    let model = cx.new(|_| {
        let mut state = threadlane_ui_state::AppState::default();
        state.is_new_task = false;
        state.messages = vec![ChatMessageInfo {
            id: "assistant-1".into(),
            role: MessageRole::Assistant,
            content: "Completed response".into(),
            tool_activities: Vec::new(),
            streaming: false,
            reasoning_content: None,
            reasoning_expanded: false,
        }]
        .into();
        state
    });
    let (_, cx) = cx.add_window_view(move |window, cx| {
        let chat = cx.new(|cx| super::ChatListView::new(model, window, cx));
        gpui_component::Root::new(chat, window, cx)
    });
    cx.run_until_parked();
    cx.update(|window, cx| window.draw(cx).clear(cx));
    let copy = cx
        .debug_bounds("message-copy")
        .expect("completed response exposes a copy action");
    cx.simulate_click(copy.center(), gpui::Modifiers::default());
    assert_eq!(
        cx.read_from_clipboard().and_then(|item| item.text()),
        Some("Completed response".to_string())
    );
}

#[gpui::test]
fn message_copy_shows_copied_feedback(cx: &mut gpui::TestAppContext) {
    use gpui::AppContext as _;
    cx.update(gpui_component::init);
    let model = cx.new(|_| {
        let mut state = threadlane_ui_state::AppState::default();
        state.is_new_task = false;
        state.messages = vec![ChatMessageInfo {
            id: "assistant-1".into(),
            role: MessageRole::Assistant,
            content: "Completed response".into(),
            tool_activities: Vec::new(),
            streaming: false,
            reasoning_content: None,
            reasoning_expanded: false,
        }]
        .into();
        state
    });
    let holder: std::rc::Rc<
        std::cell::RefCell<Option<gpui::Entity<super::ChatListView>>>,
    > = Default::default();
    let holder_clone = holder.clone();
    let (_, cx) = cx.add_window_view(move |window, cx| {
        let chat = cx.new(|cx| super::ChatListView::new(model, window, cx));
        holder_clone.borrow_mut().replace(chat.clone());
        gpui_component::Root::new(chat, window, cx)
    });
    cx.run_until_parked();
    cx.update(|window, cx| window.draw(cx).clear(cx));
    let chat = holder
        .borrow()
        .as_ref()
        .expect("chat view mounted")
        .clone();
    chat.read_with(cx, |chat, _| {
        assert!(chat.copied_message.is_none());
    });
    let copy = cx
        .debug_bounds("message-copy")
        .expect("copy action present");
    cx.simulate_click(copy.center(), gpui::Modifiers::default());
    cx.run_until_parked();
    chat.read_with(cx, |chat, _| {
        let (key, _) = chat
            .copied_message
            .as_ref()
            .expect("copy records per-message feedback");
        assert_eq!(key, "message-copy-assistant-1");
    });
    cx.update(|window, cx| window.draw(cx).clear(cx));
    assert!(cx.debug_bounds("message-copy").is_some());
}

#[gpui::test]
fn streaming_assistant_message_hides_copy_action(cx: &mut gpui::TestAppContext) {
    use gpui::AppContext as _;
    cx.update(gpui_component::init);
    let model = cx.new(|_| {
        let mut state = threadlane_ui_state::AppState::default();
        state.is_new_task = false;
        state.messages = vec![ChatMessageInfo {
            id: "assistant-streaming".into(),
            role: MessageRole::Assistant,
            content: "Partial response".into(),
            tool_activities: Vec::new(),
            streaming: true,
            reasoning_content: None,
            reasoning_expanded: false,
        }]
        .into();
        state
    });
    let (_, cx) = cx.add_window_view(move |window, cx| {
        let chat = cx.new(|cx| super::ChatListView::new(model, window, cx));
        gpui_component::Root::new(chat, window, cx)
    });
    cx.run_until_parked();
    cx.update(|window, cx| window.draw(cx).clear(cx));
    assert!(
        cx.debug_bounds("message-copy").is_none(),
        "streaming response defers its actions until generation completes"
    );
}

#[test]
fn run_elapsed_formats_seconds_minutes_and_hours() {
    assert_eq!(super::format_run_elapsed(7), "7s");
    assert_eq!(super::format_run_elapsed(59), "59s");
    assert_eq!(super::format_run_elapsed(65), "1m 05s");
    assert_eq!(super::format_run_elapsed(600), "10m 00s");
    assert_eq!(super::format_run_elapsed(3725), "1h 02m");
}

#[test]
fn sendable_prompt_accepts_text_or_images() {
    assert!(!super::has_sendable_prompt("", 0));
    assert!(!super::has_sendable_prompt("   ", 0));
    assert!(super::has_sendable_prompt("hello", 0));
    assert!(super::has_sendable_prompt("", 1));
    assert!(super::has_sendable_prompt("   ", 2));
}

#[test]
fn plan_step_truncates_long_labels_with_ellipsis() {
    assert_eq!(super::truncate_plan_step("Short step", 42), "Short step");
    assert_eq!(
        super::truncate_plan_step(&"x".repeat(42), 42),
        "x".repeat(42)
    );
    let long = "Implement the multi-turn durable queue handoff with tests";
    let truncated = super::truncate_plan_step(long, 20);
    assert!(truncated.ends_with('…'));
    assert!(truncated.chars().count() <= 20);
    assert!(long.starts_with(truncated.trim_end_matches('…')));
}

#[test]
fn preview_truncation_matches_stash_banner_bounds() {
    let short = "Fix the typo";
    assert_eq!(super::truncate_preview_text(short, 60), short);
    assert_eq!(
        super::truncate_preview_text(&"y".repeat(60), 60),
        "y".repeat(60)
    );
    let long = "z".repeat(61);
    let truncated = super::truncate_preview_text(&long, 60);
    assert!(truncated.ends_with('…'));
    assert_eq!(truncated.chars().count(), 60);
    assert_eq!(super::truncate_preview_text("  padded  ", 60), "padded");
}

#[test]
fn completed_activity_labels_use_singular_for_one() {
    assert_eq!(super::completed_activities_text(1), "1 completed activity");
    assert_eq!(super::completed_activities_text(2), "2 completed activities");
    assert_eq!(
        super::expand_activities_a11y(1),
        "Expand 1 completed tool activity"
    );
    assert_eq!(
        super::expand_activities_a11y(3),
        "Expand 3 completed tool activities"
    );
}

#[test]
fn plan_tracker_labels_avoid_repeating_plan_when_complete() {
    let (display, a11y, tooltip) = super::plan_tracker_texts(1, 3, Some("Run tests"));
    assert_eq!(display, "Run tests");
    assert_eq!(a11y, "Task plan, 1 of 3 complete, current step: Run tests");
    assert_eq!(tooltip, "Show task plan · Run tests");
    let (display, a11y, tooltip) = super::plan_tracker_texts(3, 3, None);
    assert_eq!(display, "Complete");
    assert_eq!(a11y, "Task plan, 3 of 3 complete");
    assert_eq!(tooltip, "Show task plan · all steps complete");
}

#[test]
fn reasoning_token_badge_uses_singular_for_one_token() {
    assert_eq!(super::reasoning_token_badge(true, 100), "thinking…");
    assert_eq!(super::reasoning_token_badge(false, 1), "~1 token");
    assert_eq!(super::reasoning_token_badge(false, 0), "~0 tokens");
    assert_eq!(super::reasoning_token_badge(false, 100), "~25 tokens");
}

#[test]
fn tool_activity_glyph_marks_unknown_categories_neutral() {
    assert_eq!(super::tool_activity_glyph("Error"), "!");
    assert_eq!(super::tool_activity_glyph("Working"), "◌");
    assert_eq!(super::tool_activity_glyph("Thinking"), "◌");
    assert_eq!(super::tool_activity_glyph("Completed"), "✓");
    assert_eq!(super::tool_activity_glyph("Edited"), "✓");
    assert_eq!(super::tool_activity_glyph("tool"), "•");
    assert_eq!(super::tool_activity_glyph(""), "•");
}

#[test]
fn progress_header_prefix_names_errors_explicitly() {
    assert_eq!(super::progress_header_prefix(false), "Latest activity:");
    assert_eq!(super::progress_header_prefix(true), "Needs attention:");
}

#[test]
fn skills_chip_label_uses_singular_for_one_skill() {
    assert_eq!(super::skills_chip_label(0), "Skills");
    assert_eq!(super::skills_chip_label(1), "1 Skill");
    assert_eq!(super::skills_chip_label(4), "4 Skills");
}

#[test]
fn stats_nouns_use_singular_for_one() {
    assert_eq!(crate::trajectory_view::plural_noun(0, "turn", "turns"), "turns");
    assert_eq!(crate::trajectory_view::plural_noun(1, "turn", "turns"), "turn");
    assert_eq!(crate::trajectory_view::plural_noun(2, "tool call", "tool calls"), "tool calls");
    assert_eq!(crate::trajectory_view::plural_noun(1, "anomaly", "anomalies"), "anomaly");
}

#[test]
fn project_menus_mark_the_current_project() {
    use std::path::{Path, PathBuf};

    let active = PathBuf::from("/work/mypi");
    assert!(super::is_current_project(Some(&active), Path::new("/work/mypi")));
    assert!(!super::is_current_project(
        Some(&active),
        Path::new("/work/other")
    ));
    assert!(!super::is_current_project(None, Path::new("/work/mypi")));
}

#[test]
fn header_title_drops_duplicated_issue_prefix() {
    use super::ChatListView;
    assert_eq!(
        ChatListView::header_title_without_issue_prefix("#295 Preview staged images", Some(295)),
        "Preview staged images"
    );
    assert_eq!(
        ChatListView::header_title_without_issue_prefix("#295 · Preview", Some(295)),
        "Preview"
    );
    assert_eq!(
        ChatListView::header_title_without_issue_prefix("#295", Some(295)),
        "#295",
        "keep the title if stripping would leave it empty"
    );
    assert_eq!(
        ChatListView::header_title_without_issue_prefix("#42 unrelated", Some(295)),
        "#42 unrelated"
    );
    assert_eq!(
        ChatListView::header_title_without_issue_prefix("Plain title", Some(295)),
        "Plain title"
    );
    assert_eq!(
        ChatListView::header_title_without_issue_prefix("#295 Preview", None),
        "#295 Preview"
    );
}

#[gpui::test]
fn new_task_hero_offers_project_picker(cx: &mut gpui::TestAppContext) {
    use gpui::AppContext as _;

    cx.update(gpui_component::init);
    let model = cx.new(|_| {
        let mut state = threadlane_ui_state::AppState::default();
        state.is_new_task = true;
        state
    });
    let (_, cx) = cx.add_window_view(move |window, cx| {
        let chat = cx.new(|cx| super::ChatListView::new(model, window, cx));
        gpui_component::Root::new(chat, window, cx)
    });
    cx.run_until_parked();
    cx.update(|window, cx| window.draw(cx).clear(cx));
    assert!(
        cx.debug_bounds("new-task-project-picker").is_some(),
        "new-task hero names its project picker"
    );
}

#[gpui::test]
fn question_card_toggles_option_selection(cx: &mut gpui::TestAppContext) {
    use gpui::AppContext as _;

    cx.update(gpui_component::init);
    let session_file = std::path::Path::new("/test-project/.threadlane/sessions/session-1.jsonl");
    let model = cx.new(|_| {
        let mut state = threadlane_ui_state::AppState::default();
        state.is_new_task = false;
        threadlane_ui_state::activate_test_session(&mut state, "session-1", session_file);
        state.pending_questions.insert(
            "session-1".into(),
            threadlane_protocol::QuestionRequest {
                id: "q-req-1".into(),
                questions: vec![threadlane_protocol::QuestionItem {
                    id: "q1".into(),
                    header: "Scope".into(),
                    question: "Which scope should apply?".into(),
                    options: vec!["Yes".into(), "No".into()],
                    allow_custom: false,
                }],
            },
        );
        state
    });
    // No Root layer: option toggles only notify, and submit/dismiss need a
    // live session runtime to resolve, so the card's UI-owned toggle state is
    // what's pinned here.
    let (chat, cx) =
        cx.add_window_view(move |window, cx| super::ChatListView::new(model, window, cx));
    cx.run_until_parked();
    cx.update(|window, cx| window.draw(cx).clear(cx));
    for selector in [
        "question-card",
        "question-option-q1-0",
        "question-option-q1-1",
        "question-submit",
        "question-dismiss",
    ] {
        assert!(
            cx.debug_bounds(selector).is_some(),
            "{selector} is visible while a question is pending"
        );
    }
    let selection_key = "q-req-1\0q1".to_string();
    assert!(
        chat.read_with(cx, |chat, _| {
            chat.question_selections
                .get(&selection_key)
                .cloned()
                .unwrap_or_default()
        })
        .is_empty()
    );
    let option = cx.debug_bounds("question-option-q1-0").unwrap();
    cx.simulate_click(option.center(), gpui::Modifiers::default());
    cx.run_until_parked();
    assert_eq!(
        chat.read_with(cx, |chat, _| {
            chat.question_selections
                .get(&selection_key)
                .cloned()
                .unwrap_or_default()
        }),
        vec!["Yes".to_string()]
    );
    // Toggling again removes the answer.
    let option = cx.debug_bounds("question-option-q1-0").unwrap();
    cx.simulate_click(option.center(), gpui::Modifiers::default());
    cx.run_until_parked();
    assert!(
        chat.read_with(cx, |chat, _| {
            chat.question_selections
                .get(&selection_key)
                .cloned()
                .unwrap_or_default()
        })
        .is_empty()
    );
}

#[gpui::test]
fn pending_preview_offers_queue_steer_and_edit_while_generating(cx: &mut gpui::TestAppContext) {
    use gpui::AppContext as _;

    cx.update(gpui_component::init);
    let model = cx.new(|_| {
        let mut state = threadlane_ui_state::AppState::default();
        state.is_new_task = false;
        state.is_generating = true;
        threadlane_ui_state::activate_test_session(
            &mut state,
            "session-1",
            std::path::Path::new("/test-project/.threadlane/sessions/session-1.jsonl"),
        );
        threadlane_ui_state::controller::dispatch(
            &mut state,
            threadlane_ui_state::actions::AppAction::StageBusyMessage {
                text: "Follow up after this turn".into(),
                images: Vec::new(),
            },
        );
        state
    });
    let (_, cx) = cx.add_window_view(move |window, cx| {
        let chat = cx.new(|cx| super::ChatListView::new(model, window, cx));
        gpui_component::Root::new(chat, window, cx)
    });
    cx.run_until_parked();
    cx.update(|window, cx| window.draw(cx).clear(cx));
    for selector in [
        "pending-preview-row",
        "pending-queue",
        "pending-steer",
        "pending-dismiss",
    ] {
        assert!(
            cx.debug_bounds(selector).is_some(),
            "{selector} is visible while a message is staged for the next turn"
        );
    }
}

#[gpui::test]
fn workspace_changes_review_entry_tracks_uncommitted_files(cx: &mut gpui::TestAppContext) {
    use gpui::AppContext as _;

    cx.update(gpui_component::init);
    let session_file = std::path::Path::new("/test-project/.threadlane/sessions/session-1.jsonl");
    let work_dir = session_file.parent().unwrap().to_path_buf();
    let model = cx.new(|_| {
        let mut state = threadlane_ui_state::AppState::default();
        state.is_new_task = false;
        threadlane_ui_state::activate_test_session(&mut state, "session-1", session_file);
        state.git_statuses.insert(
            work_dir.clone(),
            threadlane_git::GitStatus {
                files: vec![
                    threadlane_git::GitFile::default(),
                    threadlane_git::GitFile::default(),
                ],
                ..Default::default()
            },
        );
        state
    });
    let retained_model = model.clone();
    let (_, cx) = cx.add_window_view(move |window, cx| {
        let chat = cx.new(|cx| super::ChatListView::new(model, window, cx));
        gpui_component::Root::new(chat, window, cx)
    });
    cx.run_until_parked();
    cx.update(|window, cx| window.draw(cx).clear(cx));
    assert!(
        cx.debug_bounds("workspace-changes-review").is_some(),
        "Review entry appears with uncommitted files"
    );
    retained_model.update(cx, |state, cx| {
        if let Some(status) = state.git_statuses.get_mut(&work_dir) {
            status.files.clear();
        }
        cx.notify();
    });
    cx.run_until_parked();
    cx.update(|window, cx| window.draw(cx).clear(cx));
    assert!(
        cx.debug_bounds("workspace-changes-review").is_none(),
        "Review entry hides with a clean tree"
    );
}

#[gpui::test]
fn provider_setup_banner_hides_when_models_exist(cx: &mut gpui::TestAppContext) {
    use gpui::AppContext as _;

    cx.update(gpui_component::init);
    let model = cx.new(|_| {
        let mut state = threadlane_ui_state::AppState::default();
        state.is_new_task = false;
        state
    });
    let (_, cx) = cx.add_window_view(move |window, cx| {
        let chat = cx.new(|cx| super::ChatListView::new(model, window, cx));
        gpui_component::Root::new(chat, window, cx)
    });
    cx.run_until_parked();
    cx.update(|window, cx| window.draw(cx).clear(cx));
    // The static registry seeds always provide models in-process, so the
    // setup banner must stay hidden; its visible state needs an empty
    // catalog, which has no test seam yet.
    assert!(
        cx.debug_bounds("provider-setup-banner").is_none(),
        "setup banner hides when models are available"
    );
    assert!(
        cx.debug_bounds("composer-model-picker").is_some(),
        "model picker stays visible instead"
    );
}

#[gpui::test]
fn trajectory_inspector_tabs_switch_content(cx: &mut gpui::TestAppContext) {
    use gpui::AppContext as _;
    use std::collections::BTreeMap;
    use std::sync::Arc;

    cx.update(gpui_component::init);
    let model = cx.new(|_| {
        let mut state = threadlane_ui_state::AppState::default();
        state.is_new_task = false;
        state
    });
    let (chat, cx) =
        cx.add_window_view(move |window, cx| crate::TrajectoryView::new(model, window, cx));
    chat.update(cx, |chat, cx| {
        chat.trajectory_cache = Some(TrajectoryRenderCache {
            key: TrajectoryCacheKey {
                revision: 0,
                epoch: 0,
                mode: TrajectoryMode::Execution,
                query: String::new(),
                category: None,
                lane: None,
            },
            all_entries: vec![trajectory_entry("Tool", Some(1), Some(1))],
            categories: Arc::new(vec!["Tool".to_string()]),
            lanes: Arc::new(Vec::new()),
            lane_latest: Arc::new(BTreeMap::new()),
            filtered_indices: vec![0],
            previews: vec!["Tool".into()],
            rows: vec![TrajectoryRow::Entry(0)],
            summary: TrajectorySummary::default(),
        });
        chat.selected_trajectory_index = Some(0);
        cx.notify();
    });
    cx.run_until_parked();
    cx.update(|window, cx| window.draw(cx).clear(cx));
    for selector in [
        "trajectory-inspector-Overview",
        "trajectory-inspector-Preview",
        "trajectory-inspector-Raw",
        "trajectory-inspector-Source",
    ] {
        assert!(
            cx.debug_bounds(selector).is_some(),
            "{selector} inspector tab is reachable"
        );
    }
    chat.read_with(cx, |chat, _| {
        assert_eq!(
            chat.trajectory_inspector_tab,
            TrajectoryInspectorTab::Overview
        );
    });
    let raw = cx.debug_bounds("trajectory-inspector-Raw").unwrap();
    cx.simulate_click(raw.center(), gpui::Modifiers::default());
    cx.run_until_parked();
    chat.read_with(cx, |chat, _| {
        assert_eq!(chat.trajectory_inspector_tab, TrajectoryInspectorTab::Raw);
    });
}

#[gpui::test]
fn environment_section_renders_without_git_data(cx: &mut gpui::TestAppContext) {
    use gpui::AppContext as _;

    cx.update(gpui_component::init);
    let model = cx.new(|_| {
        let mut state = threadlane_ui_state::AppState::default();
        state.is_new_task = false;
        state.active_work_dir = Some(std::path::PathBuf::from("/test-project"));
        state
    });
    let (chat, cx) =
        cx.add_window_view(move |window, cx| super::ChatListView::new(model, window, cx));
    chat.update(cx, |chat, cx| {
        chat.environment_available = true;
        cx.notify();
    });
    cx.run_until_parked();
    cx.update(|window, cx| window.draw(cx).clear(cx));
    for selector in [
        "chat-environment",
        "environment-changes",
        "environment-files",
        "environment-terminal",
        "environment-token-efficiency",
    ] {
        assert!(
            cx.debug_bounds(selector).is_some(),
            "{selector} renders with no session or git status"
        );
    }
}

#[gpui::test]
fn environment_token_efficiency_follows_durable_session_hydration(cx: &mut gpui::TestAppContext) {
    use gpui::AppContext as _;
    use threadlane_ui_state::activate_test_session;

    let root = tempfile::tempdir().unwrap();
    let file = root.path().join("session.jsonl");
    let usage = |lane: &str, seq: u64, input: u64| {
        serde_json::json!({"Usage": {"id": format!("usage-{seq}"), "seq": seq,
            "lane": lane, "timestamp": seq, "cause": "Provider",
            "usage": {"input_tokens": input, "output_tokens": 10,
                "cache_read_tokens": 20, "cache_write_tokens": 0, "total_tokens": input + 30}}})
    };
    std::fs::write(
        &file,
        format!("{}\n{}\n", usage("main", 1, 100), usage("child", 2, 40)),
    )
    .unwrap();
    let projection = threadlane_daemon::projection::compute_full_session_projection(&file).unwrap();
    assert_eq!(projection.token_efficiency.usage.processed_tokens(), 200);
    cx.update(gpui_component::init);
    let model = cx.new(|_| {
        let mut state = threadlane_ui_state::AppState::default();
        activate_test_session(&mut state, "session", &file);
        state.is_new_task = false;
        state.apply_session_hydration("session", &file, projection);
        state
    });
    let model_for_view = model.clone();
    let (_, cx) = cx.add_window_view(move |window, cx| {
        let chat = cx.new(|cx| {
            let mut chat = super::ChatListView::new(model_for_view, window, cx);
            chat.environment_available = true;
            chat
        });
        gpui_component::Root::new(chat, window, cx)
    });
    cx.run_until_parked();
    cx.update(|window, cx| window.draw(cx).clear(cx));
    assert!(cx.debug_bounds("environment-token-efficiency").is_some());
    assert!(cx.debug_bounds("efficiency-Processed tokens").is_some());
    assert!(cx.debug_bounds("efficiency-Child tokens").is_some());
    model.read_with(cx, |state, _| {
        let report = state.active_token_efficiency().unwrap();
        assert_eq!(report.usage.processed_tokens(), 200);
        assert_eq!(report.lanes["child"].usage.processed_tokens(), 70);
    });

    // A completed-run hydration refresh replaces the snapshot without a second accounting path.
    std::fs::write(&file, format!("{}\n", usage("main", 1, 250))).unwrap();
    let refreshed = threadlane_daemon::projection::compute_full_session_projection(&file).unwrap();
    model.update(cx, |state, cx| {
        state.apply_session_hydration("session", &file, refreshed);
        assert_eq!(
            state
                .active_token_efficiency()
                .unwrap()
                .usage
                .processed_tokens(),
            280
        );
        state.active_session_id = Some("other-session".into());
        assert!(
            state.active_token_efficiency().is_none(),
            "never show another session's totals"
        );
        cx.notify();
    });
    cx.run_until_parked();
    cx.update(|window, cx| window.draw(cx).clear(cx));
    assert!(cx.debug_bounds("environment-token-efficiency").is_some());
    assert!(cx.debug_bounds("efficiency-Processed tokens").is_none());
}

#[gpui::test]
fn environment_git_shortcuts_follow_checkout(cx: &mut gpui::TestAppContext) {
    use gpui::AppContext as _;
    cx.update(gpui_component::init);
    let model = cx.new(|_| {
        let mut state = threadlane_ui_state::AppState::default();
        state.is_new_task = false;
        state.active_work_dir = Some("/project".into());
        state.active_session_id = None;
        state.git_statuses.insert(
            "/project".into(),
            threadlane_git::GitStatus {
                branch: Some("feature".into()),
                remote: Some("git@github.com:owner/repo.git".into()),
                ahead: 2,
                behind: 1,
                pr: Some(threadlane_git::GitHubPrInfo {
                    number: 271,
                    url: "https://github.com/owner/repo/pull/271".into(),
                    title: "Environment".into(),
                    ..Default::default()
                }),
                ..Default::default()
            },
        );
        state
    });
    let retained_model = model.clone();
    let (chat, cx) =
        cx.add_window_view(move |window, cx| super::ChatListView::new(model, window, cx));
    chat.update(cx, |chat, cx| {
        chat.set_environment_width(gpui::px(1200.), gpui::px(16.), cx)
    });
    cx.update(|window, cx| window.draw(cx).clear(cx));
    for selector in [
        "environment-git-actions",
        "environment-repository",
        "environment-pr",
        "environment-sync",
    ] {
        assert!(cx.debug_bounds(selector).is_some(), "missing {selector}");
    }
    let pr = cx.debug_bounds("environment-pr").unwrap();
    cx.simulate_click(pr.center(), gpui::Modifiers::default());
    assert_eq!(
        cx.opened_url().as_deref(),
        Some("https://github.com/owner/repo/pull/271")
    );
    for key in ["enter", "space"] {
        cx.update(|window, cx| {
            cx.open_url("https://example.invalid/keyboard-marker");
            window.draw(cx).clear(cx);
        });
        let keystroke = gpui::Keystroke::parse(key).unwrap();
        cx.simulate_event(gpui::KeyDownEvent {
            keystroke: keystroke.clone(),
            is_held: false,
            prefer_character_input: false,
        });
        cx.simulate_event(gpui::KeyUpEvent { keystroke });
        assert_eq!(
            cx.opened_url().as_deref(),
            Some("https://github.com/owner/repo/pull/271"),
            "PR link activates with {key}"
        );
    }
    let repository = cx.debug_bounds("environment-repository").unwrap();
    cx.simulate_click(repository.center(), gpui::Modifiers::default());
    retained_model.read_with(cx, |state, _| {
        assert_eq!(
            state.workspace_page,
            threadlane_ui_state::WorkspacePage::GitHub
        );
    });
    retained_model.update(cx, |state, cx| {
        let status = state
            .git_statuses
            .get_mut(std::path::Path::new("/project"))
            .unwrap();
        status.remote = None;
        status.ahead = 0;
        status.behind = 0;
        status.pr.as_mut().unwrap().url = "file:///private/file".into();
        cx.notify();
    });
    cx.run_until_parked();
    cx.update(|window, cx| window.draw(cx).clear(cx));
    assert!(cx.debug_bounds("environment-git-actions").is_some());
    for selector in [
        "environment-repository",
        "environment-pr",
        "environment-sync",
    ] {
        assert!(
            cx.debug_bounds(selector).is_none(),
            "unavailable {selector}"
        );
    }
    retained_model.update(cx, |state, cx| {
        state.active_work_dir = Some("/other".into());
        cx.notify();
    });
    cx.run_until_parked();
    cx.update(|window, cx| window.draw(cx).clear(cx));
    for selector in [
        "environment-git-actions",
        "environment-repository",
        "environment-pr",
        "environment-sync",
    ] {
        assert!(cx.debug_bounds(selector).is_none(), "stale {selector}");
    }
}

#[test]
fn environment_changes_summary_handles_missing_clean_binary_and_large_totals() {
    use super::environment_changes_label;
    assert_eq!(environment_changes_label(None), "Git status unavailable");
    let mut status = threadlane_git::GitStatus::default();
    assert_eq!(
        environment_changes_label(Some(&status)),
        "No uncommitted changes"
    );
    status.files.push(threadlane_git::GitFile::default());
    assert_eq!(environment_changes_label(Some(&status)), "1 changed file");
    status.files[0].additions = u32::MAX;
    status.files[0].deletions = 3;
    status.files.push(status.files[0].clone());
    assert_eq!(
        environment_changes_label(Some(&status)),
        "2 changed files · +8589934590 −6"
    );
}

#[gpui::test]
fn trajectory_toolbar_filters_are_reachable(cx: &mut gpui::TestAppContext) {
    use gpui::AppContext as _;
    use std::collections::BTreeMap;
    use std::sync::Arc;

    cx.update(gpui_component::init);
    let model = cx.new(|_| {
        let mut state = threadlane_ui_state::AppState::default();
        state.is_new_task = false;
        state
    });
    let (chat, cx) =
        cx.add_window_view(move |window, cx| crate::TrajectoryView::new(model, window, cx));
    chat.update(cx, |chat, cx| {
        chat.trajectory_cache = Some(TrajectoryRenderCache {
            key: TrajectoryCacheKey {
                revision: 0,
                epoch: 0,
                mode: TrajectoryMode::Execution,
                query: String::new(),
                category: None,
                lane: None,
            },
            all_entries: vec![trajectory_entry("Tool", Some(1), Some(1))],
            categories: Arc::new(vec!["Tool".to_string()]),
            lanes: Arc::new(vec!["main".to_string(), "worker".to_string()]),
            lane_latest: Arc::new(BTreeMap::from([
                ("main".to_string(), "Read file".to_string()),
                ("worker".to_string(), "Write file".to_string()),
            ])),
            filtered_indices: vec![0],
            previews: vec!["Tool".into()],
            rows: vec![TrajectoryRow::Entry(0)],
            summary: TrajectorySummary::default(),
        });
        cx.notify();
    });
    cx.run_until_parked();
    cx.update(|window, cx| window.draw(cx).clear(cx));
    for selector in [
        "trajectory-mode-filter",
        "trajectory-category-filter",
        "trajectory-lane-filter",
    ] {
        assert!(
            cx.debug_bounds(selector).is_some(),
            "{selector} is reachable in the trajectory toolbar"
        );
    }
}

fn find_message(id: &str, role: MessageRole, content: &str) -> ChatMessageInfo {
    ChatMessageInfo {
        id: id.into(),
        role,
        content: content.into(),
        tool_activities: Vec::new(),
        streaming: false,
        reasoning_content: None,
        reasoning_expanded: false,
    }
}

#[test]
fn conversation_find_matches_message_rows_not_occurrences_or_hidden_payloads() {
    let mut activity = find_message("activity", MessageRole::Assistant, "");
    activity.tool_activities.push(ToolActivityInfo {
        id: "tool".into(),
        category: "tool".into(),
        title: "read_file".into(),
        display_summary: "needle".into(),
        detail: "needle".into(),
        is_expanded: false,
    });
    let mut reasoning = find_message("reasoning", MessageRole::Assistant, "");
    reasoning.reasoning_content = Some("needle".into());
    let messages = vec![
        find_message("a", MessageRole::User, "needle needle"),
        activity.clone(),
        activity,
        reasoning,
        find_message("marker", MessageRole::ContextMarker, "needle"),
        find_message("system", MessageRole::System, "needle"),
        find_message("error", MessageRole::Error, "needle"),
        find_message("queued-user-1", MessageRole::User, "needle"),
        find_message("b", MessageRole::Assistant, "needle needle"),
    ];
    let hits = super::find_conversation_messages(&messages, true, "NEEDLE");
    assert_eq!(
        hits.iter()
            .map(|hit| hit.message_id.as_str())
            .collect::<Vec<_>>(),
        ["a", "b"]
    );
    assert_eq!(
        hits.iter().map(|hit| hit.row_index).collect::<Vec<_>>(),
        [0, 6]
    );
    for query in ["", "  \n  "] {
        assert!(super::find_conversation_messages(&messages, true, query).is_empty());
    }
}

#[test]
fn conversation_find_literal_unicode_excerpts_and_wraparound() {
    let content = format!("{}İ ÉCOLE [a.*]{}", "🐈".repeat(200), "界".repeat(200));
    let messages = vec![find_message("unicode", MessageRole::Assistant, &content)];
    for query in ["i\u{307}", "école", "[a.*]"] {
        let hits = super::find_conversation_messages(&messages, false, query);
        assert_eq!(hits.len(), 1);
        assert!(hits[0].excerpt.contains("ÉCOLE [a.*]"));
        assert!(hits[0].excerpt.chars().count() <= 162);
    }
    assert!(super::find_conversation_messages(&messages, false, ".+").is_empty());
    let spaced = vec![find_message("spaced", MessageRole::User, "alpha beta")];
    assert_eq!(
        super::find_conversation_messages(&spaced, false, "alpha ").len(),
        1
    );
    assert_eq!(
        super::find_conversation_messages(&spaced, false, " beta").len(),
        1
    );
    for query in [" alpha", "beta ", "   "] {
        assert!(super::find_conversation_messages(&spaced, false, query).is_empty());
    }
    assert_eq!(super::next_find_match(None, 3, false), Some(0));
    assert_eq!(super::next_find_match(None, 3, true), Some(2));
    assert_eq!(super::next_find_match(Some(2), 3, false), Some(0));
    assert_eq!(super::next_find_match(Some(0), 3, true), Some(2));
    assert_eq!(super::next_find_match(Some(0), 0, false), None);
    let messages = vec![find_message(
        "multiline",
        MessageRole::Assistant,
        &format!("{}needle", "\n".repeat(200)),
    )];
    assert!(
        !super::find_conversation_messages(&messages, false, "needle")[0]
            .excerpt
            .contains('\n')
    );
}

#[gpui::test]
fn conversation_find_keyboard_offscreen_streaming_and_close(cx: &mut gpui::TestAppContext) {
    use gpui::{AppContext as _, Focusable as _};
    cx.update(gpui_component::init);
    cx.update(super::init);
    let model = cx.new(|_| {
        let mut state = threadlane_ui_state::AppState::default();
        state.is_new_task = false;
        state.pending_hydrations.clear();
        state.messages = (0..200)
            .map(|i| {
                find_message(
                    &format!("m{i}"),
                    MessageRole::User,
                    &format!(
                        "Message {i} {}",
                        if i == 4 || i == 150 {
                            "needle"
                        } else {
                            "other"
                        }
                    ),
                )
            })
            .collect::<Vec<_>>()
            .into();
        state
    });
    let retained = model.clone();
    let holder = std::rc::Rc::new(std::cell::RefCell::new(None));
    let capture = holder.clone();
    let (_, cx) = cx.add_window_view(move |window, cx| {
        let chat = cx.new(|cx| super::ChatListView::new(model, window, cx));
        capture.replace(Some(chat.clone()));
        gpui_component::Root::new(chat, window, cx)
    });
    let chat = holder.borrow().as_ref().unwrap().clone();
    cx.run_until_parked();
    cx.update(|window, cx| {
        chat.update(cx, |chat, cx| {
            chat.input_state.update(cx, |input, cx| {
                input.set_value("unsent draft", window, cx);
                input.select_all(window, cx);
            });
            chat.focus_composer(window, cx);
        })
    });
    cx.run_until_parked();
    cx.simulate_keystrokes(if cfg!(target_os = "macos") {
        "cmd-f"
    } else {
        "ctrl-f"
    });
    cx.run_until_parked();
    assert!(chat.read_with(cx, |chat, _| chat.find_open));
    cx.simulate_input("needle");
    cx.run_until_parked();
    cx.executor()
        .advance_clock(std::time::Duration::from_millis(200));
    cx.run_until_parked();
    chat.read_with(cx, |chat, _| {
        assert_eq!(
            chat.find_results.len(),
            2,
            "query={:?} pending={} open={} gen={}",
            chat.find_query,
            chat.find_pending,
            chat.find_open,
            chat.find_generation
        );
        assert_eq!(chat.find_selected.as_deref(), Some("m4"));
        assert!(!chat.transcript_list_state.is_following_tail());
        assert!(chat.transcript_list_state.logical_scroll_top().item_ix <= 4);
    });
    cx.simulate_keystrokes("shift-enter");
    cx.run_until_parked();
    assert_eq!(
        chat.read_with(cx, |chat, _| chat.find_selected.clone()),
        Some("m150".into())
    );
    let before = chat.read_with(cx, |chat, _| {
        chat.transcript_list_state.logical_scroll_top()
    });
    retained.update(cx, |state, cx| {
        std::sync::Arc::make_mut(&mut state.messages)[199]
            .content
            .push_str(" stream");
        cx.notify();
    });
    cx.run_until_parked();
    cx.executor()
        .advance_clock(std::time::Duration::from_millis(200));
    cx.run_until_parked();
    chat.read_with(cx, |chat, _| {
        assert_eq!(chat.find_selected.as_deref(), Some("m150"));
        assert_eq!(
            chat.transcript_list_state.logical_scroll_top().item_ix,
            before.item_ix
        );
        assert_eq!(
            chat.transcript_list_state
                .logical_scroll_top()
                .offset_in_item,
            before.offset_in_item
        );
    });
    cx.update(|window, cx| chat.update(cx, |chat, cx| chat.focus_composer(window, cx)));
    cx.run_until_parked();
    cx.simulate_keystrokes("end shift-enter");
    cx.run_until_parked();
    assert!(
        chat.read_with(cx, |chat, cx| chat
            .input_state
            .read(cx)
            .value()
            .ends_with('\n')),
        "find navigation must not intercept the composer Input context"
    );
    cx.update(|window, cx| {
        chat.update(cx, |chat, cx| {
            chat.input_state.update(cx, |input, cx| {
                input.set_value("unsent draft", window, cx);
                input.select_all(window, cx);
            });
            chat.find_input
                .update(cx, |input, cx| input.focus(window, cx));
        })
    });
    cx.run_until_parked();
    let generation = chat.read_with(cx, |chat, _| chat.find_generation);
    retained.update(cx, |_, cx| cx.notify());
    cx.run_until_parked();
    assert_eq!(
        chat.read_with(cx, |chat, _| chat.find_generation),
        generation,
        "unrelated notifications must not scan"
    );
    // Start a refresh, then replace its source before the debounce expires.
    retained.update(cx, |state, _| {
        state.is_generating = true;
        std::sync::Arc::make_mut(&mut state.messages)[199].content = "stream A".into();
    });
    chat.update(cx, |chat, cx| chat.refresh_conversation_find(false, cx));
    let scan_source = retained.update(cx, |state, _| {
        std::sync::Arc::make_mut(&mut state.messages)[199].content = "needle stream B".into();
        state.messages.clone()
    });
    // Tick only until the task captures B, leaving its background scan in flight.
    for _ in 0..1000 {
        cx.executor().tick();
        cx.dispatcher
            .scheduler()
            .clock()
            .advance(std::time::Duration::from_millis(120));
        if chat.read_with(cx, |chat, _| {
            chat.find_source
                .as_ref()
                .is_some_and(|(source, _, _)| std::sync::Arc::ptr_eq(source, &scan_source))
        }) {
            break;
        }
    }
    let generation = chat.read_with(cx, |chat, _| {
        assert!(std::sync::Arc::ptr_eq(
            &chat.find_source.as_ref().unwrap().0,
            &scan_source
        ));
        assert!(chat.find_pending);
        assert_eq!(chat.find_results.len(), 2);
        chat.find_generation
    });
    retained.update(cx, |state, cx| {
        std::sync::Arc::make_mut(&mut state.messages)[199].content = "needle stream C".into();
        cx.notify();
    });
    for _ in 0..1000 {
        cx.executor().tick();
        if chat.read_with(cx, |chat, _| chat.find_generation != generation) {
            break;
        }
    }
    chat.read_with(cx, |chat, cx| {
        assert!(
            chat.find_generation > generation,
            "stale scan must schedule a refresh"
        );
        assert!(chat.find_pending, "latest scan has not completed yet");
        assert_eq!(
            chat.find_results.len(),
            3,
            "completed B scan must be published"
        );
        assert!(chat.find_results[2].excerpt.contains("stream B"));
        assert!(chat
            .conversation_find_status(cx)
            .contains("3 matching messages"));
    });
    // Enter must still navigate while C is pending, using B's validated row IDs.
    cx.update(|window, cx| {
        window.dispatch_keystroke(gpui::Keystroke::parse("enter").unwrap(), cx);
    });
    assert_eq!(
        chat.read_with(cx, |chat, _| chat.find_selected.clone()),
        Some("m199".into())
    );
    cx.update(|window, cx| {
        window.dispatch_keystroke(gpui::Keystroke::parse("shift-enter").unwrap(), cx);
    });
    assert_eq!(
        chat.read_with(cx, |chat, _| chat.find_selected.clone()),
        Some("m150".into())
    );
    assert!(chat.read_with(cx, |chat, _| chat.find_pending));
    cx.run_until_parked();
    let mut settled_during_stream = false;
    for i in 0..12 {
        retained.update(cx, |state, cx| {
            std::sync::Arc::make_mut(&mut state.messages)[199].content = format!("stream {i}");
            cx.notify();
        });
        cx.run_until_parked();
        cx.executor()
            .advance_clock(std::time::Duration::from_millis(30));
        cx.run_until_parked();
        settled_during_stream |= chat.read_with(cx, |chat, _| !chat.find_pending);
    }
    assert!(
        settled_during_stream,
        "streaming must not indefinitely restart the debounce timer"
    );
    retained.update(cx, |state, cx| {
        let messages = std::sync::Arc::make_mut(&mut state.messages);
        for message in messages.iter_mut() {
            message.id = format!("durable-{}", message.id);
        }
        messages.push(find_message("hydrated-extra", MessageRole::User, "history"));
        cx.notify();
    });
    cx.run_until_parked();
    cx.executor()
        .advance_clock(std::time::Duration::from_millis(200));
    cx.run_until_parked();
    chat.read_with(cx, |chat, _| {
        assert!(chat.find_selected.is_none());
        assert_eq!(
            chat.transcript_list_state.logical_scroll_top().item_ix,
            before.item_ix,
            "hydration must not reset a find reader to the streaming tail"
        );
    });
    cx.simulate_keystrokes("escape");
    cx.run_until_parked();
    cx.update(|window, cx| {
        chat.read_with(cx, |chat, cx| {
            assert!(!chat.find_open);
            assert_eq!(chat.input_state.read(cx).value().as_str(), "unsent draft");
            assert_eq!(chat.input_state.read(cx).selected_range(), 0..12);
            assert!(chat
                .input_state
                .read(cx)
                .focus_handle(cx)
                .is_focused(window));
            assert_eq!(
                chat.transcript_list_state.logical_scroll_top().item_ix,
                before.item_ix
            );
            assert_eq!(
                chat.transcript_list_state
                    .logical_scroll_top()
                    .offset_in_item,
                before.offset_in_item
            );
        })
    });
    cx.simulate_keystrokes(if cfg!(target_os = "macos") {
        "cmd-f"
    } else {
        "ctrl-f"
    });
    chat.update(cx, |chat, cx| {
        chat.progress_summary_expanded = true;
        cx.notify();
    });
    cx.run_until_parked();
    let open = cx
        .debug_bounds("progress-open-trajectory")
        .expect("trajectory action visible");
    cx.simulate_click(open.center(), gpui::Modifiers::default());
    cx.run_until_parked();
    chat.read_with(cx, |chat, _| {
        assert_eq!(chat.current_tab, super::CentralTab::Chat);
        assert!(!chat.progress_summary_expanded);
        assert!(!chat.find_open);
        assert!(chat.find_results.is_empty());
        assert!(chat.find_task.is_none());
    });

}

#[gpui::test]
fn conversation_find_rejects_stale_queries_sessions_and_reconciled_ids(
    cx: &mut gpui::TestAppContext,
) {
    use gpui::AppContext as _;
    cx.update(gpui_component::init);
    cx.update(super::init);
    let model = cx.new(|_| {
        let mut state = threadlane_ui_state::AppState::default();
        state.pending_hydrations.clear();
        state.is_new_task = false;
        state.messages = vec![
            find_message("optimistic", MessageRole::User, "alpha"),
            find_message("second", MessageRole::Assistant, "bravo"),
        ]
        .into();
        state
    });
    let retained = model.clone();
    let holder = std::rc::Rc::new(std::cell::RefCell::new(None));
    let capture = holder.clone();
    let (_, cx) = cx.add_window_view(move |window, cx| {
        let chat = cx.new(|cx| super::ChatListView::new(model, window, cx));
        capture.replace(Some(chat.clone()));
        gpui_component::Root::new(chat, window, cx)
    });
    let chat = holder.borrow().as_ref().unwrap().clone();
    cx.run_until_parked();
    cx.update(|window, cx| chat.update(cx, |chat, cx| chat.focus_composer(window, cx)));
    cx.run_until_parked();
    let shortcut = if cfg!(target_os = "macos") {
        "cmd-f"
    } else {
        "ctrl-f"
    };
    cx.simulate_keystrokes(shortcut);
    cx.run_until_parked();
    cx.simulate_input("alpha");
    cx.run_until_parked();
    cx.simulate_keystrokes(shortcut);
    cx.simulate_input("bravo");
    cx.simulate_keystrokes("enter");
    assert!(chat.read_with(cx, |chat, _| chat.find_selected.is_none()));
    cx.run_until_parked();
    cx.executor()
        .advance_clock(std::time::Duration::from_millis(200));
    cx.run_until_parked();
    assert_eq!(
        chat.read_with(cx, |chat, _| chat.find_selected.clone()),
        Some("second".into())
    );
    retained.update(cx, |state, cx| {
        let messages = std::sync::Arc::make_mut(&mut state.messages);
        messages[1].id = "hydrated".into();
        cx.notify();
    });
    cx.run_until_parked();
    cx.executor()
        .advance_clock(std::time::Duration::from_millis(200));
    cx.run_until_parked();
    chat.read_with(cx, |chat, _| {
        assert!(
            chat.find_selected.is_none(),
            "equal text must not migrate a selected identity"
        );
        assert_eq!(chat.find_results[0].message_id, "hydrated");
    });
    cx.simulate_keystrokes("enter");
    cx.run_until_parked();
    assert_eq!(
        chat.read_with(cx, |chat, _| chat.find_selected.clone()),
        Some("hydrated".into())
    );
    retained.update(cx, |state, cx| {
        std::sync::Arc::make_mut(&mut state.messages)[1].content = "other".into(); // same byte length
        cx.notify();
    });
    cx.run_until_parked();
    cx.executor()
        .advance_clock(std::time::Duration::from_millis(200));
    cx.run_until_parked();
    assert!(chat.read_with(cx, |chat, _| chat.find_results.is_empty()
        && chat.find_selected.is_none()));
    cx.simulate_keystrokes(shortcut);
    cx.simulate_input("alpha");
    retained.update(cx, |state, cx| {
        state.active_session_id = Some("different-session".into());
        cx.notify();
    });
    cx.run_until_parked();
    cx.executor()
        .advance_clock(std::time::Duration::from_millis(200));
    cx.run_until_parked();
    assert!(chat.read_with(cx, |chat, _| !chat.find_open
        && chat.find_results.is_empty()));
}

#[gpui::test]
fn conversation_find_escape_dialog_and_other_focus_contexts(cx: &mut gpui::TestAppContext) {
    use gpui::{
        AppContext as _, Focusable as _, InteractiveElement as _, ParentElement as _, Styled as _,
    };
    use gpui_component::WindowExt as _;
    gpui::actions!(find_test, [CancelTurn]);
    struct Host {
        chat: gpui::Entity<super::ChatListView>,
        other: gpui::Entity<gpui_component::input::InputState>,
        cancelled: std::rc::Rc<std::cell::Cell<bool>>,
        context: &'static str,
    }
    impl gpui::Render for Host {
        fn render(
            &mut self,
            _: &mut gpui::Window,
            _: &mut gpui::Context<Self>,
        ) -> impl gpui::IntoElement {
            let cancelled = self.cancelled.clone();
            gpui::div()
                .size_full()
                .flex()
                .flex_col()
                .key_context("FindTestWorkspace")
                .on_action(move |_: &CancelTurn, _, _| cancelled.set(true))
                .child(self.chat.clone())
                .child(
                    gpui::div()
                        .key_context(self.context)
                        .child(gpui_component::input::Input::new(&self.other)),
                )
        }
    }
    cx.update(|cx| {
        gpui_component::init(cx);
        super::init(cx);
        cx.bind_keys([gpui::KeyBinding::new(
            "escape",
            CancelTurn,
            Some("FindTestWorkspace"),
        )]);
    });
    let model = cx.new(|_| {
        let mut state = threadlane_ui_state::AppState::default();
        state.pending_hydrations.clear();
        state.is_new_task = false;
        state
    });
    let holder = std::rc::Rc::new(std::cell::RefCell::new(None));
    let capture = holder.clone();
    let cancelled = std::rc::Rc::new(std::cell::Cell::new(false));
    let tracked = cancelled.clone();
    let (_, cx) = cx.add_window_view(move |window, cx| {
        let chat = cx.new(|cx| super::ChatListView::new(model, window, cx));
        let other = cx.new(|cx| gpui_component::input::InputState::new(window, cx));
        let host = cx.new(|_| Host {
            chat: chat.clone(),
            other: other.clone(),
            cancelled: tracked,
            context: "Terminal",
        });
        capture.replace(Some((chat, other, host.clone())));
        gpui_component::Root::new(host, window, cx)
    });
    let (chat, other, host) = holder.borrow().as_ref().unwrap().clone();
    cx.run_until_parked();
    let shortcut = if cfg!(target_os = "macos") {
        "cmd-f"
    } else {
        "ctrl-f"
    };
    for context in ["Terminal", "Editor", "Browser"] {
        host.update(cx, |host, cx| {
            host.context = context;
            cx.notify();
        });
        cx.update(|window, cx| window.focus(&other.read(cx).focus_handle(cx), cx));
        cx.run_until_parked();
        cx.simulate_keystrokes(shortcut);
        cx.run_until_parked();
        assert!(
            !chat.read_with(cx, |chat, _| chat.find_open),
            "must not intercept {context}"
        );
    }
    cx.update(|window, cx| chat.update(cx, |chat, cx| chat.focus_composer(window, cx)));
    cx.run_until_parked();
    cx.simulate_keystrokes(shortcut);
    cx.run_until_parked();
    cx.update(|window, cx| window.open_dialog(cx, |dialog, _, _| dialog.title("Topmost dialog")));
    cx.run_until_parked();
    cx.simulate_keystrokes("escape");
    cx.run_until_parked();
    cx.update(|window, cx| assert!(!window.has_active_dialog(cx)));
    assert!(chat.read_with(cx, |chat, _| chat.find_open));
    assert!(!cancelled.get());
    cx.simulate_keystrokes("escape");
    cx.run_until_parked();
    assert!(!chat.read_with(cx, |chat, _| chat.find_open));
    assert!(!cancelled.get(), "closing find must consume Escape");
    cx.simulate_keystrokes("escape");
    cx.run_until_parked();
    assert!(
        cancelled.get(),
        "the workspace gets Escape only after find closes"
    );
}

#[gpui::test]
fn conversation_find_same_count_row_replacement_is_reachable(cx: &mut gpui::TestAppContext) {
    use gpui::AppContext as _;
    cx.update(gpui_component::init);
    let model = cx.new(|_| threadlane_ui_state::AppState::default());
    let (_, cx) = cx.add_window_view(move |window, cx| {
        let chat = cx.new(|cx| super::ChatListView::new(model, window, cx));
        chat.update(cx, |chat, _| {
            let mut activity = find_message("tool", MessageRole::Assistant, "");
            activity.tool_activities.push(ToolActivityInfo {
                id: "tool".into(),
                category: "tool".into(),
                title: "read_file".into(),
                display_summary: String::new(),
                detail: String::new(),
                is_expanded: false,
            });
            let before = vec![
                activity,
                find_message("user", MessageRole::User, "question"),
            ];
            chat.sync_transcript_rows(before.clone().into(), true, true);
            let mut after = before;
            after[0].content = "answer needle".into();
            chat.sync_transcript_rows(after.into(), true, false);
            assert_eq!(chat.transcript_rows[0], TranscriptRow::Message(0));
        });
        gpui_component::Root::new(chat, window, cx)
    });
    cx.run_until_parked();
}

#[test]
fn conversation_find_large_history_profile() {
    let mut messages: Vec<_> = (0..20_000)
        .map(|i| {
            find_message(
                &format!("m{i}"),
                MessageRole::Assistant,
                &format!(
                    "{} {}",
                    "Representative coding conversation text and code. ".repeat(20),
                    if i % 100 == 0 { "needle" } else { "other" }
                ),
            )
        })
        .collect();
    messages.insert(
        10_000,
        find_message("compacted", MessageRole::ContextMarker, "Compacted"),
    );
    let start = std::time::Instant::now();
    let hits = super::find_conversation_messages(&messages, false, "needle");
    eprintln!(
        "conversation find: 20,000 messages (~19 MB), {} hits, {:?}",
        hits.len(),
        start.elapsed()
    );
    assert_eq!(hits.len(), 200);
    assert_eq!(
        hits[0].message_id, "m0",
        "pre-compaction history is searchable"
    );
    assert_eq!(hits[199].message_id, "m19900");
}

#[gpui::test]
fn conversation_find_controls_fit_themes_narrow_panes_and_large_text(
    cx: &mut gpui::TestAppContext,
) {
    use gpui::AppContext as _;
    cx.update(gpui_component::init);
    cx.update(super::init);
    let model = cx.new(|_| threadlane_ui_state::AppState::default());
    let holder = std::rc::Rc::new(std::cell::RefCell::new(None));
    let capture = holder.clone();
    let (_, cx) = cx.add_window_view(move |window, cx| {
        let chat = cx.new(|cx| super::ChatListView::new(model, window, cx));
        capture.replace(Some(chat.clone()));
        gpui_component::Root::new(chat, window, cx)
    });
    let chat = holder.borrow().as_ref().unwrap().clone();
    cx.run_until_parked();
    cx.update(|window, cx| chat.update(cx, |chat, cx| chat.focus_composer(window, cx)));
    cx.run_until_parked();
    cx.simulate_keystrokes(if cfg!(target_os = "macos") {
        "cmd-f"
    } else {
        "ctrl-f"
    });
    for mode in [
        gpui_component::ThemeMode::Light,
        gpui_component::ThemeMode::Dark,
    ] {
        for font in [14., 20.] {
            cx.update(|window, cx| {
                gpui_component::Theme::change(mode, Some(window), cx);
                gpui_component::Theme::global_mut(cx).font_size = gpui::px(font);
            });
            for width in [320., 800.] {
                cx.simulate_resize(gpui::size(gpui::px(width), gpui::px(800.)));
                cx.run_until_parked();
                cx.update(|window, cx| window.draw(cx).clear(cx));
                for selector in [
                    "central-tab-chat",
                    "central-tab-editor",
                    "conversation-find-open",
                    "conversation-find-previous",
                    "conversation-find-next",
                    "conversation-find-close",
                ] {
                    let bounds = cx.debug_bounds(selector).expect("find control visible");
                    assert!(bounds.size.width > gpui::px(0.));
                    let right_edge = if selector.starts_with("central-tab-")
                        || selector == "conversation-find-open"
                    {
                        width - font * 8. // Header reserves pr_32 for workspace controls.
                    } else {
                        width
                    };
                    assert!(
                        bounds.left() >= gpui::px(0.) && bounds.right() <= gpui::px(right_edge),
                        "{selector} overflows at width {width}, font {font}: {bounds:?}"
                    );
                }
            }
        }
    }
}

#[gpui::test]
fn conversation_find_loading_failure_and_empty_are_distinct(cx: &mut gpui::TestAppContext) {
    use gpui::AppContext as _;
    cx.update(gpui_component::init);
    cx.update(super::init);
    let model = cx.new(|_| {
        let mut state = threadlane_ui_state::AppState::default();
        state.pending_hydrations.clear();
        state.active_work_dir = Some("/tmp/threadlane-find-loading".into());
        state.active_session_id = Some("find-session".into());
        state.messages = Vec::new().into();
        state.is_new_task = false;
        state.session_status = None;
        state
    });
    let retained = model.clone();
    let holder = std::rc::Rc::new(std::cell::RefCell::new(None));
    let capture = holder.clone();
    let (_, cx) = cx.add_window_view(move |window, cx| {
        let chat = cx.new(|cx| super::ChatListView::new(model, window, cx));
        capture.replace(Some(chat.clone()));
        gpui_component::Root::new(chat, window, cx)
    });
    let chat = holder.borrow().as_ref().unwrap().clone();
    cx.run_until_parked();
    cx.update(|window, cx| chat.update(cx, |chat, cx| chat.focus_composer(window, cx)));
    cx.run_until_parked();
    cx.simulate_keystrokes(if cfg!(target_os = "macos") {
        "cmd-f"
    } else {
        "ctrl-f"
    });
    cx.run_until_parked();
    assert_eq!(
        chat.read_with(cx, |chat, cx| chat.conversation_find_status(cx)),
        "Type to find a message"
    );
    retained.update(cx, |state, cx| {
        state
            .pending_hydrations
            .push(threadlane_ui_state::SessionHydrationRequest {
                session_id: "find-session".into(),
                session_file:
                    "/tmp/threadlane-find-loading/.threadlane/sessions/find-session.jsonl".into(),
                reload_messages: true,
                runtime_options: None,
            });
        cx.notify();
    });
    cx.run_until_parked();
    cx.simulate_input("needle");
    cx.run_until_parked();
    assert_eq!(
        chat.read_with(cx, |chat, cx| chat.conversation_find_status(cx)),
        "Loading conversation…"
    );
    retained.update(cx, |state, cx| {
        state.pending_hydrations.clear();
        state.session_status = Some("Could not load session: unreadable fixture".into());
        cx.notify();
    });
    cx.run_until_parked();
    cx.executor()
        .advance_clock(std::time::Duration::from_millis(200));
    cx.run_until_parked();
    assert_eq!(
        chat.read_with(cx, |chat, cx| chat.conversation_find_status(cx)),
        "Could not load session: unreadable fixture"
    );
    retained.update(cx, |state, cx| {
        state.session_status = None;
        cx.notify();
    });
    cx.run_until_parked();
    assert_eq!(
        chat.read_with(cx, |chat, cx| chat.conversation_find_status(cx)),
        "No matching messages"
    );
    chat.update(cx, |chat, cx| chat.set_tab(super::CentralTab::Editor, cx));
    assert!(!chat.read_with(cx, |chat, _| chat.find_open));
}

#[gpui::test]
fn worktree_base_picker_is_scoped_to_new_worktree_tasks(cx: &mut gpui::TestAppContext) {
    use gpui::AppContext as _;
    cx.update(gpui_component::init);
    let model = cx.new(|_| {
        let mut state = threadlane_ui_state::AppState::default();
        state.is_new_task = true;
        state.active_session_id = None;
        state.pending_hydrations.clear();
        state.draft_work_mode = threadlane_ui_state::WorkMode::Worktree;
        state.draft_worktree_base = Some("origin/main".into());
        state.draft_worktree_bases = vec!["origin/main".into(), "release".into()];
        state
    });
    let retained = model.clone();
    let (_, cx) = cx.add_window_view(move |window, cx| {
        let chat = cx.new(|cx| super::ChatListView::new(model, window, cx));
        gpui_component::Root::new(chat, window, cx)
    });
    for width in [320.0, 800.0] {
        cx.simulate_resize(gpui::size(gpui::px(width), gpui::px(800.0)));
        cx.run_until_parked();
        cx.update(|window, cx| window.draw(cx).clear(cx));
        let bounds = cx.debug_bounds("composer-worktree-base").unwrap();
        assert!(bounds.right() <= gpui::px(width));
        cx.simulate_click(bounds.center(), gpui::Modifiers::default());
        cx.run_until_parked();
        cx.simulate_keystrokes("escape");
        assert_eq!(
            retained
                .read_with(cx, |s, _| s.draft_worktree_base.clone())
                .as_deref(),
            Some("origin/main")
        );
    }
    retained.update(cx, |state, cx| {
        state.draft_work_mode = threadlane_ui_state::WorkMode::Local;
        cx.notify();
    });
    cx.run_until_parked();
    cx.update(|window, cx| window.draw(cx).clear(cx));
    assert!(cx.debug_bounds("composer-worktree-base").is_none());
}

#[gpui::test]
fn environment_git_menu_dispatches_commands_and_dismisses(cx: &mut gpui::TestAppContext) {
    use gpui::{
        AppContext as _, InteractiveElement as _, ParentElement as _,
        StatefulInteractiveElement as _, Styled as _,
    };
    struct Host {
        focus: gpui::FocusHandle,
        chat: gpui::Entity<super::ChatListView>,
        selected: std::rc::Rc<std::cell::Cell<&'static str>>,
    }
    impl gpui::Render for Host {
        fn render(
            &mut self,
            _: &mut gpui::Window,
            cx: &mut gpui::Context<Self>,
        ) -> impl gpui::IntoElement {
            gpui::div()
                .id("environment-host")
                .track_focus(&self.focus)
                .role(gpui::Role::Application)
                .size_full()
                .flex()
                .on_action(cx.listener(|this, _: &crate::OpenWorkspaceCommit, _, _| {
                    this.selected.set("commit")
                }))
                .on_action(cx.listener(|this, _: &crate::PullWorkspaceBranch, _, _| {
                    this.selected.set("pull")
                }))
                .on_action(cx.listener(|this, _: &crate::PushWorkspaceBranch, _, _| {
                    this.selected.set("push")
                }))
                .on_action(
                    cx.listener(|this, _: &crate::CreateWorkspacePullRequest, _, _| {
                        this.selected.set("pr")
                    }),
                )
                .on_action(cx.listener(|this, _: &crate::CreateWorkspaceBranch, _, _| {
                    this.selected.set("branch")
                }))
                .child(self.chat.clone())
        }
    }
    cx.update(gpui_component::init);
    let model = cx.new(|_| {
        let mut state = threadlane_ui_state::AppState::default();
        state.is_new_task = false;
        state.active_work_dir = Some("/project".into());
        state.active_session_id = None;
        let mut file = threadlane_git::GitFile::default();
        file.path = "changed.rs".into();
        state.git_statuses.insert(
            "/project".into(),
            threadlane_git::GitStatus {
                branch: Some("feature".into()),
                remote: Some("git@github.com:owner/repo.git".into()),
                has_upstream: true,
                pr_ready: true,
                pr_lookup_available: true,
                files: vec![file],
                ..Default::default()
            },
        );
        state
    });
    let retained_model = model.clone();
    let selected = std::rc::Rc::new(std::cell::Cell::new(""));
    let captured = selected.clone();
    let (_, cx) = cx.add_window_view(move |window, cx| {
        let chat = cx.new(|cx| {
            let mut chat = super::ChatListView::new(model, window, cx);
            chat.environment_available = true;
            chat
        });
        let focus = cx.focus_handle();
        focus.focus(window, cx);
        let host = cx.new(|_| Host {
            chat,
            selected: captured,
            focus,
        });
        gpui_component::Root::new(host, window, cx)
    });
    cx.run_until_parked();
    cx.update(|window, cx| window.draw(cx).clear(cx));
    for (index, command) in ["commit", "pull", "push", "pr", "branch"]
        .into_iter()
        .enumerate()
    {
        cx.update(|window, cx| window.draw(cx).clear(cx));
        let menu = cx.debug_bounds("environment-git-actions").unwrap();
        cx.simulate_click(menu.center(), gpui::Modifiers::default());
        cx.run_until_parked();
        for _ in 0..=index {
            cx.simulate_keystrokes("down");
        }
        cx.simulate_keystrokes("enter");
        cx.run_until_parked();
        assert_eq!(selected.replace(""), command);
    }
    cx.update(|window, cx| window.draw(cx).clear(cx));
    let menu = cx.debug_bounds("environment-git-actions").unwrap();
    cx.simulate_click(menu.center(), gpui::Modifiers::default());
    cx.simulate_keystrokes("escape");
    cx.run_until_parked();
    assert_eq!(selected.get(), "", "Escape must not dispatch a Git action");

    for blocked in ["upstream", "commits", "lookup", "existing-pr"] {
        retained_model.update(cx, |state, cx| {
            let status = state
                .git_statuses
                .get_mut(std::path::Path::new("/project"))
                .unwrap();
            status.has_upstream = blocked != "upstream";
            status.pr_ready = blocked != "commits";
            status.pr_lookup_available = blocked != "lookup";
            status.pr = (blocked == "existing-pr").then(Default::default);
            cx.notify();
        });
        cx.run_until_parked();
        cx.update(|window, cx| window.draw(cx).clear(cx));
        let menu = cx.debug_bounds("environment-git-actions").unwrap();
        cx.simulate_click(menu.center(), gpui::Modifiers::default());
        cx.run_until_parked();
        cx.simulate_keystrokes(if blocked == "upstream" {
            "down down enter"
        } else {
            "down down down enter"
        });
        cx.run_until_parked();
        assert_eq!(
            selected.replace(""),
            "push",
            "Push stays enabled; Pull requires upstream"
        );
        cx.update(|window, cx| window.draw(cx).clear(cx));
        let menu = cx.debug_bounds("environment-git-actions").unwrap();
        cx.simulate_click(menu.center(), gpui::Modifiers::default());
        cx.run_until_parked();
        cx.simulate_keystrokes(if blocked == "upstream" {
            "down down down enter"
        } else {
            "down down down down enter"
        });
        cx.run_until_parked();
        assert_eq!(
            selected.replace(""),
            "branch",
            "PR creation must be skipped for {blocked}"
        );
    }

    retained_model.update(cx, |state, cx| {
        let status = state
            .git_statuses
            .get_mut(std::path::Path::new("/project"))
            .unwrap();
        status.files.clear();
        status.remote = None;
        cx.notify();
    });
    cx.run_until_parked();
    cx.update(|window, cx| window.draw(cx).clear(cx));
    let menu = cx.debug_bounds("environment-git-actions").unwrap();
    cx.simulate_click(menu.center(), gpui::Modifiers::default());
    cx.run_until_parked();
    // The first Down selects the disabled first row; the next skips remote-only actions.
    cx.simulate_keystrokes("down down enter");
    cx.run_until_parked();
    assert_eq!(
        selected.replace(""),
        "branch",
        "A local-only checkout offers branch creation, not remote operations"
    );
}


#[test]
fn staged_image_decoder_converts_png_pixels_to_gpui_bgra() {
    use base64::Engine as _;

    let rgba = image::RgbaImage::from_pixel(1, 1, image::Rgba([10, 20, 30, 255]));
    let mut encoded = std::io::Cursor::new(Vec::new());
    image::DynamicImage::ImageRgba8(rgba)
        .write_to(&mut encoded, image::ImageFormat::Png)
        .unwrap();
    let attachment = threadlane_protocol::ImageAttachment {
        display_name: "test.png".into(),
        data_url: format!(
            "data:image/png;base64,{}",
            base64::engine::general_purpose::STANDARD.encode(encoded.into_inner())
        ),
    };

    let decoded = super::decode_staged_image(&attachment).unwrap();
    assert_eq!(decoded.as_bytes(0).unwrap(), &[30, 20, 10, 255]);
}

#[test]
fn staged_image_decoder_rejects_non_image_data_urls() {
    let attachment = threadlane_protocol::ImageAttachment {
        display_name: "invalid.png".into(),
        data_url: "data:text/plain;base64,aGVsbG8=".into(),
    };

    assert!(super::decode_staged_image(&attachment).is_err());
}

#[test]
fn prompt_landmarks_list_user_prompts_in_chronological_order() {
    let msg = |id: &str, role: MessageRole, content: &str| ChatMessageInfo {
        id: id.into(),
        role,
        content: content.into(),
        tool_activities: Vec::new(),
        streaming: false,
        reasoning_content: None,
        reasoning_expanded: false,
    };
    let messages = vec![
        msg("u1", MessageRole::User, "first prompt\nwith newlines"),
        msg("a1", MessageRole::Assistant, "answer"),
        msg("u2", MessageRole::User, "second   prompt"),
        msg("e1", MessageRole::Error, "boom"),
        msg("u3", MessageRole::User, ""),
        msg("queued-user-9", MessageRole::User, "not yet sent"),
    ];

    let landmarks = super::prompt_landmarks(&messages, false);
    assert_eq!(
        landmarks.iter().map(|l| l.message_id.as_str()).collect::<Vec<_>>(),
        ["u1", "u2", "u3", "queued-user-9"]
    );
    assert_eq!(
        landmarks.iter().map(|l| l.ordinal).collect::<Vec<_>>(),
        [1, 2, 3, 4]
    );
    assert!(landmarks[0].row_index < landmarks[1].row_index);
    // Excerpts collapse all whitespace runs into single spaces.
    assert_eq!(landmarks[0].excerpt, "first prompt with newlines");
    assert_eq!(landmarks[1].excerpt, "second prompt");
    assert_eq!(landmarks[2].excerpt, "");
    // Optimistic queue/steer echoes are flagged, never silently dropped here.
    assert!(!landmarks[0].pending_echo);
    assert!(landmarks[3].pending_echo);
    // While generating, queued echoes are excluded like other transcript rows.
    let live = super::prompt_landmarks(&messages, true);
    assert_eq!(live.len(), 3);
    assert!(live.iter().all(|l| !l.pending_echo));
}

#[test]
fn prompt_recall_step_saturates_old_and_clears_past_newest() {
    use super::prompt_recall_step;
    let (old, new) = (true, false);
    assert_eq!(prompt_recall_step(None, 0, old), PromptRecallStep::PassThrough);
    assert_eq!(prompt_recall_step(None, 0, new), PromptRecallStep::PassThrough);
    // Entering browses at the newest entry; Down without browsing is native.
    assert_eq!(prompt_recall_step(None, 3, old), PromptRecallStep::Load(2));
    assert_eq!(prompt_recall_step(None, 3, new), PromptRecallStep::PassThrough);
    // Older saturates at the oldest entry (index 0).
    assert_eq!(prompt_recall_step(Some(2), 3, old), PromptRecallStep::Load(1));
    assert_eq!(prompt_recall_step(Some(0), 3, old), PromptRecallStep::Load(0));
    // Newer past the newest restores the empty composer.
    assert_eq!(prompt_recall_step(Some(0), 3, new), PromptRecallStep::Load(1));
    assert_eq!(prompt_recall_step(Some(2), 3, new), PromptRecallStep::Clear);
}

#[test]
fn step_prompt_focus_moves_without_wrapping() {
    use super::step_prompt_focus;
    assert_eq!(step_prompt_focus(None, 0, "up"), None);
    assert_eq!(step_prompt_focus(None, 3, "down"), Some(0));
    assert_eq!(step_prompt_focus(Some(0), 3, "up"), Some(0));
    assert_eq!(step_prompt_focus(Some(0), 3, "down"), Some(1));
    assert_eq!(step_prompt_focus(Some(2), 3, "down"), Some(2));
    assert_eq!(step_prompt_focus(Some(1), 3, "home"), Some(0));
    assert_eq!(step_prompt_focus(Some(0), 3, "end"), Some(2));
    assert_eq!(step_prompt_focus(Some(1), 3, "enter"), Some(1));
}

fn prompt_test_messages() -> Vec<ChatMessageInfo> {
    let msg = |id: &str, role: MessageRole, content: &str| ChatMessageInfo {
        id: id.into(),
        role,
        content: content.into(),
        tool_activities: Vec::new(),
        streaming: false,
        reasoning_content: None,
        reasoning_expanded: false,
    };
    vec![
        msg("u1", MessageRole::User, "first prompt"),
        msg("a1", MessageRole::Assistant, "answer one"),
        msg("u2", MessageRole::User, "second prompt"),
        msg("u3", MessageRole::User, "third prompt"),
    ]
}

#[gpui::test]
fn composer_up_recalls_prompts_and_down_restores_empty(cx: &mut gpui::TestAppContext) {
    use gpui::AppContext as _;
    cx.update(|cx| {
        gpui_component::init(cx);
        super::init(cx);
    });
    let model = cx.new(|_| {
        let mut state = threadlane_ui_state::AppState::default();
        state.is_new_task = false;
        state.active_session_id = Some("recall-task".into());
        state.messages = prompt_test_messages().into();
        state
    });
    let holder: std::rc::Rc<
        std::cell::RefCell<Option<gpui::Entity<super::ChatListView>>>,
    > = Default::default();
    let holder_clone = holder.clone();
    let (_, cx) = cx.add_window_view(move |window, cx| {
        let chat = cx.new(|cx| super::ChatListView::new(model, window, cx));
        holder_clone.borrow_mut().replace(chat.clone());
        gpui_component::Root::new(chat, window, cx)
    });
    let chat = holder.borrow().as_ref().expect("chat mounted").clone();
    chat.update_in(cx, |chat, window, cx| chat.focus_composer(window, cx));
    cx.run_until_parked();
    cx.update(|window, cx| window.draw(cx).clear(cx));

    cx.simulate_keystrokes("up");
    chat.read_with(cx, |chat, cx| {
        let input = chat.input_state.read(cx);
        assert_eq!(input.value().as_ref(), "third prompt");
        assert_eq!(input.cursor(), 0, "older navigation leaves the caret at the start");
        assert!(chat.prompt_recall.is_some());
    });
    cx.simulate_keystrokes("up up");
    chat.read_with(cx, |chat, cx| {
        assert_eq!(chat.input_state.read(cx).value().as_ref(), "first prompt");
    });
    // Older saturates at the oldest entry.
    cx.simulate_keystrokes("up");
    chat.read_with(cx, |chat, cx| {
        assert_eq!(chat.input_state.read(cx).value().as_ref(), "first prompt");
    });
    // Down only goes newer when the caret is at the end; from the start it
    // keeps its native caret behavior.
    cx.simulate_keystrokes("down");
    chat.read_with(cx, |chat, cx| {
        assert_eq!(chat.input_state.read(cx).value().as_ref(), "first prompt");
        assert!(chat.prompt_recall.is_some());
    });
    chat.update_in(cx, |chat, _, cx| {
        let len = chat.input_state.read(cx).value().len();
        chat.input_state
            .update(cx, |input, cx| input.set_selected_range(len..len, cx));
    });
    cx.simulate_keystrokes("down down");
    chat.read_with(cx, |chat, cx| {
        assert_eq!(chat.input_state.read(cx).value().as_ref(), "third prompt");
        assert_eq!(
            chat.input_state.read(cx).cursor(),
            "third prompt".len(),
            "newer navigation leaves the caret at the end"
        );
    });
    // Newer past the newest restores the empty composer.
    cx.simulate_keystrokes("down");
    chat.read_with(cx, |chat, cx| {
        assert_eq!(chat.input_state.read(cx).value().as_ref(), "");
        assert!(chat.prompt_recall.is_none());
    });
}

#[gpui::test]
fn composer_recall_ends_on_edit_and_stays_gated(cx: &mut gpui::TestAppContext) {
    use gpui::AppContext as _;
    cx.update(|cx| {
        gpui_component::init(cx);
        super::init(cx);
    });
    let model = cx.new(|_| {
        let mut state = threadlane_ui_state::AppState::default();
        state.is_new_task = false;
        state.active_session_id = Some("recall-task".into());
        state.messages = prompt_test_messages().into();
        state
    });
    let retained_model = model.clone();
    let holder: std::rc::Rc<
        std::cell::RefCell<Option<gpui::Entity<super::ChatListView>>>,
    > = Default::default();
    let holder_clone = holder.clone();
    let (_, cx) = cx.add_window_view(move |window, cx| {
        let chat = cx.new(|cx| super::ChatListView::new(model, window, cx));
        holder_clone.borrow_mut().replace(chat.clone());
        gpui_component::Root::new(chat, window, cx)
    });
    let chat = holder.borrow().as_ref().expect("chat mounted").clone();
    chat.update_in(cx, |chat, window, cx| chat.focus_composer(window, cx));
    cx.run_until_parked();
    cx.update(|window, cx| window.draw(cx).clear(cx));

    // Any draft text, even whitespace, keeps arrows native.
    chat.update_in(cx, |chat, window, cx| {
        chat.input_state
            .update(cx, |input, cx| input.set_value("   ", window, cx));
    });
    cx.simulate_keystrokes("up");
    chat.read_with(cx, |chat, cx| {
        assert_eq!(chat.input_state.read(cx).value().as_ref(), "   ");
        assert!(chat.prompt_recall.is_none());
    });
    chat.update_in(cx, |chat, window, cx| {
        chat.input_state
            .update(cx, |input, cx| input.set_value("", window, cx));
    });

    // Staged images keep the composer busy.
    chat.update(cx, |chat, _| {
        chat.pasted_images.push(super::ImageAttachment {
            display_name: "draft.png".into(),
            data_url: "data:image/png;base64,test".into(),
        });
    });
    cx.simulate_keystrokes("up");
    chat.read_with(cx, |chat, _| assert!(chat.prompt_recall.is_none()));
    chat.update(cx, |chat, _| chat.pasted_images.clear());

    // An active turn disables recall.
    retained_model.update(cx, |state, cx| {
        state.is_generating = true;
        cx.notify();
    });
    cx.simulate_keystrokes("up");
    chat.read_with(cx, |chat, _| assert!(chat.prompt_recall.is_none()));
    retained_model.update(cx, |state, cx| {
        state.is_generating = false;
        cx.notify();
    });

    // Editing the recalled text ends browsing but keeps the draft.
    cx.simulate_keystrokes("up");
    cx.run_until_parked();
    chat.read_with(cx, |chat, cx| {
        assert_eq!(chat.input_state.read(cx).value().as_ref(), "third prompt");
    });
    // The caret sits at the start after older navigation, so the typed
    // character prepends.
    cx.simulate_input("!");
    cx.run_until_parked();
    chat.read_with(cx, |chat, cx| {
        assert_eq!(chat.input_state.read(cx).value().as_ref(), "!third prompt");
        assert!(chat.prompt_recall.is_none(), "typing ends browsing");
    });
    // With the composer now holding a draft, Up is native again.
    cx.simulate_keystrokes("up");
    chat.read_with(cx, |chat, cx| {
        assert_eq!(chat.input_state.read(cx).value().as_ref(), "!third prompt");
        assert!(chat.prompt_recall.is_none());
    });
}

#[gpui::test]
fn prompt_recall_buttons_step_and_report_position(cx: &mut gpui::TestAppContext) {
    use gpui::AppContext as _;
    cx.update(gpui_component::init);
    let model = cx.new(|_| {
        let mut state = threadlane_ui_state::AppState::default();
        state.is_new_task = false;
        state.active_session_id = Some("recall-task".into());
        state.messages = prompt_test_messages().into();
        state
    });
    let holder: std::rc::Rc<
        std::cell::RefCell<Option<gpui::Entity<super::ChatListView>>>,
    > = Default::default();
    let holder_clone = holder.clone();
    let (_, cx) = cx.add_window_view(move |window, cx| {
        let chat = cx.new(|cx| super::ChatListView::new(model, window, cx));
        holder_clone.borrow_mut().replace(chat.clone());
        gpui_component::Root::new(chat, window, cx)
    });
    let chat = holder.borrow().as_ref().expect("chat mounted").clone();
    cx.run_until_parked();
    cx.update(|window, cx| window.draw(cx).clear(cx));

    // The composer-level command enters browsing like the Up arrow.
    let recall = cx.debug_bounds("prompt-recall-btn").expect("recall command");
    cx.simulate_click(recall.center(), gpui::Modifiers::default());
    cx.run_until_parked();
    chat.read_with(cx, |chat, cx| {
        assert_eq!(chat.input_state.read(cx).value().as_ref(), "third prompt");
        assert!(chat.prompt_recall.is_some());
    });
    cx.update(|window, cx| window.draw(cx).clear(cx));
    assert!(
        cx.debug_bounds("prompt-recall-strip").is_some(),
        "browsing shows the text-only status strip"
    );
    let older = cx.debug_bounds("prompt-recall-older").expect("older button");
    cx.simulate_click(older.center(), gpui::Modifiers::default());
    cx.run_until_parked();
    chat.read_with(cx, |chat, cx| {
        assert_eq!(chat.input_state.read(cx).value().as_ref(), "second prompt");
    });
    let newer = cx.debug_bounds("prompt-recall-newer").expect("newer button");
    cx.simulate_click(newer.center(), gpui::Modifiers::default());
    cx.simulate_click(newer.center(), gpui::Modifiers::default());
    cx.run_until_parked();
    chat.read_with(cx, |chat, cx| {
        assert_eq!(chat.input_state.read(cx).value().as_ref(), "");
        assert!(chat.prompt_recall.is_none());
    });
}

#[gpui::test]
fn conversation_outline_focuses_and_jumps_to_prompts(cx: &mut gpui::TestAppContext) {
    use gpui::AppContext as _;
    cx.update(gpui_component::init);
    let model = cx.new(|_| {
        let mut state = threadlane_ui_state::AppState::default();
        state.is_new_task = false;
        state.active_session_id = Some("outline-task".into());
        // Pad the transcript so a jump to the first prompt really leaves the
        // tail — with everything visible, follow-tail re-engages on layout.
        let mut messages = prompt_test_messages();
        for ix in 0..40 {
            messages.push(ChatMessageInfo {
                id: format!("a-pad-{ix}"),
                role: MessageRole::Assistant,
                content: format!("padding answer {ix}"),
                tool_activities: Vec::new(),
                streaming: false,
                reasoning_content: None,
                reasoning_expanded: false,
            });
        }
        state.messages = messages.into();
        state
    });
    let retained_model = model.clone();
    let holder: std::rc::Rc<
        std::cell::RefCell<Option<gpui::Entity<super::ChatListView>>>,
    > = Default::default();
    let holder_clone = holder.clone();
    let (_, cx) = cx.add_window_view(move |window, cx| {
        let chat = cx.new(|cx| super::ChatListView::new(model, window, cx));
        holder_clone.borrow_mut().replace(chat.clone());
        gpui_component::Root::new(chat, window, cx)
    });
    let chat = holder.borrow().as_ref().expect("chat mounted").clone();
    cx.run_until_parked();
    cx.update(|window, cx| window.draw(cx).clear(cx));
    chat.update(cx, |chat, _| chat.initial_scroll_frames = 0);

    let trigger = cx
        .debug_bounds("conversation-outline-open")
        .expect("outline command in the header");
    cx.simulate_click(trigger.center(), gpui::Modifiers::default());
    cx.run_until_parked();
    cx.update(|window, cx| window.draw(cx).clear(cx));
    chat.read_with(cx, |chat, _| {
        assert!(chat.outline_open);
        assert_eq!(chat.outline_landmarks.len(), 3);
        assert_eq!(chat.outline_focus_id.as_deref(), Some("u3"), "newest prompt focused first");
    });

    // Arrows/Home move list focus only; Enter jumps.
    cx.simulate_keystrokes("up");
    chat.read_with(cx, |chat, _| {
        assert_eq!(chat.outline_focus_id.as_deref(), Some("u2"));
        assert!(chat.transcript_list_state.is_following_tail(), "focus alone never scrolls");
    });
    cx.simulate_keystrokes("home");
    chat.read_with(cx, |chat, _| {
        assert_eq!(chat.outline_focus_id.as_deref(), Some("u1"));
    });
    cx.simulate_keystrokes("enter");
    cx.run_until_parked();
    chat.read_with(cx, |chat, _| {
        assert!(!chat.outline_open, "jump closes the outline");
        assert_eq!(chat.outline_selected_id.as_deref(), Some("u1"));
        assert!(!chat.transcript_list_state.is_following_tail(), "the jump pauses tail following");
    });

    // Reopening prefers the last jumped-to prompt when it is still listed.
    cx.simulate_click(trigger.center(), gpui::Modifiers::default());
    cx.run_until_parked();
    chat.read_with(cx, |chat, _| {
        assert_eq!(chat.outline_focus_id.as_deref(), Some("u1"));
    });
    // Escape cancels without jumping.
    cx.simulate_keystrokes("up escape");
    chat.read_with(cx, |chat, _| {
        assert!(!chat.outline_open);
        assert_eq!(chat.outline_selected_id.as_deref(), Some("u1"), "cancel keeps the last jump");
    });

    // A second pointer click on the trigger closes the outline instead of
    // reopening it.
    cx.simulate_click(trigger.center(), gpui::Modifiers::default());
    cx.run_until_parked();
    chat.read_with(cx, |chat, _| assert!(chat.outline_open));
    cx.simulate_click(trigger.center(), gpui::Modifiers::default());
    cx.run_until_parked();
    chat.read_with(cx, |chat, _| {
        assert!(!chat.outline_open, "trigger toggles the outline closed");
    });

    // Opening Find clears the outline selection.
    chat.update_in(cx, |chat, window, cx| {
        chat.open_conversation_find(&super::FindInConversation, window, cx)
    });
    chat.read_with(cx, |chat, _| {
        assert!(chat.find_open);
        assert!(chat.outline_selected_id.is_none());
    });
    chat.update_in(cx, |chat, window, cx| {
        chat.close_conversation_find(&super::CloseConversationFind, window, cx)
    });

    // A session switch clears the popover and the marker.
    chat.update_in(cx, |chat, window, cx| chat.open_conversation_outline(window, cx));
    chat.read_with(cx, |chat, _| assert!(chat.outline_open));
    retained_model.update(cx, |state, cx| {
        state.active_session_id = Some("other-task".into());
        state.messages = prompt_test_messages().into();
        cx.notify();
    });
    cx.run_until_parked();
    chat.read_with(cx, |chat, _| {
        assert!(!chat.outline_open, "session switch closes the outline");
        assert!(chat.outline_selected_id.is_none());
    });
}

fn file_completion_repo(files: &[&str]) -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("temp dir");
    let init = std::process::Command::new("git")
        .args(["init", "--quiet"])
        .current_dir(dir.path())
        .output()
        .expect("git must be on PATH");
    assert!(init.status.success(), "git init failed: {init:?}");
    for file in files {
        let path = dir.path().join(file);
        std::fs::create_dir_all(path.parent().expect("repo-relative file"))
            .expect("parent dir");
        std::fs::write(path, "contents").expect("file write");
    }
    dir
}

fn mount_chat_with_work_dir<'a>(
    cx: &'a mut gpui::TestAppContext,
    work_dir: Option<std::path::PathBuf>,
) -> (
    gpui::Entity<super::ChatListView>,
    gpui::Entity<threadlane_ui_state::AppState>,
    &'a mut gpui::VisualTestContext,
) {
    use gpui::AppContext as _;

    cx.update(gpui_component::init);
    cx.update(super::init);
    let model = cx.new(|_| {
        let mut state = threadlane_ui_state::AppState::default();
        state.active_work_dir = work_dir;
        state.is_new_task = false;
        state
    });
    let retained = model.clone();
    let holder = std::rc::Rc::new(std::cell::RefCell::new(None));
    let capture = holder.clone();
    let (_, cx) = cx.add_window_view(move |window, cx| {
        let chat = cx.new(|cx| super::ChatListView::new(model, window, cx));
        capture.replace(Some(chat.clone()));
        gpui_component::Root::new(chat, window, cx)
    });
    let mounted = holder.borrow().as_ref().expect("mounted chat").clone();
    (mounted, retained, cx)
}

#[gpui::test]
fn composer_at_completion_inserts_code_span_and_preserves_text(
    cx: &mut gpui::TestAppContext,
) {
    let repo = file_completion_repo(&["README.md", "src/main.rs"]);
    let (chat, _model, cx) =
        mount_chat_with_work_dir(cx, Some(repo.path().to_path_buf()));
    cx.run_until_parked();
    cx.update(|window, cx| chat.update(cx, |chat, cx| chat.focus_composer(window, cx)));
    cx.run_until_parked();

    cx.simulate_input("run @");
    cx.run_until_parked();
    cx.update(|window, cx| window.draw(cx).clear(cx));
    chat.read_with(cx, |chat, _| {
        assert!(
            matches!(
                chat.file_completion.as_ref().map(|state| &state.status),
                Some(super::file_completion::FileCompletionStatus::Ready(_))
            ),
            "picker must finish loading the git inventory"
        );
    });
    cx.update(|window, cx| window.draw(cx).clear(cx));
    assert!(cx.debug_bounds("file-completion-list").is_some());

    // Empty query sorts shortest-path-first: README.md wins over src/main.rs.
    cx.simulate_keystrokes("enter");
    cx.run_until_parked();
    chat.read_with(cx, |chat, cx| {
        assert_eq!(
            chat.input_state.read(cx).value().as_ref(),
            "run `README.md` ",
            "only the @ trigger range is replaced by the code span"
        );
        assert!(chat.file_completion.is_none(), "picker closes after insert");
    });
}

#[gpui::test]
fn composer_at_completion_filters_and_never_submits_on_enter(
    cx: &mut gpui::TestAppContext,
) {
    let repo = file_completion_repo(&["src/main.rs", "src/lib.rs", "notes.md"]);
    let (chat, _model, cx) =
        mount_chat_with_work_dir(cx, Some(repo.path().to_path_buf()));
    cx.run_until_parked();
    cx.update(|window, cx| chat.update(cx, |chat, cx| chat.focus_composer(window, cx)));
    cx.run_until_parked();

    // A query matching nothing must still own Enter: submission clears the
    // composer, so an unchanged value proves it never reached Send/Queue.
    cx.simulate_input("@zzz");
    cx.run_until_parked();
    cx.update(|window, cx| window.draw(cx).clear(cx));
    cx.simulate_keystrokes("enter");
    cx.run_until_parked();
    chat.read_with(cx, |chat, cx| {
        assert_eq!(chat.input_state.read(cx).value().as_ref(), "@zzz");
    });

    // Narrowing then accepting mid-list inserts the selected relative path.
    chat.update_in(cx, |chat, _window, cx| {
        chat.input_state.update(cx, |input, cx| {
            input.set_selected_range(0..4, cx);
        });
    });
    cx.simulate_input("open @src/");
    cx.run_until_parked();
    cx.update(|window, cx| window.draw(cx).clear(cx));
    chat.read_with(cx, |chat, _| {
        assert!(matches!(
            chat.file_completion.as_ref().map(|state| &state.status),
            Some(super::file_completion::FileCompletionStatus::Ready(_))
        ));
    });
    cx.simulate_keystrokes("down");
    cx.run_until_parked();
    chat.read_with(cx, |chat, _| {
        assert_eq!(chat.selected_file_index, 1, "Down selects the second row");
    });
    cx.simulate_keystrokes("enter");
    cx.run_until_parked();
    chat.read_with(cx, |chat, cx| {
        assert_eq!(
            chat.input_state.read(cx).value().as_ref(),
            "open `src/main.rs` "
        );
    });
}

#[gpui::test]
fn composer_at_completion_escape_dismisses_and_keeps_query(
    cx: &mut gpui::TestAppContext,
) {
    let repo = file_completion_repo(&["README.md"]);
    let (chat, _model, cx) =
        mount_chat_with_work_dir(cx, Some(repo.path().to_path_buf()));
    cx.run_until_parked();
    cx.update(|window, cx| chat.update(cx, |chat, cx| chat.focus_composer(window, cx)));
    cx.run_until_parked();

    cx.simulate_input("look @RE");
    cx.run_until_parked();
    cx.update(|window, cx| window.draw(cx).clear(cx));
    assert!(cx.debug_bounds("file-completion-list").is_some());

    cx.simulate_keystrokes("escape");
    cx.run_until_parked();
    chat.read_with(cx, |chat, cx| {
        assert!(chat.dismiss_file_menu, "Escape only dismisses the menu");
        assert_eq!(
            chat.input_state.read(cx).value().as_ref(),
            "look @RE",
            "the typed query is preserved"
        );
    });
    cx.update(|window, cx| window.draw(cx).clear(cx));
    assert!(
        cx.debug_bounds("file-completion-list").is_none(),
        "dismissed menu no longer renders"
    );
}

#[gpui::test]
fn composer_at_completion_ignores_interiors_and_noncollapsed_selection(
    cx: &mut gpui::TestAppContext,
) {
    let repo = file_completion_repo(&["README.md"]);
    let (chat, _model, cx) =
        mount_chat_with_work_dir(cx, Some(repo.path().to_path_buf()));
    cx.run_until_parked();
    cx.update(|window, cx| chat.update(cx, |chat, cx| chat.focus_composer(window, cx)));
    cx.run_until_parked();

    for text in ["mail user@exa", "https://x/@y", "keep@tag"] {
        chat.update_in(cx, |chat, window, cx| {
            chat.input_state.update(cx, |input, cx| {
                input.set_value(text, window, cx);
                input.set_selected_range(text.len()..text.len(), cx);
            });
            chat.sync_file_completion(cx);
        });
        cx.run_until_parked();
        chat.read_with(cx, |chat, cx| {
            assert!(
                !chat.file_menu_open(cx),
                "interior @ in {text:?} must not open the picker"
            );
        });
    }

    // A non-collapsed selection suppresses the trigger even inside a valid @.
    chat.update_in(cx, |chat, window, cx| {
        chat.input_state.update(cx, |input, cx| {
            input.set_value("open @RE", window, cx);
            input.set_selected_range(0..8, cx);
        });
        chat.sync_file_completion(cx);
    });
    cx.run_until_parked();
    chat.read_with(cx, |chat, cx| {
        assert!(!chat.file_menu_open(cx), "selection suppresses the trigger");
    });
}

#[gpui::test]
fn composer_at_completion_reports_unsupported_root(cx: &mut gpui::TestAppContext) {
    let dir = tempfile::tempdir().expect("temp dir");
    let (chat, model, cx) =
        mount_chat_with_work_dir(cx, Some(dir.path().to_path_buf()));
    cx.run_until_parked();
    cx.update(|window, cx| chat.update(cx, |chat, cx| chat.focus_composer(window, cx)));
    cx.run_until_parked();

    cx.simulate_input("@");
    cx.run_until_parked();
    chat.read_with(cx, |chat, _| {
        assert!(
            matches!(
                chat.file_completion.as_ref().map(|state| &state.status),
                Some(super::file_completion::FileCompletionStatus::Unsupported(_))
            ),
            "a non-Git root is Unsupported, never an empty match list"
        );
    });

    // No project attached at all gets its own explanation, not "no matches".
    // Detaching the project counts as a session switch and clears the old
    // picker; typing again re-syncs into the new scope.
    model.update(cx, |state, cx| {
        state.active_work_dir = None;
        cx.notify();
    });
    cx.run_until_parked();
    cx.simulate_input("@");
    cx.run_until_parked();
    cx.run_until_parked();
    chat.read_with(cx, |chat, _| {
        let reason = chat.file_completion.as_ref().and_then(|state| {
            match &state.status {
                super::file_completion::FileCompletionStatus::Unsupported(reason) => {
                    Some(reason.clone())
                }
                _ => None,
            }
        });
        assert_eq!(
            reason.as_deref(),
            Some("Attach a project to search files")
        );
    });
}

#[gpui::test]
fn composer_at_completion_deleted_file_refreshes_instead_of_inserting(
    cx: &mut gpui::TestAppContext,
) {
    let repo = file_completion_repo(&["src/gone.rs", "src/kept.rs"]);
    let (chat, _model, cx) =
        mount_chat_with_work_dir(cx, Some(repo.path().to_path_buf()));
    cx.run_until_parked();
    cx.update(|window, cx| chat.update(cx, |chat, cx| chat.focus_composer(window, cx)));
    cx.run_until_parked();

    cx.simulate_input("use @src/");
    cx.run_until_parked();
    cx.update(|window, cx| window.draw(cx).clear(cx));
    // The file disappears between enumeration and Enter: apply must refresh
    // the inventory and keep the draft, not insert a stale path.
    std::fs::remove_file(repo.path().join("src/gone.rs")).expect("delete");
    cx.simulate_keystrokes("enter");
    cx.run_until_parked();
    chat.read_with(cx, |chat, cx| {
        assert_eq!(chat.input_state.read(cx).value().as_ref(), "use @src/");
        assert!(matches!(
            chat.file_completion.as_ref().map(|state| &state.status),
            Some(super::file_completion::FileCompletionStatus::Ready(_))
        ));
    });
}

#[gpui::test]
fn composer_at_completion_resyncs_when_root_changes(cx: &mut gpui::TestAppContext) {
    let plain = tempfile::tempdir().expect("non-git dir");
    let repo = file_completion_repo(&["src/ready.rs"]);
    let (chat, model, cx) =
        mount_chat_with_work_dir(cx, Some(plain.path().to_path_buf()));
    cx.run_until_parked();
    cx.update(|window, cx| chat.update(cx, |chat, cx| chat.focus_composer(window, cx)));
    cx.run_until_parked();

    cx.simulate_input("@ready");
    cx.run_until_parked();
    chat.read_with(cx, |chat, _| {
        assert!(
            matches!(
                chat.file_completion.as_ref().map(|state| &state.status),
                Some(super::file_completion::FileCompletionStatus::Unsupported(_))
            ),
            "non-git root reports Unsupported"
        );
    });

    // The root changes without a composer edit (worktree finished
    // preparing): the next rendered frame must resync rather than leave a
    // stale Unsupported state swallowing Enter.
    model.update(cx, |state, _| {
        state.active_work_dir = Some(repo.path().to_path_buf());
    });
    cx.run_until_parked();
    cx.update(|window, cx| window.draw(cx).clear(cx));
    cx.run_until_parked();
    chat.read_with(cx, |chat, cx| {
        match chat.file_completion.as_ref().map(|state| &state.status) {
            Some(super::file_completion::FileCompletionStatus::Ready(inventory)) => {
                assert!(inventory.paths.contains(&"src/ready.rs".to_string()));
            }
            _ => panic!("expected Ready after root change"),
        }
        assert!(chat.file_menu_open(cx), "picker stays open across resync");
    });
}
