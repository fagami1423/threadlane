//! Controlled conversation code chrome. Hosts retain file and terminal guards.
use gpui::{prelude::*, *};
use gpui_component::button::{Button, ButtonVariants};
use gpui_component::scroll::ScrollableElement;
use gpui_component::tag::{Tag, TagVariant};
use gpui_component::{ActiveTheme, Icon, IconName, Sizable};

pub fn code_block_surface(key: &str, cx: &App) -> Stateful<Div> {
    let selector = format!("code-block-{key}");
    super::result_surface(&cx.theme().colors)
        .id(SharedString::from(selector.clone()))
        .debug_selector(move || selector.clone())
        .my_2p5()
        .rounded_xl()
        .border_color(cx.theme().border.opacity(0.35))
        .bg(cx.theme().muted.opacity(0.18))
}

pub fn code_block_header(
    key: &str,
    language: &str,
    path: Option<&str>,
    actions: impl IntoElement,
    cx: &App,
) -> Div {
    let language = if language.trim().is_empty() {
        "code"
    } else {
        language.trim()
    };
    super::result_header(&cx.theme().colors)
        .flex_wrap()
        .justify_between()
        .px_3p5()
        .child(
            div()
                .flex()
                .flex_1()
                .min_w_0()
                .items_center()
                .gap_2()
                .child(
                    Tag::new()
                        .child(language.to_owned())
                        .with_variant(TagVariant::Secondary)
                        .small(),
                )
                .children(path.map(|path| {
                    let path: SharedString = path.to_owned().into();
                    let tip = path.clone();
                    div()
                        .id(SharedString::from(format!("code-path-{key}")))
                        .flex()
                        .flex_1()
                        .min_w_0()
                        .items_center()
                        .gap_1()
                        .text_xs()
                        .text_color(cx.theme().muted_foreground)
                        .aria_label(path.clone())
                        .tooltip(move |window, cx| {
                            gpui_component::tooltip::Tooltip::new(tip.clone()).build(window, cx)
                        })
                        .child(Icon::new(IconName::File).xsmall().flex_shrink_0())
                        .child(div().min_w_0().truncate().child(path))
                })),
        )
        .child(actions)
}

pub fn code_block_actions() -> Div {
    div()
        .flex()
        .flex_wrap()
        .items_center()
        .justify_end()
        .gap_1()
        .min_w_0()
        .max_w_full()
}

pub fn code_block_run_button(key: &str) -> Button {
    let selector = format!("run-term-{key}");
    Button::new(SharedString::from(selector.clone()))
        .debug_selector(move || selector.clone())
        .icon(IconName::SquareTerminal)
        .label("Run in Terminal")
        .accessibility_label("Run in active project terminal")
        .tooltip("Run in active project terminal")
        .xsmall()
        .secondary()
}

pub fn code_block_open_button(key: &str) -> Button {
    let selector = format!("open-edit-{key}");
    Button::new(SharedString::from(selector.clone()))
        .debug_selector(move || selector.clone())
        .icon(IconName::File)
        .label("Open in Editor")
        .accessibility_label("Open file in central editor")
        .tooltip("Open file in central editor")
        .xsmall()
        .ghost()
}

pub fn code_block_copy_button(key: &str, copied: bool, cx: &App) -> Button {
    super::conversation::copy_feedback_button(format!("copy-code-{key}"), copied, cx)
        .accessibility_label(if copied {
            "Code copied"
        } else {
            "Copy code to clipboard"
        })
        .tooltip(if copied {
            "Code copied"
        } else {
            "Copy code to clipboard"
        })
}

pub fn code_block_body() -> impl IntoElement + ParentElement {
    div()
        .min_w_0()
        .overflow_x_scrollbar()
        .p_3()
        .font_family("monospace")
        .text_xs()
}
