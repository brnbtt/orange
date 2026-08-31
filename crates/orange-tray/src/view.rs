use super::{tray, update, Orange, Screen};
use crate::{
    supervisor::{WindowTarget, QUALITIES},
    ui::*,
};
use gpui::{
    div, prelude::*, px, rgb, size, Animation, AnimationExt, Context, FontWeight, Pixels,
    SharedString, Size, Window,
};
use std::time::{Duration, Instant};

/// Per-screen view metadata.
///
/// Every match here is exhaustive with no `_` arm on purpose. Adding a screen
/// should be a compile error in each of these, not a window that silently opens
/// at the wrong size or a titlebar that silently shows nothing.
impl Screen {
    /// The picker needs room for a two-column grid; every other screen is a
    /// narrow column. Resizing on transition keeps both comfortable rather
    /// than compromising on one size for all of them.
    fn size(self) -> Size<Pixels> {
        match self {
            Screen::PickWindow => size(px(576.0), px(660.0)),
            Screen::Streaming => size(px(480.0), px(640.0)),
            Screen::SignedOut | Screen::Home | Screen::Watching | Screen::Settings => {
                size(px(400.0), px(540.0))
            }
        }
    }

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
        let update_visible = self.updates.status().is_visible();
        let mut wanted = self.screen.size();
        if update_visible {
            wanted.height += px(84.0);
        }
        if self.sized_for != Some((self.screen, update_visible)) {
            self.sized_for = Some((self.screen, update_visible));
            window.resize(wanted);
        }

        let key = self.screen.animation_key();

        let body = match self.screen {
            Screen::SignedOut => self.render_signed_out(cx).into_any_element(),
            Screen::Home => self.render_home(cx).into_any_element(),
            Screen::PickWindow => self.render_pick(cx).into_any_element(),
            Screen::Streaming => self.render_streaming(cx).into_any_element(),
            Screen::Watching => self.render_watching(cx).into_any_element(),
            Screen::Settings => self.render_settings(cx).into_any_element(),
        };
        let update_banner = self.render_update_banner(cx);

        div()
            .flex()
            .flex_col()
            .size_full()
            .bg(rgb(BG))
            .text_sm()
            .font_family("Segoe UI")
            .child(self.render_titlebar(cx))
            .children(update_banner)
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
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .flex_1()
                            .min_h(px(0.0))
                            .child(fade_in(key, body)),
                    )
                    .children(self.notice.as_ref().map(|notice| {
                        div()
                            .absolute()
                            .top_0()
                            .left_0()
                            .w_full()
                            .p_3()
                            .rounded_md()
                            .bg(rgb(0x241514))
                            .border_1()
                            .border_color(rgb(0x3d211f))
                            .text_xs()
                            .text_color(rgb(DANGER))
                            .child(notice.text.clone())
                            .with_animation(
                                SharedString::from("error"),
                                Animation::new(Duration::from_millis(160)),
                                |element, delta| element.opacity(delta),
                            )
                    })),
            )
    }
}

