//! Automation navigation, editor, and paged run history. Execution stays in the owning daemon.
mod editor;
use gpui::prelude::FluentBuilder;
use gpui::*;
use gpui_component::button::{Button, ButtonVariants};
use gpui_component::collapsible::Collapsible;
use gpui_component::menu::{DropdownMenu, PopupMenuItem};
use gpui_component::scroll::ScrollableElement;
use gpui_component::{ActiveTheme, Disableable, Selectable, Sizable, StyledExt, WindowExt};
use std::path::PathBuf;
use threadlane_automation::{display_time, Definition, RunStatus};
use threadlane_protocol::automation::AutomationCommand as Command;
use threadlane_ui_state::{automation_io, AppState};

actions!(threadlane_automation_ui, [SaveAutomation]);
pub fn init(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new("cmd-enter", SaveAutomation, Some("AutomationEditor")),
        KeyBinding::new("ctrl-enter", SaveAutomation, Some("AutomationEditor")),
    ]);
}

pub(crate) fn picker(
    id: &'static str,
    label: String,
    choices: Vec<(String, String)>,
    selected: String,
    disabled: bool,
    on_select: impl Fn(String, &mut App) + 'static,
) -> impl IntoElement {
    let on_select = std::rc::Rc::new(on_select);
    Button::new(id)
        .label(label.clone())
        .accessibility_label(label)
        .dropdown_caret(true)
        .disabled(disabled)
        .dropdown_menu_with_anchor(Anchor::BottomLeft, move |menu, window, _| {
            choices.iter().fold(
                menu.max_h(window.rem_size() * 20.0).scrollable(true),
                |menu, (id, label)| {
                    let id = id.clone();
                    let callback = on_select.clone();
                    menu.item(
                        PopupMenuItem::new(label.clone())
                            .checked(id == selected)
                            .on_click(move |_, _, cx| callback(id.clone(), cx)),
                    )
                },
            )
        })
}

pub struct AutomationsView {
    model: Entity<AppState>,
    selected: Option<String>,
    expanded_prompt: Option<String>,
    scope: Option<PathBuf>,
    history: bool,
    page: usize,
    error: Option<String>,
    busy: bool,
    _subscription: Subscription,
}

/// A completed delete may reset navigation only while its definition remains selected.
fn deleted_selection_is_current(deleted_id: Option<&str>, selected: Option<&str>) -> bool {
    deleted_id.is_some() && deleted_id == selected
}

