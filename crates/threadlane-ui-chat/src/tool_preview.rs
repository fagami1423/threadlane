//! Read-only tool previews. Tool transcripts remain unchanged.
use gpui::*;
use gpui_component::button::{Button, ButtonVariants};
use gpui_component::scroll::ScrollableElement;
use gpui_component::{ActiveTheme, Disableable, Icon, IconName, Sizable};
use threadlane_ui_state::{actions::AppAction, controller, AppState, ToolActivityInfo};

use super::markdown::{classify_chat_link, ChatLinkTarget};
use super::tool_detail::{
    args_json, args_path, card_container, card_header, highlighted_code, preview_viewport,
};

fn read_line(line: &str) -> Option<(usize, &str)> {
    let (anchor, text) = line.split_once('|')?;
    let (number, hash) = anchor.split_once(':')?;
    if hash.len() != 3 || !hash.bytes().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    let number = number.parse::<usize>().ok().filter(|n| *n > 0)?;
    Some((number, text))
}

fn search_line(line: &str) -> Option<(&str, usize, &str)> {
    // Find the numeric separator; file names and match text may contain colons.
    line.match_indices(':').find_map(|(ix, _)| {
        let (number, text) = line[ix + 1..].split_once(':')?;
        let number = number.parse::<usize>().ok().filter(|n| *n > 0)?;
        let path = &line[..ix];
        (!path.is_empty()).then_some((path, number, text))
    })
}

fn match_highlights(
    text: &str,
    pattern: &str,
    color: Hsla,
) -> Vec<(std::ops::Range<usize>, HighlightStyle)> {
    if pattern.is_empty() {
        return Vec::new();
    }
    text.match_indices(pattern)
        .map(|(start, value)| {
            (
                start..start + value.len(),
                HighlightStyle {
                    background_color: Some(color.opacity(0.2)),
                    ..Default::default()
                },
            )
        })
        .collect()
}

fn open_button(
    id: String,
    path: String,
    line: Option<usize>,
    folder: bool,
    model: &Entity<AppState>,
) -> Button {
    let target = match classify_chat_link(&path) {
        ChatLinkTarget::ProjectFile(path) if !path.contains(":/") => Some(path),
        _ => None,
    };
    let label = if folder {
        format!("Reveal folder {path}")
    } else if let Some(line) = line {
        format!("Open {path} at line {line}")
    } else {
        format!("Open {path} in editor")
    };
    let model = model.clone();
    Button::new(SharedString::from(id))
        .ghost()
        .xsmall()
        .accessibility_label(label.clone())
        .tooltip(label)
        .disabled(target.is_none())
        .on_click(move |_, _, cx| {
            let Some(path) = target.clone() else {
                return;
            };
            if folder {
                let Some(root) = model.read(cx).active_git_work_dir() else {
                    return;
                };
                if let Ok(path) = threadlane_tools::validate_path_in_workspace(&path, &root) {
                    cx.reveal_path(&path);
                }
            } else {
                model.update(cx, |state, cx| {
                    let action = match line {
                        Some(line) => AppAction::OpenFileInEditorAtLine { path, line },
                        None => AppAction::OpenFileInEditor(path),
                    };
                    controller::dispatch(state, action);
                    cx.notify();
                });
            }
        })
}

