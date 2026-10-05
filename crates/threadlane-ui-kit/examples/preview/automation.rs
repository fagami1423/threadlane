//! Local automation samples. No scheduler, stores, sessions, or provider calls.
use gpui::{App, AppContext, Context, IntoElement, Render, Window};
use gpui_component::WindowExt;
use threadlane_automation::{Definition, Run, RunStatus, Schedule, Snapshot};
use threadlane_ui_kit::automation::{automation_screen, AutomationAction, AutomationScreen};
use threadlane_ui_kit::automation_form::automation_editor_sheet;

pub struct AutomationPreview {
    snapshot: Snapshot,
    screen: AutomationScreen,
    next_run: usize,
}

impl AutomationPreview {
    pub fn new(project: std::path::PathBuf) -> Self {
        let mut definition = Definition {
            id: "sample-research".into(), revision: 1,
            name: "Sample · Review project activity".into(),
            prompt: "Summarize recent work, identify unfinished tasks, and suggest the next useful step. This is a local UI sample; no prompt is executed.".into(),
            project: project.clone(), model: "Sample model".into(), effort: "medium".into(),
            worktree: false, schedule: Schedule::Calendar {
                hour: 9, minute: 0, days: vec![0, 1, 2, 3, 4], timezone: "UTC".into(),
            }, enabled: true, notify_all: false, anchor: 1_791_000_000,
            next_at: None, failures: 0, paused_reason: None,
        };
        definition.next_at = definition
            .schedule
            .next(definition.anchor, definition.anchor)
            .expect("valid sample schedule");
        let mut manual = definition.clone();
        manual.id = "sample-worktree".into();
        manual.name = "Sample · Inspect workspace changes".into();
        manual.worktree = true;
        manual.schedule = Schedule::Manual;
        manual.next_at = None;
        let runs = (0..31)
            .map(|i| Run {
                id: format!("sample-{i}"),
                definition: definition.clone(),
                scheduled_for: None,
                created_at: 1_791_000_000 + i * 3600,
                finished_at: None,
                status: match i {
                    30 => RunStatus::WaitingAnswer,
                    29 => RunStatus::Failed,
                    _ => RunStatus::Succeeded,
                },
                session_id: format!("sample-chat-{i}"),
                session_file: None,
                error: (i == 29)
                    .then(|| "Sample failure: provider connection was interrupted.".into()),
                reviewed: i < 28,
            })
            .collect();
        Self {
            snapshot: Snapshot {
                definitions: vec![definition, manual],
                runs,
                ..Default::default()
            },
            screen: AutomationScreen {
                projects: vec![(
                    project.to_string_lossy().into_owned(),
                    project
                        .file_name()
                        .unwrap_or_default()
                        .to_string_lossy()
                        .into_owned(),
                )],
                ..Default::default()
            },
            next_run: 31,
        }
    }

    pub fn attention_count(&self) -> usize {
        self.snapshot
            .runs
            .iter()
            .filter(|run| run.needs_attention())
            .count()
    }

    fn apply(&mut self, action: AutomationAction, window: &mut Window, cx: &mut Context<Self>) {
        match action {
            AutomationAction::Scope(scope) => {
                self.screen.scope = scope;
                self.screen.selected = None;
                self.screen.page = 0;
            }
            AutomationAction::Select(id) => {
                self.screen.selected = Some(id);
                self.screen.page = 0;
            }
            AutomationAction::Back => {
                self.screen.selected = None;
                self.screen.page = 0;
            }
            AutomationAction::History(history) => {
                self.screen.history = history;
                self.screen.selected = None;
                self.screen.page = 0;
            }
            AutomationAction::AttentionOnly(attention) => {
                self.screen.attention_only = attention;
                self.screen.page = 0;
            }
            AutomationAction::ExpandPrompt(id) => self.screen.expanded_prompt = id,
            AutomationAction::Page(page) => self.screen.page = page,
            AutomationAction::SetEnabled(id, enabled) => {
                if let Some(d) = self.snapshot.definitions.iter_mut().find(|d| d.id == id) {
                    d.enabled = enabled;
                }
            }
            AutomationAction::Delete(id) => {
                self.snapshot.definitions.retain(|d| d.id != id);
                self.screen.selected = None;
                self.screen.history = true;
                self.screen.page = 0;
            }
            AutomationAction::CancelRun(id) => {
                if let Some(run) = self.snapshot.runs.iter_mut().find(|r| r.id == id) {
                    run.status = RunStatus::Cancelled;
                }
            }
            AutomationAction::ReviewRun(id) => {
                if let Some(run) = self.snapshot.runs.iter_mut().find(|r| r.id == id) {
                    run.reviewed = true;
                }
            }
            AutomationAction::DeleteRun(id) => self.snapshot.runs.retain(|run| run.id != id),
            AutomationAction::RunNow(id) => {
                if let Some(definition) = self
                    .snapshot
                    .definitions
                    .iter()
                    .find(|d| d.id == id)
                    .cloned()
                {
                    let id = format!("sample-{}", self.next_run);
                    self.next_run += 1;
                    self.snapshot.runs.push(Run {
                        id: id.clone(),
                        definition,
                        scheduled_for: None,
                        created_at: 1_791_120_000,
                        finished_at: None,
                        status: RunStatus::Queued,
                        session_id: id,
                        session_file: None,
                        error: None,
                        reviewed: false,
                    });
                }
            }
            AutomationAction::Edit(id) => {
                self.open_editor(id, window, cx);
            }
            AutomationAction::OpenChat(_) => Self::notice(window, cx),
        }
        cx.notify();
    }

