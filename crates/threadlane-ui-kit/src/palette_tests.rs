use crate::{palette_item, workspace_commands, workspace_palette_command, workspace_palette_frame};
use gpui::{
    div, point, px, AppContext, Context, Entity, IntoElement, Modifiers, ParentElement, Render,
    Styled, TestAppContext, Window,
};
use gpui_component::{
    command::{CommandGroup, CommandState},
    IconName,
};
use std::{cell::RefCell, rc::Rc};

struct PaletteHost {
    state: Entity<CommandState>,
    width: f32,
    confirmed: Rc<RefCell<Vec<(usize, usize)>>>,
    cancelled: Rc<RefCell<usize>>,
}
impl Render for PaletteHost {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let actions =
            CommandGroup::new()
                .label("Commands & Actions")
                .items(workspace_commands().iter().map(|command| {
                    command.item(
                        (command.key() == "add_terminal_selection")
                            .then_some("No terminal is visible"),
                    )
                }));
        let settings = CommandGroup::new().label("Settings").item(palette_item(
            "Appearance & Themes",
            "Settings · Appearance",
            IconName::Settings,
        ));
        let confirmed = self.confirmed.clone();
        let cancelled = self.cancelled.clone();
        let backdrop_cancel = self.cancelled.clone();
        let command = workspace_palette_command(&self.state)
            .group(actions)
            .group(settings)
            .on_confirm(move |index, _, _| confirmed.borrow_mut().push((index.section, index.row)))
            .on_cancel(move |_, _| *cancelled.borrow_mut() += 1);
        div()
            .relative()
            .w(px(self.width))
            .h(px(800.0))
            .child(workspace_palette_frame(
                command,
                move |_, _| *backdrop_cancel.borrow_mut() += 1,
                cx,
            ))
    }
}

#[gpui::test]
fn palette_filtering_preserves_original_commands_and_disabled_activation(cx: &mut TestAppContext) {
    cx.update(gpui_component::init);
    let confirmed = Rc::new(RefCell::new(Vec::new()));
    let output = confirmed.clone();
    let cancelled = Rc::new(RefCell::new(0));
    let cancel_output = cancelled.clone();
    let mut state = None;
    let (_, cx) = cx.add_window_view(|window, cx| {
        let command = cx.new(|cx| CommandState::new(window, cx));
        state = Some(command.clone());
        gpui_component::Root::new(
            cx.new(|_| PaletteHost {
                state: command,
                width: 390.0,
                confirmed,
                cancelled,
            }),
            window,
            cx,
        )
    });
    let state = state.unwrap();
    cx.update(|window, cx| window.draw(cx).clear(cx));
    let modal = cx.debug_bounds("command-palette-modal").unwrap();
    assert!(
        modal.left() >= px(0.0) && modal.right() <= px(390.0),
        "palette must fit a narrow workspace"
    );
    for (query, expected) in [
        ("add selection to chat", None),
        ("toggle sidebar", Some((0, 23))),
        ("themes", Some((1, 0))),
    ] {
        cx.update(|window, cx| {
            state.update(cx, |state, cx| {
                state.set_query(query, window, cx);
                state.focus(window, cx);
            })
        });
        cx.run_until_parked();
        cx.update(|window, cx| window.draw(cx).clear(cx));
        output.borrow_mut().clear();
        cx.simulate_keystrokes("enter");
        cx.run_until_parked();
        assert_eq!(
            output.borrow().first().copied(),
            expected,
            "filtered confirmation for {query}"
        );
    }
    cx.update(|window, cx| {
        state.update(cx, |state, cx| {
            state.set_query("", window, cx);
            state.focus(window, cx);
        })
    });
    cx.run_until_parked();
    cx.update(|window, cx| window.draw(cx).clear(cx));
    cx.simulate_keystrokes("escape");
    cx.run_until_parked();
    assert_eq!(*cancel_output.borrow(), 1);
    cx.simulate_click(
        point(modal.left() + px(8.0), modal.top() + px(2.0)),
        Modifiers::default(),
    );
    cx.run_until_parked();
    assert_eq!(
        *cancel_output.borrow(),
        1,
        "inside clicks must not dismiss the palette"
    );
    cx.simulate_click(point(px(380.0), px(600.0)), Modifiers::default());
    cx.run_until_parked();
    assert_eq!(
        *cancel_output.borrow(),
        2,
        "backdrop click must request dismissal once"
    );
}

#[test]
fn palette_catalogue_routes_and_settings_destinations_are_unique() {
    let commands = workspace_commands();
    let keys: std::collections::HashSet<_> = commands.iter().map(|command| command.key()).collect();
    assert_eq!(keys.len(), commands.len());
    for key in [
        "search_conversations",
        "add_terminal_selection",
        "goal",
        "git_merge",
        "settings",
    ] {
        assert!(keys.contains(key));
    }
    for destination in crate::SETTINGS_SEARCH_ITEMS {
        assert!(crate::settings_search_page(destination.id).is_some());
    }
    assert!(crate::settings_search_page("unknown").is_none());
}