impl AutomationsView {
    pub fn new(model: Entity<AppState>, cx: &mut Context<Self>) -> Self {
        let subscription = cx.observe(&model, |_, _, cx| cx.notify());
        Self {
            model,
            selected: None,
            expanded_prompt: None,
            scope: None,
            history: false,
            page: 0,
            error: None,
            busy: false,
            _subscription: subscription,
        }
    }
    /// Dispatch one daemon mutation, retaining selection on failure and showing its error.
    /// Store projections arrive through the model's authoritative watch or event stream.
    fn command(&mut self, command: Command, cx: &mut Context<Self>) {
        if self.busy {
            return;
        }
        let client = self.model.read(cx).daemon_client.clone();
        self.busy = true;
        self.error = None;
        let deleted_id = match &command {
            Command::Delete { id } => Some(id.clone()),
            _ => None,
        };
        let task = threadlane_provider::exec::get_runtime()
            .spawn(async move { automation_io::mutate(&client, command).await });
        cx.spawn(async move |this, cx| {
            let result = task
                .await
                .map_err(|error| format!("Automation request failed: {error}"))
                .and_then(|result| result);
            let _ = this.update(cx, |this, cx| {
                this.busy = false;
                if result.is_ok()
                    && deleted_selection_is_current(
                        deleted_id.as_deref(),
                        this.selected.as_deref(),
                    )
                {
                    this.selected = None;
                    this.history = true;
                    this.page = 0;
                }
                this.error = result.err();
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }
    fn edit(
        &mut self,
        definition: Option<Definition>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        editor::open(self.model.clone(), definition, window, cx);
    }
}
impl Render for AutomationsView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let state = self.model.read(cx);
        let snapshot = state.automations.snapshot.clone();
        let error = self
            .error
            .clone()
            .or_else(|| state.automations.error.clone());
        let empty_projects = state.projects.is_empty();
        let selected = self.selected.clone();
        let scope = self.scope.clone();
        let mut choices = vec![(String::new(), "All projects".into())];
        choices.extend(
            state
                .projects
                .iter()
                .map(|p| (p.work_dir.to_string_lossy().into_owned(), p.name.clone())),
        );
        let scope_label = choices
            .iter()
            .find(|(id, _)| {
                *id == scope
                    .as_ref()
                    .map(|p| p.to_string_lossy().into_owned())
                    .unwrap_or_default()
            })
            .map(|(_, name)| name.clone())
            .unwrap_or_else(|| "Project unavailable".into());
        let owner = cx.entity().downgrade();
        let scope_picker = picker(
            "automation-project-scope",
            scope_label,
            choices,
            scope
                .as_ref()
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_default(),
            false,
            move |id, cx| {
                let _ = owner.update(cx, |this, cx| {
                    this.scope = (!id.is_empty()).then(|| PathBuf::from(id));
                    this.selected = None;
                    this.page = 0;
                    cx.notify();
                });
            },
        );
        let definition = snapshot
            .definitions
            .iter()
            .find(|d| Some(&d.id) == selected.as_ref())
            .cloned();
        let muted = cx.theme().muted_foreground;
        let mut content = div().flex().flex_col().gap_4().p_4();
        if let Some(error) = error {
            content = content.child(div().text_color(cx.theme().danger).child(error));
        }
        if let Some(d) = definition {
            let active = snapshot
                .runs
                .iter()
                .any(|r| r.definition.id == d.id && r.status.active());
            let run_id = d.id.clone();
            let pause_id = d.id.clone();
            let delete_id = d.id.clone();
            let edit = d.clone();
            let owner = cx.entity().downgrade();
            let enabled = d.enabled;
            let prompt_open = self.expanded_prompt.as_ref() == Some(&d.id);
            let prompt_id = d.id.clone();
            content = content.child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(
                        Button::new("automation-back")
                            .ghost()
                            .label("All automations")
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.selected = None;
                                this.page = 0;
                                cx.notify();
                            })),
                    )
                    .child(div().flex_1())
                    .child(
                        Button::new("automation-run")
                            .label("Run now")
                            .disabled(self.busy || active)
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.command(Command::RunNow { id: run_id.clone() }, cx)
                            })),
                    )
                    .child(
                        Button::new("automation-pause")
                            .label(if enabled { "Pause" } else { "Resume" })
                            .disabled(self.busy)
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.command(
                                    Command::SetEnabled { id: pause_id.clone(), enabled: !enabled },
                                    cx,
                                )
                            })),
                    )
                    .child(
                        Button::new("automation-edit")
                            .label("Edit…")
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.edit(Some(edit.clone()), window, cx)
                            })),
                    )
                    .child(
                        Button::new("automation-more")
                            .label("More")
                            .dropdown_caret(true)
                            .dropdown_menu(move |menu, _, _| {
                                let owner = owner.clone();
                                let id = delete_id.clone();
                                menu.item(
                                    PopupMenuItem::new("Delete automation")
                                        .disabled(active)
                                        .on_click(move |_, _, cx| {
                                            let _ = owner.update(cx, |this, cx| {
                                                this.command(Command::Delete { id: id.clone() }, cx)
                                            });
                                        }),
                                )
                            }),
                    ),
            );
            content = content
                .child(div().text_xl().font_semibold().child(d.name.clone()))
                .child(div().text_color(muted).child(format!(
                    "{} · {}",
                    if d.enabled { "Scheduled" } else { "Paused" },
                    d.schedule.label()
                )))
                .child(div().text_color(muted).child(format!(
                    "{} · {} · {}",
                    d.project.display(),
                    d.model,
                    if d.worktree { "Worktree" } else { "Local" }
                )))
                .child(
                    Collapsible::new()
                        .open(prompt_open)
                        .child(
                            Button::new("automation-prompt-toggle")
                                .debug_selector(|| "automation-prompt-toggle".into())
                                .ghost()
                                .small()
                                .label(if prompt_open {
                                    "Hide prompt"
                                } else {
                                    "Show prompt"
                                })
                                .accessibility_label(if prompt_open {
                                    "Hide automation prompt"
                                } else {
                                    "Show automation prompt"
                                })
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.expanded_prompt =
                                        (!prompt_open).then(|| prompt_id.clone());
                                    cx.notify();
                                })),
                        )
                        .content(
                            div()
                                .debug_selector(|| "automation-prompt-body".into())
                                .text_sm()
                                .p_3()
                                .bg(cx.theme().muted)
                                .rounded_md()
                                .child(d.prompt.clone()),
                        ),
                )
                .children(
                    d.paused_reason
                        .clone()
                        .map(|reason| div().text_color(cx.theme().warning).child(reason)),
                )
                .children(d.next_at.map(|at| {
                    div().child(format!(
                        "Next run: {}",
                        display_time(at, d.schedule.timezone())
                    ))
                }));
        } else if !self.history {
            let definitions: Vec<_> = snapshot
                .definitions
                .iter()
                .filter(|d| scope.as_ref().is_none_or(|p| *p == d.project))
                .collect();
            if definitions.is_empty() {
                content = content.child(if empty_projects {
                    "Attach a project from the sidebar to create an automation."
                } else {
                    "Schedule a prompt to run in a fresh chat. Choose New automation… to begin."
                });
            }
            for d in definitions {
                let id = d.id.clone();
                let latest = snapshot.runs.iter().rev().find(|r| r.definition.id == d.id);
                content = content.child(
                    Button::new(SharedString::from(format!("automation-{}", d.id)))
                        .ghost()
                        .w_full()
                        .h_auto()
                        .py_3()
                        .justify_start()
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .gap_3()
                                .w_full()
                                .child(
                                    div()
                                        .flex()
                                        .flex_col()
                                        .flex_1()
                                        .min_w_0()
                                        .items_start()
                                        .gap_1()
                                        .child(div().font_semibold().child(d.name.clone()))
                                        .child(div().text_sm().text_color(muted).child(format!(
                                                    "{} · {} · {}",
                                                    d.project
                                                        .file_name()
                                                        .unwrap_or_default()
                                                        .to_string_lossy(),
                                                    d.schedule.label(),
                                                    if d.worktree { "Worktree" } else { "Local" }
                                                ))),
                                )
                                .child(div().text_sm().child(if !d.enabled {
                                    "Paused".into()
                                } else {
                                    d.next_at
                                        .map(|t| display_time(t, d.schedule.timezone()))
                                        .unwrap_or_else(|| "Manual".into())
                                }))
                                .child(
                                    div().text_sm().child(
                                        latest.map(|r| r.status.label()).unwrap_or("No runs"),
                                    ),
                                ),
                        )
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.selected = Some(id.clone());
                            this.page = 0;
                            cx.notify();
                        })),
                );
            }
        }
        if self.history || selected.is_some() {
            let runs: Vec<_> = snapshot
                .runs
                .iter()
                .rev()
                .filter(|r| {
                    scope.as_ref().is_none_or(|p| *p == r.definition.project)
                        && selected.as_ref().is_none_or(|id| *id == r.definition.id)
                })
                .collect();
            content = content.child(
                div()
                    .font_semibold()
                    .child(format!("Run history · {}", runs.len())),
            );
            let page = self.page.min(runs.len().saturating_sub(1) / 25);
            if runs.is_empty() {
                content = content.child(div().text_color(muted).child("No runs yet"));
            }
            for run in runs.iter().skip(page * 25).take(25) {
                let id = run.id.clone();
                let cancel_id = id.clone();
                let review_id = id.clone();
                let delete_id = id.clone();
                let mut row = div()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .py_3()
                    .border_b_1()
                    .border_color(cx.theme().border)
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_3()
                            .flex_wrap()
                            .child(div().flex_1().min_w_0().child(format!(
                                "{} · {}",
                                run.definition.name,
                                display_time(run.created_at, run.definition.schedule.timezone())
                            )))
                            .child(run.status.label())
                            .child(
                                Button::new(SharedString::from(format!("open-{id}")))
                                    .small()
                                    .label("Open chat")
                                    .disabled(run.session_file.is_none())
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        let result = this.model.update(cx, |state, cx| {
                                            let result = state.open_automation_run(&id);
                                            cx.notify();
                                            result
                                        });
                                        match result {
                                            Ok(()) => {
                                                this.command(Command::Review { id: id.clone() }, cx)
                                            }
                                            Err(error) => {
                                                this.error = Some(error);
                                                cx.notify();
                                            }
                                        }
                                    })),
                            )
                            .when(run.status.active(), |row| {
                                row.child(
                                    Button::new(SharedString::from(format!("cancel-{cancel_id}")))
                                        .small()
                                        .label("Cancel run")
                                        .disabled(self.busy)
                                        .on_click(cx.listener(move |this, _, _, cx| {
                                            this.command(Command::Cancel { id: cancel_id.clone() }, cx)
                                        })),
                                )
                            })
                            .when(run.needs_attention() && !run.status.active(), |row| {
                                row.child(
                                    Button::new(SharedString::from(format!("review-{review_id}")))
                                        .small()
                                        .label("Mark reviewed")
                                        .disabled(self.busy)
                                        .on_click(cx.listener(move |this, _, _, cx| {
                                            this.command(Command::Review { id: review_id.clone() }, cx)
                                        })),
                                )
                            })
                            .when(!run.status.active(), |row| {
                                row.child(
                                    Button::new(SharedString::from(format!("remove-{delete_id}")))
                                        .debug_selector(move || format!("remove-run-{delete_id}"))
                                        .small()
                                        .ghost()
                                        .label("Remove")
                                        .tooltip(
                                            "Remove from run history; keep the chat and worktree",
                                        )
                                        .accessibility_label(
                                            "Remove from run history; keep the chat and worktree",
                                        )
                                        .disabled(self.busy)
                                        .on_click({
                                            let id = run.id.clone();
                                            cx.listener(move |this, _, _, cx| {
                                                this.command(Command::DeleteRun { id: id.clone() }, cx)
                                            })
                                        }),
                                )
                            }),
                    )
                    .child(div().text_xs().text_color(muted).child(format!("{} · {}",
                        run.definition.project.file_name().unwrap_or_default().to_string_lossy(),
                        if run.definition.worktree { "Worktree" } else { "Local" })));
                if let Some(error) = &run.error {
                    row = row.child(
                        div()
                            .text_sm()
                            .text_color(cx.theme().danger)
                            .child(error.clone()),
                    );
                }
                if run.status == RunStatus::Queued {
                    row =
                        row.child(div().text_sm().text_color(muted).child(
                            "Waiting for the current automation run to finish or be cancelled",
                        ));
                }
                content = content.child(row);
            }
            content = content.child(
                div()
                    .flex()
                    .gap_2()
                    .child(
                        Button::new("automation-prev")
                            .label("Previous")
                            .disabled(page == 0)
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.page = page.saturating_sub(1);
                                cx.notify();
                            })),
                    )
                    .child(
                        Button::new("automation-next")
                            .label("Next")
                            .disabled((page + 1) * 25 >= runs.len())
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.page = page + 1;
                                cx.notify();
                            })),
                    ),
            );
        }
        div().flex().flex_col().size_full().bg(cx.theme().background)
            .child(div().flex().items_center().gap_3().px_4().pb_3().pt(threadlane_ui_theme::theme::WINDOW_CONTROLS_CLEARANCE)
                .child(div().text_lg().font_semibold().child("Automations"))
                .child(scope_picker).child(div().flex_1())
                .child(Button::new("automation-history").label(if self.history { "Definitions" } else { "Run history" }).ghost().selected(self.history)
                    .on_click(cx.listener(|this, _, _, cx| { this.history = !this.history; this.selected = None; this.page = 0; cx.notify(); })))
                .child(Button::new("new-automation").debug_selector(|| "new-automation".into()).label("New automation…").disabled(empty_projects || self.busy)
                    .on_click(cx.listener(|this, _, window, cx| this.edit(None, window, cx)))))
            .child(div().px_4().pb_3().text_sm().text_color(muted).child("Runs while Threadlane is open and your computer is awake. Missed runs are combined into one."))
            .child(div().flex_1().min_h_0().overflow_y_scrollbar().child(content))
    }
}

