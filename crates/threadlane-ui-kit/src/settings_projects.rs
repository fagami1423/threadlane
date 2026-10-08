//! Attached-project presentation; the host owns persistence and removal guards.
use gpui::{prelude::*, *};
use gpui_component::button::{Button, ButtonVariants};
use gpui_component::dialog::{AlertDialog, DialogButtonProps};
use gpui_component::{ActiveTheme, Disableable, Sizable};
use std::rc::Rc;

pub struct SettingsProject {
    pub path: String,
    pub name: String,
    pub active: bool,
    pub disabled_reason: Option<String>,
}

pub fn settings_projects(
    projects: Vec<SettingsProject>,
    error: Option<String>,
    on_remove: impl Fn(String, &mut Window, &mut App) + 'static,
    cx: &App,
) -> AnyElement {
    let on_remove = Rc::new(on_remove);
    let muted = cx.theme().muted_foreground;
    super::settings_group(cx)
        .flex_col()
        .gap_3()
        .child(div().text_sm().font_weight(FontWeight::MEDIUM).child("Attached projects"))
        .child(div().text_xs().text_color(muted).child("Removing a project keeps its files and saved conversations. Attach the folder again to restore access. Automations are managed separately."))
        .when(projects.is_empty(), |group| group.child(div().text_sm().child("No attached projects. Use Attach project in the sidebar to add a folder.")))
        .children(projects.into_iter().map(|project| {
            let on_remove = on_remove.clone();
            let path = project.path.clone();
            let id: SharedString = format!("remove-project-{}", project.path).into();
            div().flex().items_center().gap_3()
                .child(div().flex_1().min_w_0()
                    .child(div().text_sm().child(format!("{}{}", project.name, if project.active { " · Active" } else { "" })))
                    .child(div().text_xs().text_color(muted).child(project.path))
                    .when_some(project.disabled_reason.clone(), |row, reason| row.child(div().text_xs().text_color(muted).child(reason))))
                .child(Button::new(id).label("Remove…").outline().small()
                    .accessibility_label(format!("Remove project {}", project.name))
                    .disabled(project.disabled_reason.is_some())
                    .on_click(move |_, window, cx| on_remove(path.clone(), window, cx)))
        }))
        .when_some(error, |group, error| group.child(div().text_sm().text_color(cx.theme().danger).child(error)))
        .into_any_element()
}

pub fn project_removal_dialog(alert: AlertDialog, name: &str, path: &str) -> AlertDialog {
    alert.title(format!("Remove “{name}”?"))
        .description(format!("{path}\n\nRemove this project from Threadlane? Files and saved conversations will not be deleted. You can attach the folder again. Automations will not be removed or paused."))
        .button_props(DialogButtonProps::default().ok_text("Remove project").show_cancel(true))
}
