use super::{tray, update, Orange, Screen};
use crate::{
    presence::Presence,
    sound,
    supervisor::{WindowTarget, FRAME_RATES, QUALITIES},
    ui::*,
    NoticeKind,
};

/// Indices into `Orange::settings_open`.
const SECTION_ACCOUNT: usize = 0;
const SECTION_STREAMING: usize = 1;
const SECTION_APPLICATION: usize = 2;
use gpui::{div, prelude::*, px, rgb, Context, FontWeight, SharedString, Window};
use std::time::Instant;

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
            Screen::Friends => "friends",
            Screen::Streaming => "streaming",
            Screen::Watching => "watching",
            Screen::Settings => "settings",
        }
    }

    fn breadcrumb(self) -> Option<&'static str> {
        match self {
            Screen::PickWindow => Some("/ SHARE"),
            Screen::Friends => Some("/ FRIENDS"),
            Screen::Streaming => Some("/ STREAMING"),
            Screen::Watching => Some("/ WATCHING"),
            Screen::Settings => Some("/ SETTINGS"),
            Screen::SignedOut | Screen::Home => None,
        }
    }
}

impl Render for Orange {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Decoration runs only while this window is the one you are looking
        // at. GPUI refreshes the window when activation changes, so reading it
        // here is enough to start and stop the ambient layer.
        self.animate = window.is_window_active();
        let animate = self.animate;

        let key = self.screen.animation_key();

        let body = match self.screen {
            Screen::SignedOut => self.render_signed_out(cx).into_any_element(),
            Screen::Home => self.render_home(cx).into_any_element(),
            Screen::PickWindow => self.render_pick(cx).into_any_element(),
            Screen::Friends => self.render_friends(cx).into_any_element(),
            Screen::Streaming => self.render_streaming(cx).into_any_element(),
            Screen::Watching => self.render_watching(cx).into_any_element(),
            Screen::Settings => self.render_settings(cx).into_any_element(),
        };
        let toasts = self.render_toasts(cx);

        div()
            .relative()
            .flex()
            .flex_col()
            .size_full()
            .bg(rgb(BG))
            .text_sm()
            .font_family("Segoe UI")
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
                    // its drift because you opened settings.
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

impl Orange {
    /// The floating toast layer.
    ///
    /// Absolutely positioned below the titlebar so a toast never reflows the
    /// screen underneath it. The update toast collapses but cannot be
    /// dismissed, because an available update stays actionable; the error toast
    /// closes, because a read error has no follow-up.
    fn render_toasts(&mut self, cx: &mut Context<Self>) -> Option<impl IntoElement> {
        let update = self.render_update_toast(cx);
        let error = self.render_error_toast(cx);
        if update.is_none() && error.is_none() {
            // Nothing to show: render no layer at all, rather than an empty one
            // whose padding would sit over the top of the body.
            return None;
        }
        Some(
            div()
                .absolute()
                .top(px(TITLEBAR_HEIGHT))
                .left_0()
                .w_full()
                .px_5()
                .pt_3()
                .flex()
                .flex_col()
                .gap_2()
                .children(update)
                .children(error),
        )
    }

    fn render_error_toast(&mut self, cx: &mut Context<Self>) -> Option<gpui::AnyElement> {
        let notice = self.notice.as_ref()?;
        // A failure is tinted red; an ordinary fact borrows the same shape in
        // the app's own colours. The shape is shared because the position and
        // the dismiss control are what make it recognisable as a notice - the
        // colour is the only thing carrying "and this one is bad".
        let failed = notice.kind == NoticeKind::Failure;
        let (background, border, text) = if failed {
            (DANGER_WASH, DANGER_EDGE, DANGER)
        } else {
            (SURFACE, BORDER, MUTED)
        };
        Some(
            appear(
                "error",
                motion::ENTER,
                div()
                    .flex()
                    .items_start()
                    .justify_between()
                    .gap_3()
                    .p_3()
                    .rounded_md()
                    .bg(rgb(background))
                    .border_1()
                    .border_color(rgb(border))
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.0))
                            .text_xs()
                            .text_color(rgb(text))
                            .child(notice.text.clone()),
                    )
                    .child(
                        toast_toggle("dismiss-error", "\u{2715}").on_click(cx.listener(
                            |this, _, _, cx| {
                                this.clear_error();
                                cx.notify();
                            },
                        )),
                    ),
            )
            .into_any_element(),
        )
    }

    fn render_update_toast(&mut self, cx: &mut Context<Self>) -> Option<gpui::AnyElement> {
        if !self.updates.status().is_visible() {
            return None;
        }
        let action = self.updates.status().action_label();
        let (heading, detail, action) = match self.updates.status() {
            update::UpdateStatus::Available(info) => (
                format!("UPDATE {} AVAILABLE", info.version),
                if info.notes.is_empty() {
                    "A new beta build is ready.".to_string()
                } else {
                    info.notes.clone()
                },
                action,
            ),
            update::UpdateStatus::Downloading(info) => (
                format!("DOWNLOADING {}", info.version),
                "Orange will restart when the verified installer is ready.".to_string(),
                None,
            ),
            update::UpdateStatus::Failed { message, .. } => (
                "UPDATE PAUSED".to_string(),
                message.clone(),
                self.updates.status().action_label(),
            ),
            _ => return None,
        };
        let collapsed = self.update_collapsed;
        Some(
            div()
                .flex()
                .items_center()
                .justify_between()
                .gap_3()
                .p_3()
                .rounded_md()
                .bg(rgb(ORANGE_WASH))
                .border_1()
                .border_color(rgb(ORANGE_DIM))
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .gap_0p5()
                        .flex_1()
                        .min_w(px(0.0))
                        .child(micro(heading, ORANGE))
                        .when(!collapsed, |element| {
                            element.child(label(detail, MUTED).text_xs())
                        }),
                )
                .children(action.filter(|_| !collapsed).map(|text| {
                    update_action("apply-update", text).on_click(cx.listener(|this, _, _, cx| {
                        this.updates.request_update();
                        cx.notify();
                    }))
                }))
                .child(
                    toast_toggle(
                        "toggle-update",
                        if collapsed { "\u{25be}" } else { "\u{25b4}" },
                    )
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.update_collapsed = !this.update_collapsed;
                        cx.notify();
                    })),
                )
                .into_any_element(),
        )
    }

    /// Custom titlebar. GPUI hides the system one via `appears_transparent`,
    /// which its source documents as supported on Windows.
    ///
    /// Dragging needs `window_control_area(Drag)` rather than a mouse handler:
    /// a borderless window is moved by the OS through hit-testing, so the
    /// draggable regions have to be declared. The buttons are deliberately
    /// left out of those regions, or the hit test would swallow their clicks.
    fn render_titlebar(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let breadcrumb = self.screen.breadcrumb();

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
                    // while the app keeps running in the tray. Minimise is a
                    // normal minimise; the two should not do the same thing.
                    .child(
                        titlebar_button("close", "×", DANGER_HOVER).on_click(cx.listener(
                            |this, _, _, cx| {
                                if this.screen == Screen::PickWindow {
                                    this.leave_picker(Screen::Home);
                                }
                                if this.tray_available {
                                    tray::hide_main_window();
                                } else {
                                    cx.quit();
                                }
                            },
                        )),
                    ),
            )
    }
}

