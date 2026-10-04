//! Shared environment presentation; hosts own current data and domain actions.
use gpui::{prelude::*, *};
use gpui_component::button::{Button, ButtonVariants};
use gpui_component::menu::{DropdownMenu, PopupMenu, PopupMenuItem};
use gpui_component::scroll::ScrollableElement;
use gpui_component::{ActiveTheme, Disableable, Icon, IconName, Sizable};
use threadlane_protocol::repo::GitStatus;
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EnvironmentAction {
    Branches,
    Review,
    Commit,
    Pull,
    Push,
    CreatePullRequest,
    CreateBranch,
    Repository,
    Files,
    Terminal,
}
pub fn environment_fits(width: Pixels, rem: Pixels) -> bool {
    width >= rem * (threadlane_ui_theme::CHAT_CONTENT_MAX_WIDTH + 24.0)
}
pub fn environment_changes_label(status: Option<&GitStatus>) -> String {
    let Some(status) = status else {
        return "Git status unavailable".into();
    };
    let summary = match status.files.len() {
        0 => "No uncommitted changes".to_string(),
        1 => "1 changed file".to_string(),
        count => format!("{count} changed files"),
    };
    let (added, removed) = status
        .files
        .iter()
        .fold((0_u64, 0_u64), |(added, removed), file| {
            (
                added + u64::from(file.additions),
                removed + u64::from(file.deletions),
            )
        });
    if added > 0 || removed > 0 {
        format!("{summary} · +{added} −{removed}")
    } else {
        summary
    }
}
/// Menu builders read host state when opened, preserving current Git capabilities.
pub fn environment_panel(
    name: String,
    location: &'static str,
    status: Option<&GitStatus>,
    checkout_available: bool,
    git_menu: impl Fn(PopupMenu, &mut Window, &mut Context<PopupMenu>) -> PopupMenu + 'static,
    efficiency: AnyElement,
    on_action: impl Fn(EnvironmentAction, &mut Window, &mut App) + 'static,
    cx: &App,
) -> AnyElement {
    let branch = status
        .and_then(|status| status.branch.clone())
        .unwrap_or_else(|| {
            if status.is_some_and(|s| s.detached) {
                "Detached HEAD"
            } else {
                "Branch unavailable"
            }
            .into()
        });
    let changes = environment_changes_label(status);
    let theme = cx.theme();
    let changes_content = status.map(|status| {
        let (added, removed) =
            status
                .files
                .iter()
                .fold((0_u64, 0_u64), |(added, removed), file| {
                    (
                        added + u64::from(file.additions),
                        removed + u64::from(file.deletions),
                    )
                });
        let summary = changes.split(" · ").next().unwrap_or(&changes).to_owned();
        let mut content = div().flex().items_center().min_w_0().child(summary);
        if added > 0 || removed > 0 {
            content = content.child(" · ");
            content = content.child(
                div()
                    .debug_selector(|| "environment-changes-additions".into())
                    .text_color(theme.success)
                    .child(format!("+{added}")),
            );
            content = content.child(" ");
            content = content.child(
                div()
                    .debug_selector(|| "environment-changes-deletions".into())
                    .text_color(theme.danger)
                    .child(format!("−{removed}")),
            );
        }
        content.into_any_element()
    });
    let changes_content = changes_content.unwrap_or_else(|| changes.clone().into_any_element());
    let on_action = std::rc::Rc::new(on_action);
    // Button's built-in icon/label wrapper centers its contents independently.
    let action_content = |icon: Icon, label: AnyElement| {
        div()
            .w_full()
            .min_w_0()
            .flex()
            .items_center()
            .gap_2()
            .child(icon.small().flex_none())
            .child(div().min_w_0().truncate().child(label))
    };
    div()
        .id("chat-environment")
        .debug_selector(|| "chat-environment".into())
        .w(rems(18.0))
        .flex_none()
        .min_h_0()
        .overflow_y_scrollbar()
        .pt_5()
        .pl_3()
        .child(
            div()
                .w_full()
                .flex()
                .flex_col()
                .gap_1()
                .text_sm()
                .p_1()
                .pb_2()
                .rounded_xl()
                .border_1()
                .border_color(theme.border.opacity(0.4))
                .bg(theme.muted.opacity(0.14))
                .child(
                    div()
                        .px_2()
                        .py_1()
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .child("Environment"),
                )
                .child(
                    div()
                        .px_2()
                        .flex()
                        .items_center()
                        .gap_2()
                        .child(Icon::new(IconName::Folder).small())
                        .child(div().min_w_0().truncate().child(name)),
                )
                .child(
                    div()
                        .px_2()
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .child(location),
                )
                .child(
                    Button::new("environment-branch")
                        .ghost()
                        .small()
                        .w_full()
                        .justify_start()
                        .accessibility_label(format!("Manage branches: {branch}"))
                        .child(action_content(
                            Icon::default().path("icons/git/branch.svg"),
                            div().child(branch.clone()).into_any_element(),
                        ))
                        .tooltip(format!("Manage branches: {branch}"))
                        .disabled(status.is_none())
                        .on_click({
                            let on_action = on_action.clone();
                            move |_, window, cx| on_action(EnvironmentAction::Branches, window, cx)
                        }),
                )
                .child(
                    Button::new("environment-changes")
                        .debug_selector(|| "environment-changes".into())
                        .ghost()
                        .small()
                        .w_full()
                        .justify_start()
                        .accessibility_label(changes.clone())
                        .child(action_content(Icon::new(IconName::File), changes_content))
                        .disabled(!checkout_available)
                        .tooltip("Review workspace changes")
                        .on_click({
                            let on_action = on_action.clone();
                            move |_, window, cx| on_action(EnvironmentAction::Review, window, cx)
                        }),
                )
                .children(status.map(|status| {
                    div()
                        .flex()
                        .flex_col()
                        .gap_2()
                        .children((status.ahead > 0 || status.behind > 0).then(|| {
                            div()
                                .debug_selector(|| "environment-sync".into())
                                .px_2()
                                .text_xs()
                                .text_color(theme.muted_foreground)
                                .child(format!("{} ahead · {} behind", status.ahead, status.behind))
                        }))
                        .child(
                            Button::new("environment-git-actions")
                                .debug_selector(|| "environment-git-actions".into())
                                .ghost()
                                .small()
                                .w_full()
                                .justify_start()
                                .accessibility_label("Git actions")
                                .tooltip("Commit, pull, push, create a pull request or branch")
                                .child(action_content(
                                    Icon::default().path("icons/git/actions.svg"),
                                    div().child("Git actions").into_any_element(),
                                ))
                                .dropdown_menu(git_menu),
                        )
                        .children(status.remote.as_ref().map(|remote| {
                            let on_action = on_action.clone();
                            let repository = remote
                                .trim()
                                .trim_end_matches('/')
                                .rsplit('/')
                                .next()
                                .unwrap_or_default()
                                .trim_end_matches(".git");
                            let repository = if repository.is_empty() {
                                "GitHub repository"
                            } else {
                                repository
                            };
                            Button::new("environment-repository")
                                .debug_selector(|| "environment-repository".into())
                                .ghost()
                                .small()
                                .w_full()
                                .justify_start()
                                .accessibility_label(format!("{repository}: open GitHub workspace"))
                                .tooltip(format!("Open GitHub workspace for {repository}"))
                                .child(action_content(
                                    Icon::new(IconName::Github),
                                    div().child(repository.to_owned()).into_any_element(),
                                ))
                                .on_click(move |_, window, cx| {
                                    on_action(EnvironmentAction::Repository, window, cx)
                                })
                        }))
                        .children(
                            status
                                .pr
                                .as_ref()
                                .filter(|pr| {
                                    pr.url.starts_with("https://") || pr.url.starts_with("http://")
                                })
                                .map(|pr| {
                                    div()
                                        .debug_selector(|| "environment-pr".into())
                                        .px_2()
                                        .min_w_0()
                                        .child(
                                            gpui_kit::base::Link::new("environment-pr-link")
                                                .href(pr.url.clone())
                                                .open_with(|url, _, _, cx| cx.open_url(url))
                                                .accessibility_label(format!(
                                                    "Open PR #{}: {}",
                                                    pr.number, pr.title
                                                ))
                                                .text_color(theme.link)
                                                .underline()
                                                .cursor_pointer()
                                                .border_1()
                                                .border_color(theme.transparent)
                                                .rounded(theme.radius)
                                                .hover(|style| style.bg(theme.list_hover))
                                                .focus_visible(|style| {
                                                    style.border_color(theme.primary)
                                                })
                                                .child(div().min_w_0().truncate().child(format!(
                                                    "PR #{} · {}",
                                                    pr.number, pr.title
                                                ))),
                                        )
                                }),
                        )
                }))
                .child(
                    div()
                        .mt_1()
                        .pt_2()
                        .border_t_1()
                        .border_color(theme.border)
                        .child(
                            Button::new("environment-files")
                                .debug_selector(|| "environment-files".into())
                                .ghost()
                                .small()
                                .w_full()
                                .justify_start()
                                .accessibility_label("Files")
                                .child(action_content(
                                    Icon::new(IconName::Folder),
                                    div().child("Files").into_any_element(),
                                ))
                                .disabled(!checkout_available)
                                .on_click({
                                    let on_action = on_action.clone();
                                    move |_, window, cx| {
                                        on_action(EnvironmentAction::Files, window, cx)
                                    }
                                }),
                        ),
                )
                .child(
                    Button::new("environment-terminal")
                        .debug_selector(|| "environment-terminal".into())
                        .ghost()
                        .small()
                        .w_full()
                        .justify_start()
                        .accessibility_label("Terminal")
                        .child(action_content(
                            Icon::new(IconName::SquareTerminal),
                            div().child("Terminal").into_any_element(),
                        ))
                        .disabled(!checkout_available)
                        .on_click(move |_, window, cx| {
                            on_action(EnvironmentAction::Terminal, window, cx)
                        }),
                )
                .child(efficiency),
        )
        .into_any_element()
}

/// PR eligibility is supplied by the host's existing domain policy.
pub fn environment_git_menu(
    menu: PopupMenu,
    status: Option<&GitStatus>,
    can_create_pull_request: bool,
    command: impl Fn(&'static str, EnvironmentAction) -> PopupMenuItem,
) -> PopupMenu {
    let item = |label, action, enabled: bool| command(label, action).disabled(!enabled);
    let can_sync = status.is_some_and(|s| s.remote.is_some() && s.branch.is_some() && !s.detached);
    menu.item(item(
        "Commit…",
        EnvironmentAction::Commit,
        status.is_some_and(|s| !s.files.is_empty()),
    ))
    .item(item(
        "Pull",
        EnvironmentAction::Pull,
        can_sync && status.is_some_and(|s| s.has_upstream),
    ))
    .item(item("Push", EnvironmentAction::Push, can_sync))
    .separator()
    .item(item(
        "Create draft PR…",
        EnvironmentAction::CreatePullRequest,
        can_create_pull_request,
    ))
    .item(item(
        "Create branch…",
        EnvironmentAction::CreateBranch,
        status.is_some(),
    ))
}
