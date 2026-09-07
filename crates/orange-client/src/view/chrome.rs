//! The window chrome: the custom titlebar and its window controls.

use crate::{client, ui::*, Orange, Screen};

use gpui::{div, prelude::*, px, rgb, Context, FontWeight};

impl Orange {
    /// Custom titlebar. GPUI hides the system one via `appears_transparent`,
    /// which its source documents as supported on Windows.
    ///
    /// Dragging needs `window_control_area(Drag)` rather than a mouse handler:
    /// a borderless window is moved by the OS through hit-testing, so the
    /// draggable regions have to be declared. The buttons are deliberately
    /// left out of those regions, or the hit test would swallow their clicks.
    pub(super) fn render_titlebar(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let breadcrumb = self.screen.breadcrumb(self.copy());

        div()
            .flex()
            .items_center()
            .h(px(TITLEBAR_HEIGHT))
            .pl_4()
            .pr_1()
            .border_b_1()
            .border_color(rgb(BORDER))
            .child(
                div()
                    .id("titlebar-brand")
                    .flex()
                    .items_center()
                    .gap_2()
                    .window_control_area(gpui::WindowControlArea::Drag)
                    .child(logo(MARK_TITLEBAR, LogoState::Idle, 0, false))
                    .child(
                        label(WORDMARK, TEXT)
                            .font_family("Bahnschrift")
                            .text_xs()
                            .font_weight(FontWeight::SEMIBOLD),
                    )
                    .children(breadcrumb.map(|text| micro(text, ORANGE_DIM))),
            )
            // The empty middle is draggable too, so the whole bar behaves as
            // people expect - except where the controls are.
            .child(
                div()
                    .id("titlebar-drag")
                    .flex_1()
                    .h_full()
                    .window_control_area(gpui::WindowControlArea::Drag),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .child(
                        titlebar_button("settings", "⚙︎", SURFACE_HOVER).on_click(cx.listener(
                            |this, _, _, cx| {
                                let destination = if this.screen == Screen::Settings {
                                    Screen::Home
                                } else {
                                    Screen::Settings
                                };
                                if this.screen == Screen::PickWindow {
                                    this.leave_picker(destination);
                                } else {
                                    this.screen = destination;
                                }
                                cx.notify();
                            },
                        )),
                    )
                    .child(titlebar_button("minimise", "−", SURFACE_HOVER).on_click(
                        |_, window, _| {
                            window.minimize_window();
                        },
                    ))
                    // Close hides the window completely - no taskbar entry -
                    // while the app keeps running in the client. Minimise is a
                    // normal minimise; the two should not do the same thing.
                    .child(
                        titlebar_button("close", "×", DANGER_HOVER).on_click(cx.listener(
                            |this, _, _, cx| {
                                if this.screen == Screen::PickWindow {
                                    this.leave_picker(Screen::Home);
                                }
                                if this.client_available {
                                    client::hide_main_window();
                                } else {
                                    cx.quit();
                                }
                            },
                        )),
                    ),
            )
    }
}