pub(crate) fn render(
    activity: &ToolActivityInfo,
    model: &Entity<AppState>,
    cx: &mut App,
) -> Option<AnyElement> {
    let tool = activity.title.trim().to_lowercase().replace(' ', "_");
    if !matches!(tool.as_str(), "read_file" | "grep_search" | "list_dir") {
        return None;
    }
    let theme = cx.theme().colors;
    let args = args_json(&activity.arguments).unwrap_or_default();
    let path = threadlane_tools::read_file_snapshot_path(&activity.detail)
        .or_else(|| args_path(&args))
        .unwrap_or_else(|| ".".into());
    let pending = activity.category == "Working"
        && (activity.detail.trim().is_empty()
            || activity.detail.trim() == activity.arguments.trim());
    let mut header = card_header(&theme).child(
        Icon::new(match tool.as_str() {
            "grep_search" => IconName::Search,
            "list_dir" => IconName::Folder,
            _ => IconName::File,
        })
        .xsmall(),
    );
    let title = if tool == "grep_search" {
        format!(
            "Search · {}",
            args.get("pattern").and_then(|v| v.as_str()).unwrap_or("")
        )
    } else {
        path.clone()
    };
    header = header.child(
        div()
            .id(SharedString::from(format!("preview-title-{}", activity.id)))
            .min_w_0()
            .flex_1()
            .truncate()
            .text_sm()
            .tooltip({
                let title = title.clone();
                move |window, cx| {
                    gpui_component::tooltip::Tooltip::new(title.clone()).build(window, cx)
                }
            })
            .child(title),
    );
    let mut rows = Vec::new();
    if pending {
        rows.push(
            div()
                .text_color(theme.muted_foreground)
                .child("Loading…")
                .into_any_element(),
        );
    } else if activity.category == "Error" {
        rows.push(
            div()
                .text_color(theme.danger)
                .child(activity.detail.clone())
                .into_any_element(),
        );
    } else if tool == "read_file" {
        let source = activity
            .detail
            .lines()
            .filter_map(read_line)
            .collect::<Vec<_>>();
        if source.is_empty() {
            rows.push(div().child(activity.detail.clone()).into_any_element());
        } else {
            let first = source[0].0;
            let last = source.last().unwrap().0;
            header = header
                .child(
                    div()
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .child(format!("{first}–{last}")),
                )
                .child(
                    open_button(
                        format!("read-open-{}", activity.id),
                        path.clone(),
                        Some(first),
                        false,
                        model,
                    )
                    .debug_selector(|| "tool-preview-open".into())
                    .icon(IconName::ExternalLink)
                    .label("Open in editor"),
                );
            let numbers = source
                .iter()
                .map(|(no, _)| no.to_string())
                .collect::<Vec<_>>()
                .join("\n");
            let code = source
                .iter()
                .map(|(_, text)| *text)
                .collect::<Vec<_>>()
                .join("\n");
            rows.push(
                div()
                    .flex()
                    .items_start()
                    .gap_3()
                    .child(
                        div()
                            .flex_none()
                            .text_right()
                            .text_color(theme.muted_foreground)
                            .child(numbers),
                    )
                    .child(div().child(highlighted_code(
                        code,
                        threadlane_ui_editor::detect_language(&path),
                        cx,
                    )))
                    .into_any_element(),
            );
            // Keep continuation and recovery notices visible, but omit snapshot metadata.
            rows.extend(
                activity
                    .detail
                    .lines()
                    .filter(|line| {
                        read_line(line).is_none() && !line.starts_with("[Threadlane read_file ")
                    })
                    .map(|line| {
                        div()
                            .text_color(theme.muted_foreground)
                            .child(line.to_string())
                            .into_any_element()
                    }),
            );
        }
    } else if tool == "grep_search" {
        let pattern = args.get("pattern").and_then(|v| v.as_str()).unwrap_or("");
        let mut previous_path = "";
        for line in activity.detail.lines() {
            let Some((path, number, text)) = search_line(line) else {
                rows.push(
                    div()
                        .text_color(theme.muted_foreground)
                        .child(line.to_string())
                        .into_any_element(),
                );
                continue;
            };
            if path != previous_path {
                rows.push(
                    div()
                        .mt_1()
                        .text_color(theme.muted_foreground)
                        .child(path.to_string())
                        .into_any_element(),
                );
                previous_path = path;
            }
            let selector = format!("tool-preview-search-match-{path}-{number}");
            rows.push(
                open_button(
                    format!("search-{}-{path}-{number}", activity.id),
                    path.into(),
                    Some(number),
                    false,
                    model,
                )
                .debug_selector(move || selector.clone())
                .justify_start()
                .gap_2()
                .child(
                    div()
                        .flex_none()
                        .text_color(theme.muted_foreground)
                        .child(number.to_string()),
                )
                .child(
                    StyledText::new(text.to_string()).with_highlights(match_highlights(
                        text,
                        pattern,
                        theme.primary,
                    )),
                )
                .into_any_element(),
            );
        }
    } else {
        let entry_base = if std::path::Path::new(&path).is_absolute() {
            model
                .read(cx)
                .active_git_work_dir()
                .and_then(|root| {
                    let validated = threadlane_tools::validate_path_in_workspace(&path, &root).ok()?;
                    let canonical_root = root.canonicalize().ok()?;
                    validated
                        .strip_prefix(canonical_root)
                        .ok()
                        .map(std::path::PathBuf::from)
                })
                .unwrap_or_else(|| std::path::PathBuf::from(&path))
        } else {
            std::path::PathBuf::from(&path)
        };
        for line in activity.detail.lines() {
            let entry = line
                .strip_prefix("[DIR]  ")
                .map(|name| (name, true))
                .or_else(|| line.strip_prefix("[FILE] ").map(|name| (name, false)));
            let Some((name, folder)) = entry else {
                rows.push(
                    div()
                        .text_color(theme.muted_foreground)
                        .child(line.to_string())
                        .into_any_element(),
                );
                continue;
            };
            let entry_path = entry_base
                .join(name)
                .to_string_lossy()
                .into_owned();
            let selector = format!("tool-preview-directory-entry-{name}");
            rows.push(
                open_button(
                    format!("directory-{}-{entry_path}", activity.id),
                    entry_path,
                    None,
                    folder,
                    model,
                )
                .debug_selector(move || selector.clone())
                .icon(if folder {
                    IconName::Folder
                } else {
                    IconName::File
                })
                .label(name.to_string())
                .justify_start()
                .into_any_element(),
            );
        }
        if rows.is_empty() {
            rows.push(
                div()
                    .text_color(theme.muted_foreground)
                    .child("Empty directory")
                    .into_any_element(),
            );
        }
    }
    Some(
        card_container(&theme)
            .debug_selector(|| "tool-preview-card".into())
            .child(header)
            .child(
                preview_viewport(format!("tool-preview-{}", activity.id))
                    .debug_selector(|| "tool-preview-viewport".into())
                    .child(
                        div()
                            .h_full()
                            .min_h_0()
                            .text_color(theme.foreground)
                            .font_family("monospace")
                            .text_xs()
                            .overflow_y_scrollbar()
                            .id(SharedString::from(format!(
                                "tool-preview-scroll-{}",
                                activity.id
                            )))
                            .child(
                                div()
                                    .debug_selector(|| "tool-preview-content".into())
                                    .p_2()
                                    .flex()
                                    .flex_col()
                                    .items_start()
                                    .gap_1()
                                    .children(rows),
                            ),
                    ),
            )
            .into_any_element(),
    )
}