#[cfg(test)]
mod tests {
    use gpui::{AppContext, Modifiers, TestAppContext};
    use gpui_component::WindowExt;
    use threadlane_ui_state::{activate_test_session, AppState};

    /// Remote deletion must not undo a newer selection or a return to the list.
    #[test]
    fn delayed_delete_preserves_newer_navigation() {
        assert!(super::deleted_selection_is_current(Some("a"), Some("a")));
        assert!(!super::deleted_selection_is_current(Some("a"), Some("b")));
        assert!(!super::deleted_selection_is_current(Some("a"), None));
        assert!(!super::deleted_selection_is_current(None, Some("a")));
        assert!(!super::deleted_selection_is_current(None, None));
    }

    #[gpui::test]
    fn history_keeps_long_prompts_collapsed_and_only_finished_runs_removable(
        cx: &mut TestAppContext,
    ) {
        use threadlane_automation::{Definition, Run, RunStatus, Schedule};
        cx.update(gpui_component::init);
        let temp = tempfile::tempdir().unwrap();
        let model = cx.new(|_| {
            let mut state = AppState::default();
            activate_test_session(&mut state, "original", &temp.path().join("original.jsonl"));
            let definition = Definition {
                id: "research".into(),
                revision: 1,
                name: "Research".into(),
                prompt: "Research without edits. ".repeat(200),
                project: temp.path().into(),
                model: "model".into(),
                effort: "medium".into(),
                worktree: false,
                schedule: Schedule::Manual,
                enabled: false,
                notify_all: false,
                anchor: 0,
                next_at: None,
                failures: 0,
                paused_reason: None,
            };
            state.automations.snapshot.definitions = vec![definition.clone()];
            state.automations.snapshot.runs = [RunStatus::Failed, RunStatus::Running]
                .into_iter()
                .enumerate()
                .map(|(i, status)| Run {
                    id: i.to_string(),
                    definition: definition.clone(),
                    scheduled_for: None,
                    created_at: 0,
                    finished_at: None,
                    status,
                    session_id: format!("automation_{i}"),
                    session_file: None,
                    error: None,
                    reviewed: false,
                })
                .collect();
            state
        });
        let shared = model.clone();
        let (_, cx) = cx.add_window_view(move |window, cx| {
            let view = cx.new(|cx| {
                let mut view = super::AutomationsView::new(shared, cx);
                view.selected = Some("research".into());
                view.page = 9; // A removed last page must fall back to existing rows.
                view
            });
            gpui_component::Root::new(view, window, cx)
        });
        cx.run_until_parked();
        cx.update(|window, cx| window.draw(cx).clear(cx));
        assert!(cx.debug_bounds("automation-prompt-body").is_none());
        assert!(cx.debug_bounds("remove-run-0").is_some());
        assert!(cx.debug_bounds("remove-run-1").is_none());
        let toggle = cx.debug_bounds("automation-prompt-toggle").unwrap();
        cx.simulate_click(toggle.center(), Modifiers::default());
        cx.run_until_parked();
        cx.update(|window, cx| window.draw(cx).clear(cx));
        assert!(cx.debug_bounds("automation-prompt-body").is_some());
        model.read_with(cx, |state, _| {
            assert_eq!(state.active_session_id.as_deref(), Some("original"))
        });
    }

