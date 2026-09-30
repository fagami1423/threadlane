use super::{visible_terminal_frame, LinkDestination, OpenTerminalLink, PtyEvent, TerminalView};
use gpui::{
    point, px, AppContext, Entity, Focusable, Modifiers, MouseButton, MouseDownEvent,
    MouseMoveEvent, MouseUpEvent, TestAppContext,
};
use std::{cell::RefCell, path::PathBuf, rc::Rc};

fn output(terminal: &mut TerminalView, text: &str) {
    let mut parser = vt100::Parser::new(terminal.rows, terminal.cols, 20);
    parser.process(text.as_bytes());
    let frame = visible_terminal_frame(&mut parser, terminal.rows, terminal.cols)
        .with_link_epoch(terminal.frame_epoch);
    terminal.apply_event(PtyEvent::Frame(frame));
}

fn terminal(cx: &mut TestAppContext) -> (Entity<TerminalView>, &mut gpui::VisualTestContext) {
    cx.update(gpui_component::init);
    let (root, cx) = cx.add_window_view(|window, cx| {
        let terminal = cx.new(|cx| TerminalView::new_for_test(PathBuf::from("/tmp"), cx));
        gpui_component::Root::new(terminal, window, cx)
    });
    let terminal = root.read_with(cx, |root, _| {
        root.view().clone().downcast::<TerminalView>().unwrap()
    });
    cx.run_until_parked();
    (terminal, cx)
}

#[gpui::test]
fn links_menu_escape_restores_terminal_and_freezes_destination(cx: &mut TestAppContext) {
    let (terminal, cx) = terminal(cx);
    let opened = Rc::new(RefCell::new(Vec::new()));
    let events = opened.clone();
    let _subscription = cx.update(|_, cx| {
        cx.subscribe(&terminal, move |_, event: &OpenTerminalLink, _| {
            events.borrow_mut().push(event.clone())
        })
    });
    cx.update(|window, cx| {
        terminal.update(cx, |terminal, cx| {
            output(terminal, "http://localhost:3000/ http://localhost:3000/");
            terminal.open_links(window, cx);
        })
    });
    cx.run_until_parked();
    cx.simulate_keystrokes("escape");
    cx.run_until_parked();
    cx.update(|window, cx| {
        assert!(terminal.read(cx).link_menu.is_none());
        assert!(terminal.read(cx).focus_handle(cx).is_focused(window));
        terminal.update(cx, |terminal, cx| terminal.open_links(window, cx));
    });
    cx.run_until_parked();
    let epoch = terminal.read_with(cx, |terminal, _| {
        assert!(!terminal.links.is_empty(), "links lost before reopening");
        terminal.link_epoch
    });
    terminal.update(cx, |terminal, cx| {
        output(terminal, "http://different/");
        cx.notify();
    });
    cx.run_until_parked();
    cx.update(|window, cx| {
        let terminal = terminal.read(cx);
        assert_eq!(terminal.link_epoch, epoch);
        assert!(terminal.link_menu.is_some(), "menu dismissed by output");
        assert!(
            terminal
                .link_menu
                .as_ref()
                .unwrap()
                .read(cx)
                .focus_handle(cx)
                .is_focused(window),
            "menu lost focus"
        );
    });
    cx.simulate_keystrokes("down down");
    cx.run_until_parked();
    cx.simulate_keystrokes("enter");
    cx.run_until_parked();
    assert_eq!(opened.borrow().len(), 1);
    assert_eq!(opened.borrow()[0].url, "http://localhost:3000/");
}

#[gpui::test]
fn links_pointer_requires_modifier_and_stable_press_release(cx: &mut TestAppContext) {
    let (terminal, cx) = terminal(cx);
    let opened = Rc::new(RefCell::new(Vec::new()));
    let events = opened.clone();
    let _subscription = cx.update(|_, cx| {
        cx.subscribe(&terminal, move |_, event: &OpenTerminalLink, _| {
            events.borrow_mut().push(event.clone())
        })
    });
    cx.update(|window, cx| {
        terminal.update(cx, |terminal, cx| {
            output(terminal, "http://localhost:3000/");
            let bounds = terminal.screen_bounds.unwrap();
            let position = bounds.origin + point(px(16.), px(16.));
            let modifiers = if cfg!(target_os = "macos") {
                Modifiers {
                    platform: true,
                    ..Default::default()
                }
            } else {
                Modifiers {
                    control: true,
                    ..Default::default()
                }
            };
            let down = MouseDownEvent {
                position,
                button: MouseButton::Left,
                modifiers,
                click_count: 1,
                ..Default::default()
            };
            let up = MouseUpEvent {
                position,
                button: MouseButton::Left,
                modifiers,
                click_count: 1,
            };
            terminal.begin_selection(
                &MouseDownEvent {
                    modifiers: Modifiers::default(),
                    ..down.clone()
                },
                window,
                cx,
            );
            terminal.end_selection(&up, window, cx);
            terminal.begin_selection(&down, window, cx);
            terminal.extend_selection(
                &MouseMoveEvent {
                    position: position + point(px(10.), px(0.)),
                    pressed_button: Some(MouseButton::Left),
                    modifiers,
                },
                window,
                cx,
            );
            terminal.end_selection(&up, window, cx);
            terminal.begin_selection(&down, window, cx);
            output(terminal, "http://different/");
            terminal.end_selection(&up, window, cx);
            terminal.begin_selection(&down, window, cx);
            terminal.end_selection(&up, window, cx);
        })
    });
    cx.run_until_parked();
    assert_eq!(opened.borrow().len(), 1);
    assert_eq!(opened.borrow()[0].url, "http://different/");
}

