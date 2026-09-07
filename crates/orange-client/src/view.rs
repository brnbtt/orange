//! The screen layer.
//!
//! This file owns only what every screen shares: the frame, the entry
//! animation, and the dispatch from [`Screen`] to the renderer that draws it.
//! Each screen lives in its own submodule and adds its renderers to `Orange`
//! there, so a change to one screen touches one file:
//!
//! - [`chrome`] — the titlebar and its window controls.
//! - [`toast`] — the floating notice layer above every screen.
//! - [`home`] — signed out, and home with the friends list.
//! - [`pick`] — the share picker.
//! - [`stream`] — hosting a stream, and watching one.
//! - [`settings`] — the settings sections and cards.

mod chrome;
mod home;
mod pick;
mod requests;
mod settings;
mod stream;
mod toast;

use crate::{ui::*, Orange, Screen};

use gpui::{div, prelude::*, px, rgb, Context, Window};

/// Per-screen view metadata.
///
/// Every match here is exhaustive with no `_` arm on purpose. Adding a screen
/// should be a compile error in each of these, rather than a titlebar that
/// silently shows nothing.
impl Screen {
    /// Distinguishes screens for the entry animation, which restarts when this
    /// changes.
    fn animation_key(self) -> &'static str {
        match self {
            Screen::SignedOut => "signedout",
            Screen::Home => "home",
            Screen::PickWindow => "pick",
            Screen::Streaming => "streaming",
            Screen::Watching => "watching",
            Screen::Settings => "settings",
        }
    }

    fn breadcrumb(self) -> Option<&'static str> {
        match self {
            Screen::PickWindow => Some("/ SHARE"),
            Screen::Streaming => Some("/ STREAMING"),
            Screen::Watching => Some("/ WATCHING"),
            Screen::Settings => Some("/ SETTINGS"),
            Screen::SignedOut | Screen::Home => None,
        }
    }
}

impl Render for Orange {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let root_focus = self
            .root_focus
            .get_or_insert_with(|| cx.focus_handle().tab_stop(false))
            .clone();
        if window.focused(cx).is_none() {
            root_focus.focus(window);
        }
        // Decoration runs only while this window is the one you are looking
        // at. GPUI refreshes the window when activation changes, so reading it
        // here is enough to start and stop the ambient layer.
        self.animate = window.is_window_active();
        let animate = self.animate;
        self.dismiss_friend_menu_for_state();
        self.dismiss_friend_menu_if_focus_left(window, cx);

        let key = self.screen.animation_key();

        let body = match self.screen {
            Screen::SignedOut => self.render_signed_out(cx).into_any_element(),
            Screen::Home => self.render_home(cx).into_any_element(),
            Screen::PickWindow => self.render_pick(cx).into_any_element(),
            Screen::Streaming => self.render_streaming(cx).into_any_element(),
            Screen::Watching => self.render_watching(cx).into_any_element(),
            Screen::Settings => self.render_settings(cx).into_any_element(),
        };
        let toasts = self.render_toasts(cx);

        div()
            .id("orange-app")
            .track_focus(&root_focus)
            // This focus handle is a keyboard fallback. Child controls have
            // already handled the click; blank space must not steal focus.
            .on_any_mouse_down(|_, window, _| window.prevent_default())
            .relative()
            .flex()
            .flex_col()
            .size_full()
            .bg(rgb(BG))
            .text_sm()
            .font_family("Segoe UI")
            // GPUI records tab stops but does not bind Tab itself. Without
            // this, the options button could only gain focus with the mouse.
            .on_key_down(cx.listener(|this, event: &gpui::KeyDownEvent, window, cx| {
                let modifiers = event.keystroke.modifiers;
                if event.keystroke.key == "tab"
                    && !modifiers.control
                    && !modifiers.alt
                    && !modifiers.platform
                {
                    this.close_friend_menu_and_restore_focus(window);
                    if modifiers.shift {
                        window.focus_prev();
                    } else {
                        window.focus_next();
                    }
                    cx.stop_propagation();
                    cx.notify();
                }
            }))
            .child(self.render_titlebar(cx))
            .child(
                div()
                    .relative()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_h(px(0.0))
                    // 20px gutters, 16 top, 16 bottom.
                    .px_5()
                    .pt_4()
                    .pb_4()
                    .gap_4()
                    // Behind everything, and first, so it never takes a hit
                    // test. One instance for the whole app rather than one per
                    // screen: it is the room, and the room does not restart
                    // its lighting cycle because you opened settings.
                    .child(grid(animate))
                    .child(
                        div()
                            .relative()
                            .flex()
                            .flex_col()
                            .flex_1()
                            .min_h(px(0.0))
                            .child(fade_in(key, body)),
                    ),
            )
            // Last child, so it paints over the body.
            .children(toasts)
    }
}