#[cfg(test)]
mod tests {
    use super::{match_highlights, read_line, search_line};
    #[test]
    fn native_tool_rows_preserve_source_and_match_boundaries() {
        assert_eq!(
            read_line("12:a3f|  let x = \"é\";"),
            Some((12, "  let x = \"é\";"))
        );
        assert_eq!(read_line("[Continue reading at start_line: 13]"), None);
        assert_eq!(read_line("0:a3f|invalid"), None);
        assert_eq!(
            search_line("src/a:b.rs:12:é: needle"),
            Some(("src/a:b.rs", 12, "é: needle"))
        );
        assert_eq!(search_line("No matches found."), None);
        let ranges = match_highlights("é needle needle", "needle", gpui::Hsla::default());
        assert_eq!(
            ranges
                .iter()
                .map(|(range, _)| range.clone())
                .collect::<Vec<_>>(),
            vec![3..9, 10..16]
        );
        assert!(match_highlights("text", "", gpui::Hsla::default()).is_empty());
    }
    #[gpui::test]
    fn previews_show_results_trap_scroll_and_open_files(cx: &mut gpui::TestAppContext) {
        use gpui::AppContext as _;
        use gpui_component::Root;
        use threadlane_ui_state::{AppState, RequestedEditorTarget, ToolActivityInfo};
        struct Harness {
            model: gpui::Entity<AppState>,
            activity: ToolActivityInfo,
            outer_scrolls: std::rc::Rc<std::cell::Cell<usize>>,
        }
        impl gpui::Render for Harness {
            fn render(
                &mut self,
                _: &mut gpui::Window,
                cx: &mut gpui::Context<Self>,
            ) -> impl gpui::IntoElement {
                use gpui::{InteractiveElement as _, ParentElement as _, Styled as _};
                let outer_scrolls = self.outer_scrolls.clone();
                gpui::div()
                    .id("outer-scroll")
                    .size_full()
                    .on_scroll_wheel(move |_, _, _| {
                        outer_scrolls.set(outer_scrolls.get() + 1);
                    })
                    .child(super::render(&self.activity, &self.model, cx).unwrap())
            }
        }
        cx.update(gpui_component::init);
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("sample.rs"), "fn sample() {}\n".repeat(80)).unwrap();
        let model = cx.new(|_| {
            let mut state = AppState::default();
            state.active_work_dir = Some(dir.path().to_path_buf());
            state.active_session_id = None;
            state.is_new_task = false;
            state
        });
        let retained_model = model.clone();
        let outer_scrolls = std::rc::Rc::new(std::cell::Cell::new(0));
        let retained_scrolls = outer_scrolls.clone();
        let holder = std::rc::Rc::new(std::cell::RefCell::new(None));
        let holder_clone = holder.clone();
        let activity = ToolActivityInfo {
            id: "preview".into(),
            title: "read_file".into(),
            category: "Completed".into(),
            display_summary: "Read sample.rs".into(),
            arguments: r#"{"path":"sample.rs"}"#.into(),
            detail: (10..80)
                .map(|no| format!("{no}:a3f|fn sample() {{}}\n"))
                .collect(),
            is_expanded: false,
        };
        let (_, cx) = cx.add_window_view(move |window, cx| {
            let harness = cx.new(|_| Harness {
                model,
                activity,
                outer_scrolls,
            });
            holder_clone.borrow_mut().replace(harness.clone());
            Root::new(harness, window, cx)
        });
        let harness = holder.borrow().as_ref().unwrap().clone();
        for (tool, selector, expected_line, absolute_listing) in [
            ("read_file", "tool-preview-open", Some(10), false),
            (
                "grep_search",
                "tool-preview-search-match-sample.rs-10",
                Some(10),
                false,
            ),
            ("list_dir", "tool-preview-directory-entry-sample.rs", None, false),
            ("list_dir", "tool-preview-directory-entry-sample.rs", None, true),
        ] {
            retained_model.update(cx, |model, _| model.requested_editor_target = None);
            if tool != "read_file" {
                harness.update(cx, |harness, cx| {
                    harness.activity.title = tool.into();
                    harness.activity.arguments = r#"{"path":".","pattern":"sample"}"#.into();
                    if absolute_listing {
                        harness.activity.arguments =
                            serde_json::json!({"path": dir.path()}).to_string();
                    }
                    harness.activity.detail = if tool == "grep_search" {
                        (10..80)
                            .map(|no| format!("sample.rs:{no}:fn sample() {{}}\n"))
                            .collect()
                    } else {
                        "[FILE] sample.rs\n".to_string()
                            + &(0..70)
                                .map(|ix| format!("[FILE] another-{ix}.rs\n"))
                                .collect::<String>()
                    };
                    cx.notify();
                });
            }
            cx.run_until_parked();
            cx.update(|window, cx| window.draw(cx).clear(cx));
            let viewport = cx
                .debug_bounds("tool-preview-viewport")
                .expect("preview is visible without disclosure");
            assert!(viewport.size.height <= gpui::px(96.));
            let button = cx.debug_bounds(selector).expect("file action is visible");
            retained_model.read_with(cx, |model, _| {
                assert!(
                    model.active_git_work_dir().is_some(),
                    "active checkout missing"
                )
            });
            cx.simulate_click(button.center(), gpui::Modifiers::default());
            cx.run_until_parked();
            retained_model.read_with(cx, |model, _| {
                assert_eq!(
                    model.requested_editor_target,
                    Some(RequestedEditorTarget::File {
                        project: dir.path().to_path_buf(),
                        path: "sample.rs".into(),
                        line: expected_line,
                    }),
                    "tool {tool}, button {button:?}, status {:?}",
                    model.session_status
                )
            });
            let before = cx.debug_bounds("tool-preview-content").unwrap();
            for delta in [-40., -10000., -40., 10000., 40.] {
                cx.simulate_event(gpui::ScrollWheelEvent {
                    position: viewport.center(),
                    delta: gpui::ScrollDelta::Pixels(gpui::point(gpui::px(0.), gpui::px(delta))),
                    ..Default::default()
                });
                cx.run_until_parked();
                cx.update(|window, cx| window.draw(cx).clear(cx));
                if delta == -40. {
                    assert!(
                        cx.debug_bounds("tool-preview-content").unwrap().origin.y < before.origin.y,
                        "tool {tool} must scroll its content: before {before:?}, after {:?}",
                        cx.debug_bounds("tool-preview-content")
                    );
                }
            }
            assert_eq!(
                retained_scrolls.get(),
                0,
                "wheel events stay inside every preview, including at edges"
            );
        }
        retained_model.update(cx, |model, _| model.requested_editor_target = None);
        harness.update(cx, |harness, cx| {
            harness.activity.arguments =
                serde_json::json!({"path": dir.path().parent().unwrap()}).to_string();
            harness.activity.detail = "[FILE] sample.rs\n".into();
            cx.notify();
        });
        cx.run_until_parked();
        cx.update(|window, cx| window.draw(cx).clear(cx));
        let button = cx
            .debug_bounds("tool-preview-directory-entry-sample.rs")
            .unwrap();
        cx.simulate_click(button.center(), gpui::Modifiers::default());
        retained_model.read_with(cx, |model, _| {
            assert!(
                model.requested_editor_target.is_none(),
                "outside-workspace targets stay disabled"
            );
        });
    }
}
