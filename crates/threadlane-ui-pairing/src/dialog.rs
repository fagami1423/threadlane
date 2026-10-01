//! Pairing dialog: start/stop the LAN listener and render the
//! `threadlane://pair` deep link as a scannable QR code.

use gpui::prelude::*;
use gpui::*;
use gpui_component::button::{Button, ButtonVariants};
use gpui_component::{v_flex, ActiveTheme, Disableable, Sizable, WindowExt};
use threadlane_ui_state::AppState;

/// Open the pairing dialog; call from a click handler that owns a
/// `&mut Window`.
pub fn open_pairing_dialog(model: Entity<AppState>, window: &mut Window, cx: &mut App) {
    let view = cx.new(|cx| PairingDialogView::new(model, cx));
    window.open_dialog(cx, move |dialog, _, _| {
        dialog
            .title("Share with mobile")
            .w(px(400.))
            .child(view.clone())
    });
}

/// Dialog content: a live view over `AppState::pairing` so the QR appears
/// the moment the async listener bind resolves.
struct PairingDialogView {
    model: Entity<AppState>,
    /// Async bind in flight (token + listener not ready yet).
    starting: bool,
    _subscription: Subscription,
}

impl PairingDialogView {
    fn new(model: Entity<AppState>, cx: &mut Context<Self>) -> Self {
        let subscription = cx.observe(&model, |_, _, cx| cx.notify());
        Self {
            model,
            starting: false,
            _subscription: subscription,
        }
    }

    fn start(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let pending = self.model.update(cx, |state, _| state.start_pairing());
        match pending {
            Ok(task) => {
                self.starting = true;
                cx.notify();
                let model = self.model.clone();
                cx.spawn_in(window, async move |this, cx| {
                    let result = task.await;
                    let _ = this.update(cx, |this, cx| {
                        this.starting = false;
                        model.update(cx, |state, cx| {
                            match result {
                                Ok(Ok(server)) => state.pairing = Some(server),
                                Ok(Err(error)) => {
                                    state.pairing_error = Some(error);
                                }
                                Err(error) => {
                                    state.pairing_error =
                                        Some(format!("pairing listener failed: {error}"));
                                }
                            }
                            cx.notify();
                        });
                    });
                })
                .detach();
            }
            Err(error) => {
                self.model.update(cx, |state, cx| {
                    state.pairing_error = Some(error);
                    cx.notify();
                });
            }
        }
    }

    fn stop(&mut self, cx: &mut Context<Self>) {
        self.model.update(cx, |state, cx| {
            state.stop_pairing();
            cx.notify();
        });
    }
}

impl Render for PairingDialogView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let state = self.model.read(cx);
        let theme = cx.theme().colors;
        let pairing = state.pairing.as_ref().map(|server| server.info().clone());
        let pairing_error = state.pairing_error.clone();
        let starting = self.starting;
        drop(state);

        let mut content = v_flex().gap_3().text_sm();
        if let Some(info) = pairing {
            let uri = info.uri();
            content = content
                .child(
                    div()
                        .text_color(theme.muted_foreground)
                        .child("Scan with the Threadlane mobile app to watch this workspace's sessions live."),
                )
                .child(div().flex().justify_center().child(qr_grid(&uri)))
                .child(
                    v_flex()
                        .gap_1()
                        .child(
                            div()
                                .font_weight(FontWeight::SEMIBOLD)
                                .child(format!("{}:{}", info.host, info.port)),
                        )
                        .child(
                            Button::new("pairing-copy-uri")
                                .ghost()
                                .small()
                                .label("Copy pairing link")
                                .on_click(move |_, _, cx| {
                                    cx.write_to_clipboard(ClipboardItem::new_string(uri.clone()));
                                }),
                        ),
                )
                .child(
                    div()
                        .text_xs()
                        .text_color(theme.warning)
                        .child("Anyone on this network who scans the code can drive sessions until you stop sharing."),
                )
                .child(
                    Button::new("pairing-stop")
                        .danger()
                        .label("Stop sharing")
                        .on_click(cx.listener(|this, _, _, cx| this.stop(cx))),
                );
        } else {
            content = content.child(
                div().text_color(theme.muted_foreground).child(
                    "Share this workspace's sessions with the Threadlane mobile app on your \
                     local network. A QR code with a one-time credential is shown here; the \
                     listener stops when you stop sharing or quit.",
                ),
            );
            if let Some(error) = pairing_error {
                content = content.child(
                    div().text_xs().text_color(theme.danger).child(error),
                );
            }
            content = content.child(
                Button::new("pairing-start")
                    .primary()
                    .label(if starting {
                        "Starting listener…"
                    } else {
                        "Start sharing"
                    })
                    .disabled(starting)
                    .on_click(cx.listener(|this, _, window, cx| this.start(window, cx))),
            );
        }
        content
    }
}

/// QR cell size. A pairing URI is ~90 chars → a version ~5 code, 37×37
/// modules incl. quiet zone → about 220px square, which fits the dialog.
const QR_CELL: f32 = 5.0;

/// Render `data` as a QR bitmap made of GPUI divs — no image pipeline
/// needed and it rasterizes crisply at any display scale.
fn qr_grid(data: &str) -> AnyElement {
    let Ok(code) = qrcode::QrCode::new(data.as_bytes()) else {
        return div().child("Could not encode pairing link").into_any_element();
    };
    let colors = code.to_colors();
    let width = code.width();
    let cell = px(QR_CELL);
    div()
        .p_2()
        .bg(rgb(0xffffff))
        .rounded_md()
        .flex()
        .flex_col()
        .children((0..width).map(|y| {
            div()
                .flex()
                .children((0..width).map(|x| {
                    let dark = colors[y * width + x] == qrcode::Color::Dark;
                    div()
                        .w(cell)
                        .h(cell)
                        .when(dark, |cell| cell.bg(rgb(0x000000)))
                }))
        }))
        .into_any_element()
}