impl Orange {
    fn render_signed_out(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let animate = self.animate;
        div()
            .flex()
            .flex_col()
            .gap_5()
            .flex_1()
            .justify_center()
            .items_center()
            .px_2()
            // The mark reports the wait, so "Waiting for Discord…" on the
            // button is not the only thing saying anything is happening.
            .child(logo(
                MARK_BRAND,
                if self.logging_in.is_some() {
                    LogoState::Loading
                } else {
                    LogoState::Idle
                },
                self.logo_epoch,
                animate,
            ))
            .child(wordmark(20.0))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_1p5()
                    .items_center()
                    .child(heading("Share games directly with friends", 20.0))
                    .child(
                        div()
                            .max_w(px(260.0))
                            .text_center()
                            .text_xs()
                            .text_color(rgb(MUTED))
                            .child(
                                "High bitrate, low overhead, straight to your friends. \
                                 Sign in so people know whose stream they are opening.",
                            ),
                    ),
            )
            .child(
                div().w_full().max_w(px(280.0)).child(
                    primary(
                        "signin",
                        if self.logging_in.is_some() {
                            "Waiting for Discord…"
                        } else {
                            "Sign in with Discord"
                        },
                        false,
                    )
                    .when(self.logging_in.is_some(), |d| d.bg(rgb(ORANGE_DIM)))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.start_login();
                        cx.notify();
                    })),
                ),
            )
            .children(
                self.logging_in.is_some().then(|| {
                    label("Finish in your browser, then come back here.", FAINT).text_xs()
                }),
            )
            // Identity is optional in the protocol - it only attaches a name.
            // Blocking streaming behind it would be a self-imposed limit.
            .child(
                quiet("skip", "Continue without signing in").on_click(cx.listener(
                    |this, _, _, cx| {
                        this.logging_in = None;
                        this.screen = Screen::Home;
                        cx.notify();
                    },
                )),
            )
    }

    fn render_home(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let animate = self.animate;
        let hosting = self.host.is_some();
        let watching = !self.watches.is_empty();
        let defaults = format!(
            "{} · {} FPS",
            self.quality().label.to_ascii_uppercase(),
            self.fps
        );
        let user_name = self
            .session
            .as_ref()
            .map(|session| session.name.clone())
            .unwrap_or_else(|| "Anonymous".into());
        let avatar_image = self.avatar.clone();

        div()
            .flex()
            .flex_col()
            .gap_4()
            .flex_1()
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .flex_shrink_0()
                    .child(dot(ORANGE))
                    .child(micro("READY TO STREAM", ORANGE))
                    .child(div().flex_1().h(px(1.0)).bg(rgb(BORDER)))
                    .child(micro(defaults, MUTED)),
            )
            .child(
                // The hero. Framed rather than floating: the brackets and the
                // edge marks are what stop a centred logo on a dark field
                // reading as an empty screen that has not loaded yet.
                div()
                    .relative()
                    .flex()
                    .flex_1()
                    .min_h(px(0.0))
                    .items_center()
                    .justify_center()
                    .child(corner_brackets(14.0, FRAME))
                    .child(div().absolute().left(px(0.0)).child(crosshair(9.0, FRAME)))
                    .child(div().absolute().right(px(0.0)).child(crosshair(9.0, FRAME)))
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .items_center()
                            .gap_2()
                            .child(logo(
                                MARK_HERO,
                                if hosting {
                                    LogoState::Live
                                } else {
                                    LogoState::Idle
                                },
                                self.logo_epoch,
                                animate,
                            ))
                            .child(wordmark(23.0))
                            .child(accent_rule(28.0))
                            .child(micro("STREAM.  SHARE.  CONNECT.", MUTED)),
                    ),
            )
            .child(
                action_card(
                    "start",
                    broadcast_mark(20.0, ORANGE),
                    if hosting {
                        "VIEW ACTIVE STREAM"
                    } else {
                        "START STREAMING"
                    },
                    if hosting {
                        "Your stream is running now."
                    } else {
                        "Go live and share your game, desktop or app."
                    },
                    true,
                )
                .on_click(cx.listener(move |this, _, _, cx| {
                    if hosting {
                        this.screen = Screen::Streaming;
                    } else {
                        this.refresh_windows();
                        this.screen = Screen::PickWindow;
                    }
                    cx.notify();
                })),
            )
            .child(
                action_card(
                    "join",
                    people_mark(20.0, if watching { SUCCESS } else { MUTED }),
                    if watching {
                        "VIEW ACTIVE STREAMS"
                    } else {
                        "FRIENDS"
                    },
                    if watching {
                        "Manage your open viewer windows."
                    } else {
                        "See who is streaming and join in one click."
                    },
                    false,
                )
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.screen = if watching {
                        Screen::Watching
                    } else {
                        Screen::Friends
                    };
                    cx.notify();
                })),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .gap_3()
                    .flex_shrink_0()
                    .pt_3()
                    .border_t_1()
                    .border_color(rgb(BORDER))
                    .child(identity(avatar_image, user_name, "SIGNED IN AS"))
                    .child(
                        ghost("signout", "Sign out").on_click(cx.listener(|this, _, _, cx| {
                            this.sign_out(Some(Screen::SignedOut));
                            cx.notify();
                        })),
                    ),
            )
    }

    fn render_pick(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let selected = self.quality;
        // Distinguishes "still capturing" from "this window refuses to draw",
        // which previously both showed as "no preview" and made every card
        // flash a failure message before its thumbnail arrived.
        let capturing = self.thumbnail_job.is_some();

        // Sharing a display and sharing a window are different decisions - one
        // of them puts every notification you receive on the stream, along
        // with all system audio. They used to be the same kind of card in the
        // same grid, told apart only by their caption. The display is pulled
        // out above the grid so the choice is made before the scanning starts.
        let (displays, windows): (Vec<_>, Vec<_>) = self
            .windows
            .iter()
            .cloned()
            .partition(|target| target.hwnd == 0);
        let count = windows.len();

        // Last frame's scroll state, which is close enough for an edge that
        // only says "there is more".
        let fade = scroll_fade(&self.picker_scroll);

        div()
            .flex()
            .flex_col()
            .gap_3()
            .flex_1()
            .min_h(px(0.0))
            .child(
                div()
                    .flex()
                    .items_end()
                    .justify_between()
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap_0p5()
                            .child(micro(format!("{} SOURCES AVAILABLE", count), ORANGE))
                            .child(heading("Choose what to share", 14.0))
                            .child(
                                label("Click a preview to start streaming immediately.", FAINT)
                                    .text_xs(),
                            ),
                    )
                    .child(
                        ghost("refresh", "Refresh").on_click(cx.listener(|this, _, _, cx| {
                            this.refresh_windows();
                            cx.notify();
                        })),
                    ),
            )
            .children(
                displays
                    .into_iter()
                    .map(|target| self.display_row(target, capturing, cx))
                    .collect::<Vec<_>>(),
            )
            .child(
                // The scroll region and its fade share this box, so the fade
                // can sit on the bottom edge of the region rather than the
                // bottom of the screen, above the footer.
                div()
                    .relative()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_h(px(0.0))
                    .child(
                        div()
                            .id("windows")
                            .flex()
                            .flex_row()
                            .flex_wrap()
                            .items_start()
                            .gap_3()
                            .flex_1()
                            .min_h(px(0.0))
                            .overflow_y_scroll()
                            .track_scroll(&self.picker_scroll)
                            .when(count == 0, |d| {
                                d.child(
                                    card()
                                        .w_full()
                                        .items_center()
                                        .child(label("No windows found", MUTED).text_xs()),
                                )
                            })
                            .children(
                                windows
                                    .into_iter()
                                    .map(|target| self.window_card(target, capturing, cx))
                                    .collect::<Vec<_>>(),
                            ),
                    )
                    .children(fade),
            ) // Quality is a setting, not the task, so it sits in a footer rather
            // than competing with the grid for attention. One row, vertically
            // centred: the hint used to hang below and break the alignment.
            .child(
                div()
                    .flex()
                    .flex_shrink_0()
                    .items_center()
                    .justify_between()
                    .gap_4()
                    .pt_3()
                    .border_t_1()
                    .border_color(rgb(BORDER))
                    .child(
                        div().flex().items_center().gap_1p5().children(
                            QUALITIES
                                .iter()
                                .enumerate()
                                .map(|(index, q)| {
                                    option_pill(
                                        SharedString::from(format!("q{index}")),
                                        q.label,
                                        index == selected,
                                    )
                                    .on_click(cx.listener(
                                        move |this, _, _, cx| {
                                            this.quality = index;
                                            this.save_preferences();
                                            cx.notify();
                                        },
                                    ))
                                })
                                .collect::<Vec<_>>(),
                        ),
                    )
                    // No hint line beside the pills. "Streaming quality adjusts
                    // automatically" fitted the 576-wide picker and does not fit
                    // 440 of content: it pushed Back off the right edge, which
                    // left the titlebar as the only way out of this screen. The
                    // same sentence is the detail on Settings -> Resolution,
                    // where somebody actually choosing a quality will read it.
                    .child(
                        ghost("back", "← Back").on_click(cx.listener(|this, _, _, cx| {
                            this.leave_picker(Screen::Home);
                            cx.notify();
                        })),
                    ),
            )
    }

    /// The whole-display entry, pinned above the grid.
    ///
    /// A row rather than a card, because it is not one of the windows and
    /// should not be scanned as one. The preview is small for the same reason:
    /// nobody needs a thumbnail to recognise their own desktop, and the thing
    /// worth reading here is the warning about system audio.
    fn display_row(
        &self,
        target: WindowTarget,
        capturing: bool,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let thumb = self.thumbnails.get(&target.hwnd).cloned();
        let group = SharedString::from("display-row");
        let meta = if target.width > 0 && target.height > 0 {
            format!(
                "{}\u{d7}{} \u{b7} includes all system audio",
                target.width, target.height
            )
        } else {
            "Full display \u{b7} includes all system audio".to_string()
        };

        div()
            .id("display")
            .group(group.clone())
            .flex()
            .flex_row()
            .items_center()
            .flex_shrink_0()
            .gap_3()
            .p_2()
            .rounded_md()
            .bg(rgb(SURFACE))
            .border_1()
            .border_color(rgb(BORDER))
            .cursor_pointer()
            .hover(|s| s.bg(rgb(SURFACE_HOVER)).border_color(rgb(ORANGE)))
            .active(|s| s.bg(rgb(BG)).border_color(rgb(ORANGE_DIM)))
            .child(
                div()
                    .flex()
                    .flex_shrink_0()
                    .items_center()
                    .justify_center()
                    .h(px(SCREEN_THUMB_HEIGHT))
                    .w(px(SCREEN_THUMB_HEIGHT * 16.0 / 9.0))
                    .rounded_md()
                    .overflow_hidden()
                    .bg(rgb(INK))
                    .child(match (thumb, capturing) {
                        (Some(image), _) => gpui::img(image)
                            .h(px(SCREEN_THUMB_HEIGHT))
                            .into_any_element(),
                        (None, true) => micro("\u{2026}", FAINT).into_any_element(),
                        (None, false) => micro("NO PREVIEW", FAINT).into_any_element(),
                    }),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_0p5()
                    .flex_1()
                    .min_w(px(0.0))
                    .child(
                        div()
                            .text_xs()
                            .text_color(rgb(TEXT))
                            .group_hover(group.clone(), |s| s.text_color(rgb(ORANGE)))
                            .child(target.title.clone()),
                    )
                    .child(label(meta, FAINT).text_xs()),
            )
            .child(
                div()
                    .flex_shrink_0()
                    .opacity(0.0)
                    .group_hover(group, |s| s.opacity(1.0))
                    .child(go_badge()),
            )
            .on_click(cx.listener(move |this, _, _, cx| {
                this.start_stream(target.clone());
                cx.notify();
            }))
    }

    /// One card in the picker grid.
    ///
    /// Split out of render_pick so the grid and the card can be changed
    /// independently. The card is the part that gets iterated on.
    fn window_card(
        &self,
        target: WindowTarget,
        capturing: bool,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let title = if target.title.is_empty() {
            target.app_name()
        } else {
            target.title.clone()
        };
        let meta = format!(
            "{} \u{b7} {}\u{d7}{}",
            target.app_name(),
            target.width,
            target.height
        );
        let thumb = self.thumbnails.get(&target.hwnd).cloned();
        let hwnd = target.hwnd;
        let group = SharedString::from(format!("card-{hwnd}"));

        div()
            .id(SharedString::from(format!("w{hwnd}")))
            .group(group.clone())
            .relative()
            .flex()
            .flex_col()
            .flex_shrink_0()
            .w(px(PICKER_CARD_WIDTH))
            .h(px(PICKER_CARD_HEIGHT))
            .rounded_md()
            .overflow_hidden()
            .bg(rgb(SURFACE))
            .border_1()
            .border_color(rgb(BORDER))
            .cursor_pointer()
            .hover(|s| s.bg(rgb(SURFACE_HOVER)).border_color(rgb(ORANGE)))
            .active(|s| s.bg(rgb(BG)).border_color(rgb(ORANGE_DIM)))
            .child(
                div()
                    .absolute()
                    .top_0()
                    .left_0()
                    .w_full()
                    .h(px(2.0))
                    .bg(rgb(ORANGE))
                    .opacity(0.0)
                    .group_hover(group.clone(), |s| s.opacity(1.0)),
            )
            .child(
                // 16:9 preview. flex_shrink_0 is
                // load-bearing: as a flex item this
                // would otherwise be compressed to
                // nothing inside a scrolling parent.
                div()
                    .relative()
                    .flex_shrink_0()
                    .h(px(PICKER_PREVIEW_HEIGHT))
                    .w_full()
                    .flex()
                    .items_center()
                    .justify_center()
                    .bg(rgb(INK))
                    .overflow_hidden()
                    .child(match (thumb, capturing) {
                        (Some(image), _) => appear(
                            format!("fade{hwnd}"),
                            motion::ENTER,
                            gpui::img(image).h(px(PICKER_PREVIEW_HEIGHT)),
                        )
                        .into_any_element(),
                        (None, true) => label("capturing…", FAINT).text_xs().into_any_element(),
                        (None, false) => label("preview unavailable · click to share", FAINT)
                            .text_xs()
                            .into_any_element(),
                    })
                    // Over the preview rather than beside the title, where an
                    // arrow the size of the caption was easy to miss on the
                    // one card the cursor was actually on.
                    .child(
                        div()
                            .absolute()
                            .bottom(px(8.0))
                            .right(px(8.0))
                            .opacity(0.0)
                            .group_hover(group.clone(), |s| s.opacity(1.0))
                            .child(go_badge()),
                    ),
            )
            .child(
                // Fixed height keeps the grid even; a
                // two-line title would make rows ragged.
                div()
                    .flex()
                    .flex_col()
                    .justify_center()
                    .gap_0p5()
                    .flex_shrink_0()
                    .h(px(PICKER_DETAILS_HEIGHT))
                    .px_3()
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .w_full()
                            .overflow_hidden()
                            .whitespace_nowrap()
                            .text_ellipsis()
                            .text_xs()
                            .text_color(rgb(TEXT))
                            .group_hover(group.clone(), |s| s.text_color(rgb(ORANGE)))
                            .child(
                                div()
                                    .flex_1()
                                    .overflow_hidden()
                                    .whitespace_nowrap()
                                    .text_ellipsis()
                                    .child(title),
                            ),
                    )
                    .child(
                        div()
                            .w_full()
                            .overflow_hidden()
                            .whitespace_nowrap()
                            .text_ellipsis()
                            .text_xs()
                            .text_color(rgb(FAINT))
                            .child(meta),
                    ),
            )
            .on_click(cx.listener(move |this, _, _, cx| {
                this.start_stream(target.clone());
                cx.notify();
            }))
    }

    fn render_streaming(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let code = self.code();
        let viewers = self.viewers();
        let quality = self.quality();
        let source_name = self
            .active_target
            .as_ref()
            .map(|target| {
                if target.title.is_empty() {
                    target.app_name()
                } else {
                    target.title.clone()
                }
            })
            .unwrap_or_else(|| "Selected source".into());
        let stream_details = format!("{} · {} fps", quality.label, self.fps);
        let preview = self.active_preview.clone();
        let just_copied = self
            .copied_at
            .map(|t| t.elapsed() < crate::COPIED_FOR)
            .unwrap_or(false);

        div()
            .flex()
            .flex_col()
            .gap_3()
            .flex_1()
            .min_h(px(0.0))
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_1p5()
                            .child(if code.is_some() {
                                live_dot().into_any_element()
                            } else {
                                dot(MUTED).into_any_element()
                            })
                            .child(
                                label(
                                    if code.is_some() {
                                        "Streaming"
                                    } else {
                                        "Starting stream…"
                                    },
                                    TEXT,
                                )
                                .font_weight(FontWeight::SEMIBOLD),
                            ),
                    )
                    .child(micro(&stream_details, MUTED)),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .flex_shrink_0()
                    .rounded_md()
                    .overflow_hidden()
                    .bg(rgb(SURFACE))
                    .border_1()
                    .border_color(rgb(BORDER))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .justify_center()
                            .w_full()
                            .h(px(STREAM_PREVIEW_HEIGHT))
                            .flex_shrink_0()
                            .overflow_hidden()
                            .bg(rgb(BG))
                            .child(match preview {
                                // Height only, with the width left to follow.
                                // This was `w_full` with a fixed height, which
                                // stretched a 16:9 capture to the shape of the
                                // well and squashed the picture by a ninth.
                                Some(image) => gpui::img(image)
                                    .h(px(STREAM_PREVIEW_HEIGHT))
                                    .into_any_element(),
                                None => label("Source preview unavailable", FAINT)
                                    .text_xs()
                                    .into_any_element(),
                            }),
                    )
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .justify_between()
                            .gap_3()
                            .px_3()
                            .py_2()
                            .child(
                                div()
                                    .flex_1()
                                    .overflow_hidden()
                                    .whitespace_nowrap()
                                    .text_ellipsis()
                                    .child(label(source_name, TEXT).text_xs()),
                            )
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .child(micro("SOURCE PREVIEW", ORANGE)),
                            ),
                    ),
            )
            .child(match code.clone() {
                Some(code) => card()
                    .id("code")
                    .flex_row()
                    .flex_shrink_0()
                    .items_center()
                    .justify_between()
                    .py_3()
                    .cursor_pointer()
                    .hover(|s| s.bg(rgb(SURFACE_HOVER)).border_color(rgb(ORANGE_DIM)))
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap_0p5()
                            .child(label("Share code", TEXT).text_xs())
                            .child(
                                label(
                                    if just_copied {
                                        "Copied to clipboard"
                                    } else {
                                        "Click to copy again"
                                    },
                                    if just_copied { SUCCESS } else { FAINT },
                                )
                                .text_xs(),
                            ),
                    )
                    .child(
                        div()
                            .text_color(rgb(ORANGE))
                            .text_size(px(24.0))
                            .font_weight(FontWeight::BOLD)
                            .child(code.clone()),
                    )
                    .on_click(cx.listener(move |this, _, _, cx| {
                        cx.write_to_clipboard(gpui::ClipboardItem::new_string(code.clone()));
                        this.copied_at = Some(Instant::now());
                        cx.notify();
                    }))
                    .into_any_element(),
                None => card()
                    .items_center()
                    .py_3()
                    .child(label("Connecting…", MUTED).text_xs())
                    .into_any_element(),
            })
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_1p5()
                    .flex_1()
                    .min_h(px(0.0))
                    .child(
                        label(
                            if viewers.is_empty() {
                                "Nobody watching yet".to_string()
                            } else {
                                format!("{} watching", viewers.len())
                            },
                            MUTED,
                        )
                        .text_xs(),
                    )
                    .children(
                        viewers
                            .into_iter()
                            .map(|name| {
                                div()
                                    .flex()
                                    .items_center()
                                    .gap_2()
                                    .child(dot(ORANGE))
                                    .child(label(name, TEXT).text_xs())
                            })
                            .collect::<Vec<_>>(),
                    ),
            )
            .child(
                secondary("back-streaming", "Back").on_click(cx.listener(|this, _, _, cx| {
                    this.screen = Screen::Home;
                    cx.notify();
                })),
            )
            .child(
                secondary("stop", "Stop streaming")
                    .text_color(rgb(DANGER))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.stop_host();
                        // The cue lives here rather than inside stop_host,
                        // which is also how an update tears the session down on
                        // its way out. Quitting should not chime.
                        sound::play(sound::Cue::Ended);
                        cx.notify();
                    })),
            )
    }

    /// Who is streaming right now, and one click to join them.
    ///
    /// The roster is local (`preferences.json`); only presence comes from the
    /// relay, and only for friends who listed this user when they went live.
    /// A friend with no answer yet is rendered as unknown rather than offline,
    /// because "offline" is a claim the tray has not earned until a poll
    /// succeeds.
    fn render_friends(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let live = self
            .friends
            .iter()
            .filter(|friend| {
                matches!(
                    self.presence.get(&friend.id),
                    Some(Presence::Live { .. } | Presence::Full)
                )
            })
            .count();
        let polled = self.presence_error.is_none() && !self.presence.is_empty();

        let rows = self
            .friends
            .iter()
            .enumerate()
            .map(|(index, friend)| {
                let state = self.presence.get(&friend.id).cloned();
                let (status, status_color) = match (&state, polled) {
                    (Some(Presence::Live { .. }), _) => ("Streaming now", SUCCESS),
                    (Some(Presence::Full), _) => ("Stream is full", MUTED),
                    (Some(Presence::Offline), _) => ("Not streaming", FAINT),
                    (None, true) => ("Not streaming", FAINT),
                    (None, false) => ("Checking\u{2026}", FAINT),
                };
                let joinable = match &state {
                    Some(Presence::Live { code }) => Some(code.clone()),
                    _ => None,
                };
                let already_watching = joinable
                    .as_ref()
                    .is_some_and(|code| self.watches.iter().any(|watch| &watch.code == code));

                card()
                    .flex_row()
                    .items_center()
                    .justify_between()
                    .gap_3()
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2p5()
                            .min_w(px(0.0))
                            // Real Discord picture when one has been fetched;
                            // `avatar` falls back to the initial until then.
                            .child(avatar(
                                self.friend_avatars.get(&friend.id).cloned(),
                                &friend.name,
                                32.0,
                            ))
                            .child(
                                div()
                                    .flex()
                                    .flex_col()
                                    .gap_0p5()
                                    .min_w(px(0.0))
                                    .child(
                                        label(friend.name.clone(), TEXT)
                                            .font_weight(FontWeight::SEMIBOLD)
                                            .text_ellipsis(),
                                    )
                                    .child(
                                        div()
                                            .flex()
                                            .items_center()
                                            .gap_1p5()
                                            .child(
                                                if matches!(state, Some(Presence::Live { .. })) {
                                                    live_dot().into_any_element()
                                                } else {
                                                    dot(status_color).into_any_element()
                                                },
                                            )
                                            .child(label(status, status_color).text_xs()),
                                    ),
                            ),
                    )
                    .child(match (joinable, already_watching) {
                        (Some(_), true) => label("Watching", FAINT).text_xs().into_any_element(),
                        (Some(code), false) => div()
                            .id(SharedString::from(format!("join-friend-{index}")))
                            .flex_shrink_0()
                            .px_3()
                            .py_1p5()
                            .rounded_md()
                            .bg(rgb(ORANGE))
                            .text_color(rgb(INK))
                            .text_xs()
                            .font_weight(FontWeight::SEMIBOLD)
                            .cursor_pointer()
                            .hover(|style| style.bg(rgb(ORANGE_HOT)))
                            .child("Join")
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.join(code.clone());
                                cx.notify();
                            }))
                            .into_any_element(),
                        (None, _) => div().into_any_element(),
                    })
            })
            .collect::<Vec<_>>();

        let empty = card().py_3().items_center().child(
            div()
                .flex()
                .flex_col()
                .gap_1()
                .items_center()
                .child(label("No friends yet", MUTED).text_xs())
                .child(label(
                    "Add ids to \"friends\" in preferences.json for now.",
                    FAINT,
                ))
                .child(label("Then use Join with a code below.", FAINT).text_xs()),
        );

        div()
            .flex()
            .flex_col()
            .gap_3()
            .flex_1()
            .child(micro(
                if self.friends.is_empty() {
                    "NO FRIENDS ADDED".to_string()
                } else {
                    format!("{live} OF {} STREAMING", self.friends.len())
                },
                if live > 0 { SUCCESS } else { MUTED },
            ))
            .children(self.presence_error.clone().map(|error| {
                // A failed poll must not read as "nobody is live". The most
                // likely cause is a relay that forgot this session, which is
                // the signed-out case wearing a different hat.
                card()
                    .py_2()
                    .child(label("Could not reach the relay", DANGER).text_xs())
                    .child(label(error, FAINT).text_xs())
            }))
            .child(
                div()
                    .id("friend-list")
                    .flex()
                    .flex_col()
                    .gap_2()
                    .flex_1()
                    .min_h(px(0.0))
                    .overflow_y_scroll()
                    .when(self.friends.is_empty(), |list| list.child(empty))
                    .children(rows),
            )
            .child(
                secondary("join-code", "Join with a code").on_click(cx.listener(
                    |this, _, _, cx| {
                        // Kept alongside the friends list: adding a friend
                        // still starts with a pasted invite, and a relay
                        // running without Discord configured has no other way
                        // in at all.
                        let code = cx
                            .read_from_clipboard()
                            .and_then(|item| item.text())
                            .unwrap_or_default();
                        this.join(code);
                        cx.notify();
                    },
                )),
            )
            .child(
                ghost("friends-back", "Back").on_click(cx.listener(|this, _, _, cx| {
                    this.screen = Screen::Home;
                    cx.notify();
                })),
            )
    }

    fn render_watching(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let count = self.watches.len();
        let sessions = self
            .watches
            .iter()
            .enumerate()
            .map(|(index, watch)| {
                let code = watch.code.clone();
                card()
                    .flex_row()
                    .items_center()
                    .justify_between()
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap_0p5()
                            .child(label(code, TEXT).font_weight(FontWeight::SEMIBOLD))
                            .child(label("Open in its own viewer window", FAINT).text_xs()),
                    )
                    .child(
                        div()
                            .id(SharedString::from(format!("leave-watch-{index}")))
                            .text_xs()
                            .text_color(rgb(FAINT))
                            .cursor_pointer()
                            .hover(|style| style.text_color(rgb(DANGER)))
                            .child("Close")
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.stop_watch(index);
                                cx.notify();
                            })),
                    )
            })
            .collect::<Vec<_>>();

        div()
            .flex()
            .flex_col()
            .gap_3()
            .flex_1()
            .child(micro(
                format!(
                    "{} VIEWER WINDOW{} OPEN",
                    count,
                    if count == 1 { "" } else { "S" }
                ),
                SUCCESS,
            ))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_1p5()
                    .child(live_dot())
                    .child(label("Watching friends", TEXT).font_weight(FontWeight::SEMIBOLD)),
            )
            .child(
                div()
                    .id("watch-list")
                    .flex()
                    .flex_col()
                    .gap_2()
                    .flex_1()
                    .min_h(px(0.0))
                    .overflow_y_scroll()
                    .children(sessions),
            )
            .child(
                secondary("join-another", "Watch another stream").on_click(cx.listener(
                    |this, _, _, cx| {
                        let code = cx
                            .read_from_clipboard()
                            .and_then(|item| item.text())
                            .unwrap_or_default();
                        this.join(code);
                        cx.notify();
                    },
                )),
            )
            .child(
                secondary("back-watching", "Back").on_click(cx.listener(|this, _, _, cx| {
                    this.screen = Screen::Home;
                    cx.notify();
                })),
            )
            .child(
                secondary("leave-all", "Close all viewer windows")
                    .text_color(rgb(DANGER))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.stop_all_watches();
                        cx.notify();
                    })),
            )
    }

    fn settings_account_card(&mut self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let signed_in = self.session.as_ref().map(|s| s.name.clone());
        // The person is the content and the provider is the qualifier; the other
        // way round read like a list of connected services when there is one.
        let identity = match &signed_in {
            Some(name) => div()
                .flex()
                .items_center()
                .gap_2()
                .min_w(px(0.0))
                .child(avatar(self.avatar.clone(), name, 32.0))
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .gap_0p5()
                        .min_w(px(0.0))
                        .child(label(name.clone(), TEXT))
                        .child(label("Discord", MUTED).text_xs()),
                )
                .into_any_element(),
            None => div()
                .flex()
                .flex_col()
                .gap_0p5()
                .min_w(px(0.0))
                .child(label("Not signed in", TEXT))
                .child(label("Sign in with Discord to share", MUTED).text_xs())
                .into_any_element(),
        };

        card()
            .flex_shrink_0()
            .flex_row()
            .items_center()
            .justify_between()
            .gap_3()
            .child(identity)
            .child(match signed_in {
                Some(_) => ghost("so", "Sign out")
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.sign_out(None);
                        cx.notify();
                    }))
                    .into_any_element(),
                None => ghost("si", "Sign in")
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.screen = Screen::SignedOut;
                        cx.notify();
                    }))
                    .into_any_element(),
            })
            .into_any_element()
    }

    fn settings_resolution_card(&mut self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let selected = self.quality;
        let chosen = self.quality();
        let pills = QUALITIES
            .iter()
            .enumerate()
            .map(|(index, quality)| {
                option_pill(
                    SharedString::from(format!("settings-quality-{index}")),
                    quality.label,
                    index == selected,
                )
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.quality = index;
                    this.save_preferences();
                    cx.notify();
                }))
                .into_any_element()
            })
            .collect();

        setting_choice(
            "Resolution",
            Some(SharedString::from(format!(
                "{} \u{d7} {}",
                chosen.max_width, chosen.max_height
            ))),
            pills,
            chosen.detail,
        )
        .into_any_element()
    }

    fn settings_frame_rate_card(&mut self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let selected = self.fps;
        let pills = FRAME_RATES
            .iter()
            .map(|rate| {
                let fps = rate.fps;
                option_pill(
                    SharedString::from(format!("settings-fps-{fps}")),
                    rate.label,
                    fps == selected,
                )
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.fps = fps;
                    this.save_preferences();
                    cx.notify();
                }))
                .into_any_element()
            })
            .collect();

        let detail = FRAME_RATES
            .iter()
            .find(|rate| rate.fps == selected)
            .unwrap_or(&FRAME_RATES[0])
            .detail;

        // No readout: a readout shows what an abstract label resolves to, and
        // "60" is not abstract.
        setting_choice("Frame rate", None, pills, detail).into_any_element()
    }

    fn settings_updates_card(&mut self, cx: &mut Context<Self>) -> gpui::AnyElement {
        // The action is always rendered so the row does not reflow while a
        // check runs; dimmed and inert when nothing applies. The label follows
        // the state: an available update installs, anything else checks.
        let action = self.updates.settings_action();
        let button = quiet("check-updates", action.unwrap_or("Check now"));
        let action = if action.is_some() {
            button
                .on_click(cx.listener(|this, _, _, cx| {
                    this.updates.activate_settings_action();
                    cx.notify();
                }))
                .into_any_element()
        } else {
            // Dimmed by colour, not opacity. Opacity 0.35 over already-dim text
            // landed near 1.6:1, so the row read as having no action at all -
            // the opposite of reserving its space.
            button
                .text_color(rgb(FAINT))
                .border_color(rgb(BORDER_DIM))
                .cursor_default()
                .into_any_element()
        };
        setting_row("Updates", self.updates.settings_detail(), action).into_any_element()
    }

    fn settings_diagnostics_card(&mut self, cx: &mut Context<Self>) -> gpui::AnyElement {
        setting_row(
            "Diagnostics",
            "Logs from your recent sessions",
            quiet("open-diagnostics", "Open folder")
                .on_click(cx.listener(|this, _, _, cx| {
                    this.open_diagnostics();
                    cx.notify();
                }))
                .into_any_element(),
        )
        .into_any_element()
    }

    /// A heading plus its cards, folded away when the heading is clicked.
    fn settings_section(
        &mut self,
        index: usize,
        id: &'static str,
        title: &'static str,
        readout: Option<&'static str>,
        cards: Vec<gpui::AnyElement>,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let open = self.settings_open[index];
        div()
            .flex()
            .flex_col()
            .flex_shrink_0()
            .gap_3()
            .child(
                section_header(id, title, readout, open).on_click(cx.listener(
                    move |this, _, _, cx| {
                        this.settings_open[index] = !this.settings_open[index];
                        cx.notify();
                    },
                )),
            )
            // Keyed on the open state so re-expanding replays the fade rather
            // than reusing an animation that already finished.
            .children(open.then(|| section_body(id, cards)))
            .into_any_element()
    }

    fn render_settings(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let fade = scroll_fade(&self.settings_scroll);
        let account = self.settings_account_card(cx);
        let resolution = self.settings_resolution_card(cx);
        let frame_rate = self.settings_frame_rate_card(cx);
        let updates = self.settings_updates_card(cx);
        let diagnostics = self.settings_diagnostics_card(cx);

        let account = self.settings_section(
            SECTION_ACCOUNT,
            "section-account",
            "ACCOUNT",
            None,
            vec![account],
            cx,
        );
        // VIDEO, not STREAMING: the titlebar already uses "/ STREAMING" to mean
        // a stream is live right now, and this section configures none of that.
        // DEFAULTS, because resolution can still be changed in the picker at
        // share time - without saying so, someone who sets 1080p here and sees
        // something else there concludes the setting is broken.
        let video = self.settings_section(
            SECTION_STREAMING,
            "section-video",
            "VIDEO",
            Some("DEFAULTS"),
            vec![resolution, frame_rate],
            cx,
        );
        let system = self.settings_section(
            SECTION_APPLICATION,
            "section-system",
            "SYSTEM",
            None,
            vec![updates, diagnostics],
            cx,
        );

        div()
            .flex()
            .flex_col()
            .gap_3()
            .flex_1()
            .min_h(px(0.0))
            .child(
                // The list outgrew the window once Updates and Diagnostics were
                // added. Scroll the list and pin the footer so Done is always
                // reachable without scrolling to find it.
                //
                // The scroll region and its edge share this box so the fade
                // lands on the bottom of the list, not on the footer below it.
                div()
                    .relative()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_h(px(0.0))
                    .child(
                        div()
                            .id("settings-scroll")
                            .flex()
                            .flex_col()
                            .gap_4()
                            .flex_1()
                            .min_h(px(0.0))
                            .overflow_y_scroll()
                            .track_scroll(&self.settings_scroll)
                            .child(account)
                            .child(video)
                            .child(system),
                    )
                    .children(fade),
            )
            .child(micro(
                format!(
                    "VERSION {}  ·  {}",
                    update::current_version(),
                    update::build_label()
                ),
                FAINT,
            ))
            .child(
                ghost("back-settings", "Done")
                    .w_full()
                    .h(px(44.0))
                    .text_size(px(13.0))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.screen = Screen::Home;
                        cx.notify();
                    })),
            )
    }
}
