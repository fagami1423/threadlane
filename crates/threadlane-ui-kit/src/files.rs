//! File navigation presentation. Hosts supply entries, absolute paths and file actions.
use gpui::{prelude::*, *};
use gpui_component::button::{Button, ButtonVariants};
use gpui_component::list::ListItem;
use gpui_component::menu::{PopupMenu, PopupMenuItem};
use gpui_component::tree::{Tree, TreeState};
use gpui_component::{ActiveTheme, Icon, IconName, Sizable};

pub fn project_files_surface() -> Div {
    div().flex_1().min_w_0().min_h_0().flex().flex_col().py_2()
}

pub fn project_files_find_button() -> Button {
    Button::new("find-in-files")
        .debug_selector(|| "find-in-files".into())
        .label("Find in files…")
        .ghost()
        .small()
}

pub fn project_file_row(
    path: &str,
    label: &str,
    depth: usize,
    folder: bool,
    expanded: bool,
    selected: bool,
    cx: &App,
) -> ListItem {
    let theme = cx.theme().colors;
    ListItem::new(SharedString::from(format!("tree-item-{path}")))
        .debug_selector({
            let path = path.to_owned();
            move || format!("project-file-{path}")
        })
        .mx_1()
        .min_w_0()
        .rounded_md()
        .px_1p5()
        .py_1()
        .pl(rems(0.375 + depth as f32 * 0.75))
        .selected(selected)
        .child(
            div()
                .min_w_0()
                .flex()
                .items_center()
                .gap_1p5()
                .text_xs()
                .text_color(if selected {
                    theme.foreground
                } else {
                    theme.muted_foreground
                })
                .child(
                    div()
                        .w(rems(0.875))
                        .flex_none()
                        .flex()
                        .items_center()
                        .justify_center()
                        .children(folder.then(|| {
                            Icon::new(if expanded {
                                IconName::ChevronDown
                            } else {
                                IconName::ChevronRight
                            })
                            .xsmall()
                        })),
                )
                .child(
                    Icon::new(if folder {
                        IconName::Folder
                    } else {
                        IconName::File
                    })
                    .xsmall()
                    .flex_none(),
                )
                .child(div().min_w_0().flex_1().truncate().child(label.to_owned())),
        )
}

/// Uses GPUI Kit's tree for keyboard navigation, expansion and scrolling on both platforms.
pub fn project_file_tree(
    state: &Entity<TreeState>,
    on_open: impl Fn(&str, &mut Window, &mut App) + 'static,
    on_menu: impl Fn(&str, bool, PopupMenu, &mut Window, &mut App) -> PopupMenu + 'static,
) -> impl IntoElement {
    let on_open = std::rc::Rc::new(on_open);
    Tree::new(state, move |_, entry, selected, _, cx| {
        let path = entry.item().id.to_string();
        let folder = entry.is_folder();
        let on_open = on_open.clone();
        project_file_row(
            &path,
            &entry.item().label,
            entry.depth(),
            folder,
            entry.is_expanded(),
            selected,
            cx,
        )
        .when(!folder, |item| {
            item.on_click(move |_, window, cx| on_open(&path, window, cx))
        })
    })
    .context_menu(move |_, entry, menu, window, cx| {
        on_menu(&entry.item().id, entry.is_folder(), menu, window, cx)
    })
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProjectFileAction {
    Open(String),
    OpenInPanel(String),
    CopyRelative(String),
    CopyAbsolute(String),
}

pub fn project_file_menu(
    menu: PopupMenu,
    path: &str,
    folder: bool,
    absolute: Option<String>,
    on_action: impl Fn(&ProjectFileAction, &mut Window, &mut App) + 'static,
) -> PopupMenu {
    let callback = std::rc::Rc::new(on_action);
    let mut actions = Vec::new();
    if !folder {
        actions.push(("Open in Editor Tab", ProjectFileAction::Open(path.into())));
        actions.push(("Open in Panel", ProjectFileAction::OpenInPanel(path.into())));
    }
    actions.push((
        "Copy Relative Path",
        ProjectFileAction::CopyRelative(path.into()),
    ));
    if let Some(absolute) = absolute {
        actions.push((
            "Copy Absolute Path",
            ProjectFileAction::CopyAbsolute(absolute),
        ));
    }
    actions.into_iter().fold(menu, |menu, (label, action)| {
        let callback = callback.clone();
        menu.item(
            PopupMenuItem::new(label).on_click(move |_, window, cx| callback(&action, window, cx)),
        )
    })
}