impl Orange {
    fn render_update_banner(&mut self, cx: &mut Context<Self>) -> Option<gpui::AnyElement> {
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
        Some(
            div()
                .mx_5()
                .mt_3()
                .flex()
                .items_center()
                .justify_between()
                .gap_3()
                .p_3()
                .rounded_md()
                .bg(rgb(0x24190f))
                .border_1()
                .border_color(rgb(ORANGE_DIM))
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .gap_0p5()
                        .min_w(px(0.0))
                        .child(micro(heading, ORANGE))
                        .child(label(detail, MUTED).text_xs()),
                )
                .children(action.map(|text| {
                    update_action("apply-update", text).on_click(cx.listener(|this, _, _, cx| {
                        this.updates.request_update();
                        cx.notify();
                    }))
                }))
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
            .h(px(44.0))
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
                    .child(logo(18.0, 0))
                    .child(
                        label("O R A N G E", TEXT)
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
                        titlebar_button("close", "×", 0x8c2b28).on_click(cx.listener(
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
        div()
            .flex()
            .flex_col()
            .gap_5()
            .flex_1()
            .justify_center()
            .items_center()
            .px_2()
            .child(logo(56.0, self.logo_epoch))
            .child(wordmark(20.0))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_1p5()
                    .items_center()
                    .child(
                        label("Share games directly with friends", TEXT)
                            .font_family("Bahnschrift")
                            .text_xl()
                            .font_weight(FontWeight::SEMIBOLD),
                    )
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
                div().w_full().max_w(px(260.0)).child(
                    primary(
                        "signin",
                        if self.logging_in.is_some() {
                            "Waiting for Discord…"
                        } else {
                            "Sign in with Discord"
                        },
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
        let hosting = self.host.is_some();
        let defaults = format!(
            "{} · {}",
            self.quality().label.to_ascii_uppercase(),
            self.fps
                .map(|fps| format!("{fps} FPS"))
                .unwrap_or_else(|| "AUTO FPS".into())
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
                    .child(micro("READY TO STREAM", GREEN))
                    .child(div().flex_1().h(px(1.0)).bg(rgb(BORDER)))
                    .child(micro(defaults, MUTED)),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .items_center()
                    .justify_center()
                    .gap_3()
                    .child(logo(112.0, self.logo_epoch))
                    .child(wordmark(23.0))
                    .child(accent_rule(28.0))
                    .child(
                        label("Pick a window, share the code, keep playing.", MUTED)
                            .font_family("Cascadia Mono")
                            .text_size(px(10.0)),
                    ),
            )
            .child(
                primary(
                    "start",
                    if hosting {
                        "View active stream"
                    } else {
                        "Start streaming"
                    },
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
                secondary("join", "Join a stream").on_click(cx.listener(|this, _, _, cx| {
                    // A proper text field is deferred; the code is always
                    // copied from Discord anyway, so paste is the flow.
                    let code = cx
                        .read_from_clipboard()
                        .and_then(|item| item.text())
                        .unwrap_or_default();
                    this.join(code);
                    cx.notify();
                })),
            )
            .child(label("Paste a code first — it joins from your clipboard.", FAINT).text_xs())
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .pt_3()
                    .border_t_1()
                    .border_color(rgb(BORDER))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .child(avatar(avatar_image, &user_name, 26.0))
                            .child(
                                div()
                                    .flex()
                                    .flex_col()
                                    .gap_0p5()
                                    .child(micro("SIGNED IN AS", FAINT))
                                    .child(label(user_name, TEXT).text_xs()),
                            ),
                    )
                    .child(
                        quiet("signout", "Sign out").on_click(cx.listener(|this, _, _, cx| {
                            this.sign_out(Some(Screen::SignedOut));
                            cx.notify();
                        })),
                    ),
            )
    }

    fn render_pick(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let quality = self.quality();
        let selected = self.quality;
        let count = self.windows.len();
        // Distinguishes "still capturing" from "this window refuses to draw",
        // which previously both showed as "no preview" and made every card
        // flash a failure message before its thumbnail arrived.
        let capturing = self.thumbnail_job.is_some();

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
                            .child(
                                label("Choose what to share", TEXT)
                                    .font_family("Bahnschrift")
                                    .font_weight(FontWeight::SEMIBOLD),
                            )
                            .child(
                                label("Click a preview to start streaming immediately.", FAINT)
                                    .text_xs(),
                            ),
                    )
                    .child(
                        quiet("refresh", "Refresh").on_click(cx.listener(|this, _, _, cx| {
                            this.refresh_windows();
                            cx.notify();
                        })),
                    ),
            )
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
                    .when(count == 0, |d| {
                        d.child(
                            card()
                                .w_full()
                                .items_center()
                                .child(label("No windows found", MUTED).text_xs()),
                        )
                    })
                    .children(
                        self.windows
                            .clone()
                            .into_iter()
                            .map(|target| self.window_card(target, capturing, cx))
                            .collect::<Vec<_>>(),
                    ),
            )
            // Quality is a setting, not the task, so it sits in a footer rather
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
                        div()
                            .flex()
                            .items_center()
                            .gap_3()
                            .child(
                                div().flex().gap_1p5().children(
                                    QUALITIES
                                        .iter()
                                        .enumerate()
                                        .map(|(index, q)| {
                                            let active = index == selected;
                                            div()
                                                .id(SharedString::from(format!("q{index}")))
                                                .px_3()
                                                .py_1()
                                                .rounded_md()
                                                .text_xs()
                                                .cursor_pointer()
                                                .border_1()
                                                .border_color(rgb(if active {
                                                    ORANGE
                                                } else {
                                                    BORDER
                                                }))
                                                .bg(rgb(if active { ORANGE } else { SURFACE }))
                                                .text_color(rgb(if active { INK } else { MUTED }))
                                                .when(active, |d| {
                                                    d.font_weight(FontWeight::SEMIBOLD)
                                                })
                                                .when(!active, |d| {
                                                    d.hover(|s| {
                                                        s.bg(rgb(SURFACE_HOVER))
                                                            .text_color(rgb(TEXT))
                                                    })
                                                })
                                                .child(q.label)
                                                .on_click(cx.listener(move |this, _, _, cx| {
                                                    this.quality = index;
                                                    this.save_preferences();
                                                    cx.notify();
                                                }))
                                        })
                                        .collect::<Vec<_>>(),
                                ),
                            )
                            .child(
                                label(format!("~{} Mbps per viewer", quality.mbps), FAINT)
                                    .text_xs(),
                            ),
                    )
                    .child(
                        quiet("back", "← Back").on_click(cx.listener(|this, _, _, cx| {
                            this.leave_picker(Screen::Home);
                            cx.notify();
                        })),
                    ),
            )
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
        let is_screen = target.hwnd == 0;
        let title = if target.title.is_empty() {
            target.app_name()
        } else {
            target.title.clone()
        };
        let meta = if is_screen {
            "Full display · includes all system audio".to_string()
        } else {
            format!("{} · {}×{}", target.app_name(), target.width, target.height)
        };
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
            .w(px(252.0))
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
                    .flex_shrink_0()
                    .h(px(PICKER_PREVIEW_HEIGHT))
                    .w_full()
                    .flex()
                    .items_center()
                    .justify_center()
                    .bg(rgb(0x08070a))
                    .overflow_hidden()
                    .child(match (thumb, capturing) {
                        (Some(image), _) => gpui::img(image)
                            .h(px(PICKER_PREVIEW_HEIGHT))
                            .with_animation(
                                SharedString::from(format!("fade{hwnd}")),
                                Animation::new(Duration::from_millis(260)),
                                |el, delta| el.opacity(delta),
                            )
                            .into_any_element(),
                        (None, true) => label("capturing…", FAINT).text_xs().into_any_element(),
                        (None, false) => label("preview unavailable · click to share", FAINT)
                            .text_xs()
                            .into_any_element(),
                    }),
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
                            )
                            .child(
                                micro("→", ORANGE)
                                    .opacity(0.0)
                                    .group_hover(group.clone(), |s| s.opacity(1.0)),
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
        let stream_details = format!(
            "{} · {}",
            quality.label,
            self.fps
                .map(|fps| format!("{fps} fps"))
                .unwrap_or_else(|| "display refresh".into())
        );
        let preview = self.active_preview.clone();
        let just_copied = self
            .copied_at
            .map(|t| t.elapsed() < Duration::from_secs(2))
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
                            .h(px(220.0))
                            .flex_shrink_0()
                            .overflow_hidden()
                            .bg(rgb(BG))
                            .child(match preview {
                                Some(image) => {
                                    gpui::img(image).w_full().h(px(220.0)).into_any_element()
                                }
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
                                    if just_copied { GREEN } else { FAINT },
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
                GREEN,
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
                secondary("leave-all", "Close all viewer windows")
                    .text_color(rgb(DANGER))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.stop_all_watches();
                        cx.notify();
                    })),
            )
    }

    fn render_settings(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let signed_in = self.session.as_ref().map(|s| s.name.clone());
        let selected_quality = self.quality;
        let selected_fps = self.fps;
        let identity = match &signed_in {
            Some(name) => div()
                .flex()
                .items_center()
                .gap_2()
                .child(avatar(self.avatar.clone(), name, 32.0))
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .gap_0p5()
                        .child(label("Discord", TEXT))
                        .child(label(name.clone(), FAINT).text_xs()),
                )
                .into_any_element(),
            None => div()
                .flex()
                .flex_col()
                .gap_0p5()
                .child(label("Discord", TEXT))
                .child(label("Not signed in", FAINT).text_xs())
                .into_any_element(),
        };

        div()
            .flex()
            .flex_col()
            .gap_3()
            .flex_1()
            .min_h(px(0.0))
            .child(
                // The settings list outgrew the window once Updates and
                // Diagnostics were added. Scroll the list and pin the footer so
                // Done is always reachable without scrolling to find it.
                div()
                    .id("settings-scroll")
                    .flex()
                    .flex_col()
                    .gap_3()
                    .flex_1()
                    .min_h(px(0.0))
                    .overflow_y_scroll()
            .child(micro("ACCOUNT AND RELAY", ORANGE).flex_shrink_0())
            .child(
                card()
                    .flex_shrink_0()
                    .flex_row()
                    .items_center()
                    .justify_between()
                    .child(identity)
                    .child(match signed_in {
                        Some(_) => quiet("so", "Sign out")
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.sign_out(None);
                                cx.notify();
                            }))
                            .into_any_element(),
                        None => quiet("si", "Sign in")
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.screen = Screen::SignedOut;
                                cx.notify();
                            }))
                            .into_any_element(),
                    }),
            )
            .child(
                card()
                    .flex_shrink_0()
                    .gap_2()
                    .child(label("Default quality", TEXT))
                    .child(
                        div().flex().gap_1p5().children(
                            QUALITIES
                                .iter()
                                .enumerate()
                                .map(|(index, quality)| {
                                    option_pill(
                                        SharedString::from(format!("settings-quality-{index}")),
                                        quality.label,
                                        index == selected_quality,
                                    )
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.quality = index;
                                        this.save_preferences();
                                        cx.notify();
                                    }))
                                })
                                .collect::<Vec<_>>(),
                        ),
                    )
                    .child(
                        label("Used when a new stream starts. You can still change it before sharing.", FAINT)
                            .text_xs(),
                    ),
            )
            .child(
                card()
                    .flex_shrink_0()
                    .gap_2()
                    .child(label("Frame rate", TEXT))
                    .child(
                        div().flex().gap_1p5().children(
                            [
                                (None, "Auto"),
                                (Some(60), "60"),
                                (Some(120), "120"),
                                (Some(240), "240"),
                            ]
                            .into_iter()
                            .map(|(fps, text)| {
                                option_pill(
                                    SharedString::from(format!(
                                        "settings-fps-{}",
                                        fps.unwrap_or(0)
                                    )),
                                    text,
                                    fps == selected_fps,
                                )
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.fps = fps;
                                    this.save_preferences();
                                    cx.notify();
                                }))
                            })
                            .collect::<Vec<_>>(),
                        ),
                    )
                    .child(
                        label("Auto follows the refresh rate of the display being captured.", FAINT)
                            .text_xs(),
                    ),
            )
            .child(
                card()
                    .flex_shrink_0()
                    .flex_row()
                    .items_center()
                    .justify_between()
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap_0p5()
                            .min_w(px(0.0))
                            .child(label("Updates", TEXT))
                            .child(label(self.updates.settings_detail(), FAINT).text_xs()),
                    )
                    .child({
                        // Always rendered so the row does not reflow while a
                        // check runs; dimmed and inert when nothing applies.
                        // The label follows the state: an available update
                        // installs, anything else checks.
                        let action = self.updates.settings_action();
                        let button = quiet("check-updates", action.unwrap_or("Check now"));
                        if action.is_some() {
                            button
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.updates.activate_settings_action();
                                    cx.notify();
                                }))
                                .into_any_element()
                        } else {
                            button.opacity(0.35).cursor_default().into_any_element()
                        }
                    }),
            )
            .child(
                card()
                    .flex_shrink_0()
                    .flex_row()
                    .items_center()
                    .justify_between()
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap_0p5()
                            .child(label("Diagnostics", TEXT))
                            .child(
                                label("Session logs. Attach these when reporting a problem.", FAINT)
                                    .text_xs(),
                            ),
                    )
                    .child(quiet("open-diagnostics", "Open folder").on_click(cx.listener(
                        |this, _, _, cx| {
                            this.open_diagnostics();
                            cx.notify();
                        },
                    ))),
            ),
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
                secondary("back-settings", "Done").on_click(cx.listener(|this, _, _, cx| {
                    this.screen = Screen::Home;
                    cx.notify();
                })),
            )
    }
}