    fn open_editor(
        &mut self,
        id: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<gpui::Entity<crate::automation_editor::AutomationSampleEditor>> {
        let editing = id.is_some();
        let definition = if let Some(id) = id {
            self.snapshot
                .definitions
                .iter()
                .find(|d| d.id == id)?
                .clone()
        } else {
            let project = self.screen.projects.first()?.0.clone().into();
            let id = format!("sample-automation-{}", self.next_run);
            self.next_run += 1;
            Definition {
                id,
                revision: 0,
                name: String::new(),
                prompt: String::new(),
                project,
                model: "Sample model".into(),
                effort: "medium".into(),
                worktree: true,
                schedule: Schedule::Interval { minutes: 60 },
                enabled: true,
                notify_all: false,
                anchor: 1_791_000_000,
                next_at: None,
                failures: 0,
                paused_reason: None,
            }
        };
        let owner = cx.entity().downgrade();
        let projects = self.screen.projects.clone();
        let editor = cx.new(|cx| {
            crate::automation_editor::AutomationSampleEditor::new(
                definition,
                projects,
                move |mut definition, cx| {
                    let _ = owner.update(cx, |this, cx| {
                        definition.revision += 1;
                        definition.next_at = definition
                            .schedule
                            .next(definition.anchor, definition.anchor)
                            .ok()
                            .flatten();
                        if let Some(existing) = this
                            .snapshot
                            .definitions
                            .iter_mut()
                            .find(|d| d.id == definition.id)
                        {
                            *existing = definition;
                        } else {
                            this.snapshot.definitions.push(definition);
                        }
                        cx.notify();
                    });
                },
                window,
                cx,
            )
        });
        let sheet_editor = editor.clone();
        window.open_sheet(cx, move |sheet, _, _| {
            automation_editor_sheet(sheet, editing, sheet_editor.clone())
        });
        Some(editor)
    }

    fn notice(window: &mut Window, cx: &mut App) {
        window.open_alert_dialog(cx, |dialog, _, _| dialog.title("Local automation samples")
            .description("Sample runs have no associated chats. The shared form saves only to this in-memory preview; no prompts are executed."));
    }
}

impl Render for AutomationPreview {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let owner = cx.entity().downgrade();
        automation_screen(
            &self.snapshot,
            &self.screen,
            move |action, window, cx| {
                let _ = owner.update(cx, |this, cx| this.apply(action, window, cx));
            },
            cx,
        )
    }
}

#[cfg(test)]
mod tests {
    use gpui::{AppContext, Modifiers, TestAppContext};
    use threadlane_automation::RunStatus;

    #[gpui::test]
    fn shared_history_review_remove_and_cancel_update_local_attention(cx: &mut TestAppContext) {
        cx.update(gpui_component::init);
        let captured = std::rc::Rc::new(std::cell::RefCell::new(None));
        let capture = captured.clone();
        let (_, cx) = cx.add_window_view(move |window, cx| {
            let preview = cx.new(|_| {
                let mut preview = super::AutomationPreview::new("/sample-project".into());
                preview.screen.history = true;
                preview.screen.attention_only = true;
                preview
            });
            *capture.borrow_mut() = Some(preview.clone());
            gpui_component::Root::new(preview, window, cx)
        });
        let preview = captured.borrow_mut().take().unwrap();
        cx.run_until_parked();
        cx.update(|window, cx| window.draw(cx).clear(cx));
        assert_eq!(
            preview.read_with(cx, |preview, _| preview.attention_count()),
            3
        );
        for (selector, expected) in [
            ("review-run-sample-28", 2),
            ("remove-run-sample-29", 1),
            ("cancel-run-sample-30", 0),
        ] {
            let bounds = cx.debug_bounds(selector).expect("shared action is visible");
            cx.simulate_click(bounds.center(), Modifiers::default());
            cx.run_until_parked();
            cx.update(|window, cx| window.draw(cx).clear(cx));
            assert_eq!(
                preview.read_with(cx, |preview, _| preview.attention_count()),
                expected
            );
        }
        preview.read_with(cx, |state, _| {
            assert!(state
                .snapshot
                .runs
                .iter()
                .all(|run| run.session_file.is_none()));
            assert!(!state.snapshot.runs.iter().any(|run| run.id == "sample-29"));
            assert_eq!(
                state
                    .snapshot
                    .runs
                    .iter()
                    .find(|run| run.id == "sample-30")
                    .unwrap()
                    .status,
                RunStatus::Cancelled
            );
        });
        assert!(cx.debug_bounds("automation-run-sample-30").is_none());
        assert!(
            cx.debug_bounds("automation-all-runs").is_some(),
            "empty attention history keeps its recovery action"
        );
    }
}