    #[gpui::test]
    fn automation_sheet_supports_pointer_and_keyboard_without_switching_chat(
        cx: &mut TestAppContext,
    ) {
        cx.update(gpui_component::init);
        cx.update(super::init);
        let temp = tempfile::tempdir().unwrap();
        let model = cx.new(|_| {
            let mut state = AppState::default();
            activate_test_session(&mut state, "original", &temp.path().join("original.jsonl"));
            state
        });
        let original = model.read_with(cx, |state, _| {
            (
                state.active_session_id.clone(),
                state.active_work_dir.clone(),
            )
        });
        let shared = model.clone();
        let (_, cx) = cx.add_window_view(move |window, cx| {
            let view = cx.new(|cx| super::AutomationsView::new(shared, cx));
            gpui_component::Root::new(view, window, cx)
        });
        cx.run_until_parked();
        cx.update(|window, cx| window.draw(cx).clear(cx));
        let button = cx.debug_bounds("new-automation").unwrap();
        cx.simulate_click(button.center(), Modifiers::default());
        cx.run_until_parked();
        cx.update(|window, cx| {
            assert!(window.has_active_sheet(cx));
            window.draw(cx).clear(cx);
        });
        let save = cx.debug_bounds("automation-editor-save").unwrap();
        cx.simulate_click(save.center(), Modifiers::default());
        cx.run_until_parked();
        cx.update(|window, cx| {
            assert!(
                window.has_active_sheet(cx),
                "invalid fields must keep the editor open"
            )
        });
        cx.simulate_keystrokes("escape");
        cx.run_until_parked();
        cx.update(|window, cx| assert!(!window.has_active_sheet(cx)));
        model.read_with(cx, |state, _| {
            assert_eq!(
                (
                    state.active_session_id.clone(),
                    state.active_work_dir.clone()
                ),
                original
            );
            assert!(state.automations.snapshot.definitions.is_empty());
        });
    }
}