#[gpui::test]
fn links_clear_and_resize_reject_queued_frames_and_old_menu_actions(cx: &mut TestAppContext) {
    let terminal = cx.new(|cx| TerminalView::new_for_test(PathBuf::from("/tmp"), cx));
    let opened = Rc::new(RefCell::new(Vec::new()));
    let events = opened.clone();
    let _subscription = cx.update(|cx| {
        cx.subscribe(&terminal, move |_, event: &OpenTerminalLink, _| {
            events.borrow_mut().push(event.clone())
        })
    });
    terminal.update(cx, |terminal, cx| {
        output(terminal, "http://host/");
        let epoch = terminal.link_epoch;
        let mut parser = vt100::Parser::new(terminal.rows, terminal.cols, 0);
        parser.process(b"http://stale/");
        let stale = visible_terminal_frame(&mut parser, terminal.rows, terminal.cols);
        terminal.clear(cx);
        terminal.apply_event(PtyEvent::Frame(stale));
        assert!(terminal.links.is_empty());
        terminal.activate_link(
            "http://host/".into(),
            LinkDestination::DefaultBrowser,
            epoch,
            cx,
        );
        output(terminal, "http://host/");
        let epoch = terminal.link_epoch;
        terminal.resize(10, 40, cx);
        terminal.activate_link(
            "http://host/".into(),
            LinkDestination::DefaultBrowser,
            epoch,
            cx,
        );
        assert!(terminal.links.is_empty());
    });
    cx.run_until_parked();
    assert!(opened.borrow().is_empty());
}

#[test]
fn links_worker_omits_clipped_wrap_and_only_includes_current_viewport() {
    let mut parser = vt100::Parser::new(3, 20, 20);
    parser.process(b" http://host/a-long-path-that-wraps-over-rows\r\nother\r\nlast");
    let frame = visible_terminal_frame(&mut parser, 3, 20);
    assert!(frame.links.is_empty());
    parser.screen_mut().set_scrollback(2);
    let frame = visible_terminal_frame(&mut parser, 3, 20);
    assert_eq!(
        frame.links[0].url,
        "http://host/a-long-path-that-wraps-over-rows"
    );
    assert_eq!(parser.screen().scrollback(), 2);
}

#[gpui::test]
fn links_menu_clear_dismisses_and_restores_focus(cx: &mut TestAppContext) {
    let (terminal, cx) = terminal(cx);
    cx.update(|window, cx| {
        terminal.update(cx, |terminal, cx| {
            output(terminal, "http://host/");
            terminal.open_links(window, cx);
        })
    });
    cx.run_until_parked();
    terminal.update(cx, |terminal, cx| terminal.clear(cx));
    cx.run_until_parked();
    cx.update(|window, cx| {
        assert!(terminal.read(cx).link_menu.is_none());
        assert!(terminal.read(cx).focus_handle(cx).is_focused(window));
    });
}

#[test]
fn links_validation_keeps_destination_and_rejects_parser_fixups() {
    for url in [
        "https://host/%",
        "https://host/%zz",
        "https://host/\n",
        "http://@host/",
        "https://host\\other",
        "http:host",
        "http:///host",
    ] {
        assert!(!super::is_web_url(url), "{url:?}");
    }
    let mut parser = vt100::Parser::new(4, 80, 0);
    parser.process("http://0.0.0.0:3000/界e\u{301}?q=x%20y#fragment ".as_bytes());
    let frame = visible_terminal_frame(&mut parser, 4, 80);
    assert_eq!(
        frame.links[0].url,
        "http://0.0.0.0:3000/界e\u{301}?q=x%20y#fragment"
    );
    // Both columns of the wide path character belong to the same destination.
    assert!(frame.links[0].cells.contains(&(0, 18)));
    assert!(frame.links[0].cells.contains(&(0, 19)));
}
