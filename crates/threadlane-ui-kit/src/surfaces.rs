//! Shared result chrome for commands, files, searches and diffs.
use gpui::{prelude::*, *};
use gpui_component::theme::ThemeColor;

pub fn result_surface(theme: &ThemeColor) -> Div {
    div()
        .w_full()
        .min_w_0()
        .rounded_lg()
        .border_1()
        .border_color(theme.border.opacity(0.5))
        .bg(theme.title_bar)
        .overflow_hidden()
}

pub fn result_header(theme: &ThemeColor) -> Div {
    div()
        .flex()
        .items_center()
        .gap_2()
        .min_w_0()
        .px_3()
        .py_1p5()
        .bg(theme.background.opacity(0.35))
        .border_b_1()
        .border_color(theme.border.opacity(0.3))
}

/// The output scrolls inside this viewport without moving the conversation.
pub fn result_viewport(id: String) -> Stateful<Div> {
    div()
        .id(SharedString::from(id))
        .h(rems(6.0))
        .flex_none()
        .on_scroll_wheel(|_, _, cx| cx.stop_propagation())
}
