//! The signed-out screen and the home screen with its friends list.

use crate::{
    presence::{Presence, PresenceError},
    ui::*,
    Orange, Screen,
};

use gpui::{
    anchored, deferred, div, prelude::*, px, rgb, AnchoredPositionMode, ClickEvent, Context,
    FontWeight, KeyDownEvent, MouseButton, MouseDownEvent, SharedString,
};

impl Orange {
    pub(super) fn render_signed_out(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
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

    /// One friend row: picture, name, what they are doing, and the action.
    ///
    /// Lives outside `render_home` so the list and its states stay one
    /// description rather than two that drift.
    fn friend_row(&self, index: usize, cx: &mut Context<Self>) -> impl IntoElement {
        let friend = &self.friends[index];
        let state = self.presence.get(&friend.id).cloned();
        let polled = self.presence_error.is_none() && !self.presence.is_empty();
        let (status, status_color) = match (&state, polled) {
            (Some(Presence::Live { .. }), _) => ("Streaming now", SUCCESS),
            (Some(Presence::Full), _) => ("Stream is full", MUTED),
            (Some(Presence::Offline), _) | (None, true) => ("Not streaming", MUTED),
            (None, false) => ("Checking\u{2026}", FAINT),
        };
        let joinable = match &state {
            Some(Presence::Live { code }) => Some(code.clone()),
            _ => None,
        };
        let already_watching = joinable
            .as_ref()
            .is_some_and(|code| self.watches.iter().any(|watch| &watch.code == code));
        let friend_id = friend.id.clone();
        let friend_for_menu = friend.clone();
        div()
            .id(SharedString::from(format!("friend-row-{}", friend.id)))
            .flex()
            .flex_row()
            .flex_shrink_0()
            .items_center()
            .justify_between()
            .gap_2()
            .min_h(px(60.0))
            .px_3()
            .py_2()
            .rounded_md()
            .bg(rgb(SURFACE))
            .border_1()
            .border_color(rgb(BORDER))
            .hover(|style| style.border_color(rgb(BORDER_HOVER)))
            // Tab changes focus on key-down in the frame. Key-up reaches the
            // newly focused row, including rows outside the current viewport.
            .on_key_up(cx.listener(move |this, event: &gpui::KeyUpEvent, _, cx| {
                if event.keystroke.key == "tab" {
                    this.friends_scroll.scroll_to_item(index);
                    cx.notify();
                }
            }))
            .on_mouse_down(
                MouseButton::Right,
                cx.listener({
                    let friend_for_menu = friend_for_menu.clone();
                    move |this, event: &MouseDownEvent, window, cx| {
                        this.open_friend_menu(&friend_for_menu, event.position, window, cx);
                        cx.stop_propagation();
                        cx.notify();
                    }
                }),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .min_w(px(0.0))
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
                                    .gap_1()
                                    .child(if matches!(state, Some(Presence::Live { .. })) {
                                        live_dot(self.animate).into_any_element()
                                    } else {
                                        dot(status_color).into_any_element()
                                    })
                                    .child(label(status, status_color).text_xs()),
                            ),
                    ),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .flex_shrink_0()
                    .child(match (joinable, already_watching) {
                        (Some(_), true) => label("Watching", FAINT).text_xs().into_any_element(),
                        (Some(code), false) => div()
                            .id(SharedString::from(format!("join-friend-{}", friend_id)))
                            .tab_index(0)
                            .px_3()
                            .py_1p5()
                            .rounded_md()
                            .bg(rgb(ORANGE))
                            .text_color(rgb(INK))
                            .text_xs()
                            .font_weight(FontWeight::SEMIBOLD)
                            .cursor_pointer()
                            .hover(|style| style.bg(rgb(ORANGE_HOT)))
                            .focus(|style| style.bg(rgb(ORANGE_HOT)))
                            .child("Join")
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.join(code.clone());
                                cx.notify();
                            }))
                            .into_any_element(),
                        (None, _) => div().into_any_element(),
                    })
                    .child(
                        div()
                            .id(SharedString::from(format!("friend-row-more-{}", friend_id)))
                            .tab_index(0)
                            .w(px(26.0))
                            .h(px(26.0))
                            .rounded_md()
                            .border_1()
                            .border_color(rgb(BORDER))
                            .text_color(rgb(MUTED))
                            .text_sm()
                            .flex()
                            .items_center()
                            .justify_center()
                            .cursor_pointer()
                            .hover(|style| {
                                style
                                    .bg(rgb(SURFACE_HOVER))
                                    .border_color(rgb(BORDER_HOVER))
                                    .text_color(rgb(TEXT))
                            })
                            .focus(|style| {
                                style
                                    .border_color(rgb(ORANGE))
                                    .text_color(rgb(TEXT))
                                    .bg(rgb(SURFACE_HOVER))
                            })
                            .child("\u{22EF}")
                            .on_click(cx.listener({
                                move |this, event: &ClickEvent, window, cx| {
                                    this.open_friend_menu(
                                        &friend_for_menu,
                                        event.position(),
                                        window,
                                        cx,
                                    );
                                    cx.stop_propagation();
                                    cx.notify();
                                }
                            })),
                    ),
            )
    }

    fn render_friend_menu(&self, cx: &mut Context<Self>) -> Option<gpui::Deferred> {
        let menu = self.friend_menu.as_ref()?;
        self.friend_menu_target()?;
        let friend_name = menu.friend_name.clone();
        let remove = div()
            .id("friend-menu-remove")
            .flex()
            .items_center()
            .px_2()
            .h(px(30.0))
            .rounded_md()
            .text_color(rgb(DANGER))
            .text_xs()
            .cursor_pointer()
            .hover(|style| style.bg(rgb(DANGER_WASH)))
            .focus(|style| style.bg(rgb(DANGER_WASH)))
            .child("Remove friend")
            .on_click(cx.listener(|this, _, window, cx| {
                if let Some(focus) = this.remove_friend_from_menu() {
                    focus.focus(window);
                }
                cx.notify();
            }));
        let remove = if let Some(menu_focus) = menu.menu_focus.clone() {
            remove.track_focus(&menu_focus)
        } else {
            remove
        };
        let menu_card = card()
            .id("friend-context-menu")
            .occlude()
            .w(px(184.0))
            .p_1()
            .gap_1()
            .shadow_md()
            .on_mouse_down_out(cx.listener(|this, _, window, cx| {
                this.close_friend_menu_and_restore_focus(window);
                cx.notify();
            }))
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                if event.keystroke.key == "escape" {
                    this.close_friend_menu_and_restore_focus(window);
                    cx.stop_propagation();
                    cx.notify();
                }
            }))
            .child(
                label(friend_name, MUTED)
                    .text_xs()
                    .px_2()
                    .py_1()
                    .text_ellipsis(),
            )
            .child(remove);
        Some(
            deferred(
                anchored()
                    .position(menu.anchor)
                    .position_mode(AnchoredPositionMode::Window)
                    .snap_to_window_with_margin(px(8.0))
                    .child(menu_card),
            )
            .with_priority(1),
        )
    }

    pub(super) fn render_friend_offer(&self, cx: &mut Context<Self>) -> Option<gpui::Div> {
        // Offered rather than added: a code gets pasted into group chats, so
        // silently keeping everyone who clicks it would hand strangers a
        // permanent view of when this user streams.
        self.pending_friend().map(|friend| {
            let name = friend.name.clone();
            let id = friend.id.clone();
            card()
                .flex_row()
                .items_center()
                .justify_between()
                .gap_3()
                .flex_shrink_0()
                .border_color(rgb(ORANGE_DIM))
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap_2p5()
                        .min_w(px(0.0))
                        .child(avatar(None, &name, 28.0))
                        .child(
                            div()
                                .flex()
                                .flex_col()
                                .gap_0p5()
                                .min_w(px(0.0))
                                .child(
                                    label(name.clone(), TEXT)
                                        .font_weight(FontWeight::SEMIBOLD)
                                        .text_ellipsis(),
                                )
                                .child(label(format!("Discord ID: {id}"), MUTED).text_xs()),
                        ),
                )
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap_2()
                        .flex_shrink_0()
                        .child(
                            div()
                                .id("keep-friend")
                                .tab_index(0)
                                .px_3()
                                .py_1p5()
                                .rounded_md()
                                .bg(rgb(ORANGE))
                                .text_color(rgb(INK))
                                .text_xs()
                                .font_weight(FontWeight::SEMIBOLD)
                                .cursor_pointer()
                                .hover(|style| style.bg(rgb(ORANGE_HOT)))
                                .focus(|style| style.bg(rgb(ORANGE_HOT)))
                                .child(if self.friend_sync.busy() {
                                    "Saving…"
                                } else {
                                    "Send request"
                                })
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    // Act on the person rendered, not whoever
                                    // happens to lead the queue when clicked.
                                    this.add_friend(friend.clone());
                                    cx.notify();
                                })),
                        )
                        .child(
                            div()
                                .id("dismiss-friend")
                                .tab_index(0)
                                .text_xs()
                                .text_color(rgb(FAINT))
                                .cursor_pointer()
                                .hover(|style| style.text_color(rgb(TEXT)))
                                .focus(|style| style.text_color(rgb(TEXT)).bg(rgb(SURFACE_HOVER)))
                                .child("Dismiss")
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.dismiss_friend_offer(&id);
                                    cx.notify();
                                })),
                        ),
                )
        })
    }

    pub(super) fn render_home(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
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
        let rows: Vec<_> = (0..self.friends.len())
            .map(|index| self.friend_row(index, cx).into_any_element())
            .collect();
        let offer = self.render_friend_offer(cx);

        let friend_tools = card()
            .flex_shrink_0()
            .py_2()
            .gap_1p5()
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .gap_2()
                    .child(micro("FRIENDS", MUTED))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .child(
                                ghost("paste-friend", "Add friend")
                                    .tab_index(0)
                                    .focus(|style| style.border_color(rgb(ORANGE)))
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        let code = cx
                                            .read_from_clipboard()
                                            .and_then(|item| item.text())
                                            .unwrap_or_default();
                                        this.offer_friend_code(&code);
                                        cx.notify();
                                    })),
                            )
                            .child(
                                quiet("copy-friend", "Copy my code")
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .h(px(30.0))
                                    .px_3()
                                    .tab_index(0)
                                    .focus(|style| style.border_color(rgb(ORANGE)))
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        match this.session.as_ref().map(|session| session.friend_code()) {
                                            Some(Ok(code)) => {
                                                cx.write_to_clipboard(gpui::ClipboardItem::new_string(code));
                                                this.show_notice(
                                                    crate::NoticeKind::Ordinary,
                                                    "Friend code copied. Send it to your friend so they can add you.",
                                                );
                                            }
                                            Some(Err(error)) => {
                                                this.show_error(format!("Could not copy friend code: {error}"))
                                            }
                                            None => this.show_error("Sign in with Discord to share your friend code."),
                                        }
                                        cx.notify();
                                    })),
                            ),
                    ),
            )
            .child(
                label(
                    "Copy their friend code, then choose Add friend.",
                    MUTED,
                )
                .text_xs(),
            );

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
                    .gap_2()
                    .flex_shrink_0()
                    .child(dot(ORANGE))
                    .child(micro("READY TO STREAM", ORANGE))
                    .child(div().flex_1().h(px(1.0)).bg(rgb(BORDER)))
                    .child(micro(defaults, MUTED)),
            )
            .child(if self.session.is_some() {
                friend_tools
            } else {
                setting_row(
                    "Add friends without streaming",
                    "Sign in with Discord to exchange personal friend codes.",
                    ghost("friends-signin", "Sign in")
                        .tab_index(0)
                        .focus(|style| style.border_color(rgb(ORANGE)))
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.start_login();
                            cx.notify();
                        }))
                        .into_any_element(),
                )
            })
            .children(self.session.is_some().then(|| {
                let incoming = self.friend_sync.snapshot.incoming.len();
                let friends = self.friends.len();
                let tab = |id: &'static str,
                           title: &'static str,
                           count: usize,
                           active: bool,
                           accent_badge: bool| {
                    div()
                        .id(id)
                        .tab_index(0)
                        .flex()
                        .items_center()
                        .gap_1()
                        .h(px(28.0))
                        .px_2()
                        .rounded_md()
                        .border_1()
                        .border_color(rgb(if active { BORDER_HOVER } else { BORDER }))
                        .bg(rgb(if active { SURFACE } else { BG }))
                        .text_color(rgb(if active { TEXT } else { MUTED }))
                        .cursor_pointer()
                        .hover(|style| style.text_color(rgb(TEXT)))
                        .focus(|style| {
                            style
                                .border_color(rgb(ORANGE_DIM))
                                .text_color(rgb(TEXT))
                                .bg(rgb(SURFACE_HOVER))
                        })
                        .child(label(title, if active { TEXT } else { MUTED }).text_xs())
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .justify_center()
                                .px_1p5()
                                .h(px(16.0))
                                .rounded_full()
                                .bg(rgb(if accent_badge {
                                    ORANGE_WASH
                                } else {
                                    SURFACE_HOVER
                                }))
                                .text_color(rgb(if accent_badge { ORANGE } else { MUTED }))
                                .text_xs()
                                .font_weight(FontWeight::SEMIBOLD)
                                .child(count.to_string()),
                        )
                };

                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .flex_shrink_0()
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_1()
                            .p_0p5()
                            .rounded_md()
                            .bg(rgb(BG))
                            .border_1()
                            .border_color(rgb(BORDER))
                            .child(
                                tab(
                                    "friends-tab",
                                    "Friends",
                                    friends,
                                    !self.requests_open,
                                    false,
                                )
                                .on_click(cx.listener(
                                    |this, _, _, cx| {
                                        this.requests_open = false;
                                        this.close_friend_menu();
                                        cx.notify();
                                    },
                                )),
                            )
                            .child(
                                tab(
                                    "requests-tab",
                                    "Requests",
                                    incoming,
                                    self.requests_open,
                                    incoming > 0,
                                )
                                .on_click(cx.listener(
                                    |this, _, _, cx| {
                                        this.requests_open = true;
                                        this.close_friend_menu();
                                        this.friend_sync.refresh();
                                        cx.notify();
                                    },
                                )),
                            ),
                    )
                    .child(
                        label(
                            if self.friend_sync.busy() {
                                "Saving…"
                            } else if !self.friend_sync.synced {
                                "Syncing…"
                            } else {
                                ""
                            },
                            MUTED,
                        )
                        .text_xs(),
                    )
            }))
            .children(self.friend_sync.error.clone().map(|error| {
                card()
                    .flex_shrink_0()
                    .gap_1()
                    .child(
                        div()
                            .id("friend-error-detail")
                            .max_h(px(40.0))
                            .overflow_y_scroll()
                            .child(
                                label(
                                    match error {
                                        PresenceError::SignedOut => {
                                            "Sign in again to sync friends.".into()
                                        }
                                        PresenceError::Unreachable(detail) => detail,
                                    },
                                    DANGER,
                                )
                                .text_xs(),
                            ),
                    )
                    .child(
                        quiet("retry-friends", "Retry")
                            .tab_index(0)
                            .on_click(cx.listener(|this, _, _, cx| {
                                if matches!(this.friend_sync.error, Some(PresenceError::SignedOut))
                                {
                                    this.start_login();
                                } else {
                                    this.friend_sync.refresh();
                                }
                                cx.notify();
                            })),
                    )
            }))
            .children(if self.requests_open { None } else { offer })
            .child(if self.requests_open {
                self.render_requests(cx).into_any_element()
            } else if self.friends.is_empty() {
                // Direct adding is available before either person streams.
                // Keep this compact so the offer and both code actions fit.
                div()
                    .id("empty-friends")
                    .relative()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_h(px(0.0))
                    .overflow_y_scroll()
                    .items_center()
                    .justify_center()
                    .gap_2()
                    .child(corner_brackets(14.0, FRAME))
                    .child(div().absolute().left(px(0.0)).child(crosshair(9.0, FRAME)))
                    .child(div().absolute().right(px(0.0)).child(crosshair(9.0, FRAME)))
                    .children(
                        (self.pending_friend().is_none() && self.friend_sync.error.is_none()).then(
                            || {
                                logo(
                                    48.0,
                                    if hosting {
                                        LogoState::Live
                                    } else {
                                        LogoState::Idle
                                    },
                                    self.logo_epoch,
                                    self.animate,
                                )
                            },
                        ),
                    )
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_1p5()
                            .child(people_mark(14.0, MUTED))
                            .child(micro("NO FRIENDS YET", MUTED)),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap_1()
                            .items_center()
                            .max_w(px(300.0))
                            .child(
                                label("Add friends above, even when nobody is streaming.", MUTED)
                                    .text_xs()
                                    .text_center(),
                            )
                            .child(
                                label(
                                    "Already have a stream code? Use Join with a code below.",
                                    FAINT,
                                )
                                .text_xs()
                                .text_center(),
                            ),
                    )
                    .into_any_element()
            } else {
                div()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .flex_1()
                    .min_h(px(0.0))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .flex_shrink_0()
                            .child(micro(
                                format!("{live} OF {} STREAMING", self.friends.len()),
                                if live > 0 { SUCCESS } else { MUTED },
                            ))
                            .child(div().flex_1().h(px(1.0)).bg(rgb(BORDER)))
                            .children(watching.then(|| {
                                div()
                                    .id("open-watching")
                                    .cursor_pointer()
                                    .child(micro(
                                        format!("{} OPEN \u{2192}", self.watches.len()),
                                        SUCCESS,
                                    ))
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.screen = Screen::Watching;
                                        cx.notify();
                                    }))
                            })),
                    )
                    .children(self.presence_error.clone().map(|error| {
                        match error {
                            // The relay answered; it simply does not know this
                            // session any more. Reporting that as a network fault
                            // sends people to look at their connection, and the
                            // raw URL tells them nothing they can act on.
                            PresenceError::SignedOut => card()
                                .flex_row()
                                .items_center()
                                .justify_between()
                                .gap_3()
                                .py_2()
                                .flex_shrink_0()
                                .border_color(rgb(ORANGE_DIM))
                                .child(
                                    div()
                                        .flex()
                                        .flex_col()
                                        .gap_0p5()
                                        .min_w(px(0.0))
                                        .child(
                                            label("Signed out", TEXT)
                                                .font_weight(FontWeight::SEMIBOLD)
                                                .text_xs(),
                                        )
                                        .child(
                                            label(
                                                "The relay restarted. Sign in to see friends.",
                                                FAINT,
                                            )
                                            .text_xs(),
                                        ),
                                )
                                .child(ghost("presence-signin", "Sign in").on_click(cx.listener(
                                    |this, _, _, cx| {
                                        this.start_login();
                                        cx.notify();
                                    },
                                ))),
                            PresenceError::Unreachable(detail) => card()
                                .py_2()
                                .flex_shrink_0()
                                .child(label("Could not reach the relay", DANGER).text_xs())
                                .child(label(detail, FAINT).text_xs()),
                        }
                    }))
                    .child(
                        div()
                            .id("friend-list")
                            .track_scroll(&self.friends_scroll)
                            .flex()
                            .flex_col()
                            .gap_2()
                            .flex_1()
                            .min_h(px(0.0))
                            .overflow_y_scroll()
                            .children(rows),
                    )
                    .into_any_element()
            })
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .flex_shrink_0()
                    .child(
                        primary(
                            "start",
                            div()
                                .flex()
                                .items_center()
                                .gap_2()
                                .child(share_icon(INK))
                                .child(if hosting {
                                    "View active stream"
                                } else {
                                    "Start streaming"
                                }),
                            false,
                        )
                        .h(px(40.0))
                        .px_3()
                        .border_1()
                        .border_color(rgb(ORANGE_HOT))
                        .tab_index(0)
                        .focus(|style| style.bg(rgb(ORANGE_HOT)))
                        .flex_1()
                        .min_w(px(0.0))
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
                        secondary(
                            "join-code",
                            div()
                                .flex()
                                .items_center()
                                .gap_2()
                                .child(join_icon(TEXT))
                                .child("Join with a code"),
                        )
                        .h(px(40.0))
                        .px_3()
                        .w(px(180.0))
                        .flex_shrink_0()
                        .tab_index(0)
                        .focus(|style| style.border_color(rgb(ORANGE)))
                        .on_click(cx.listener(|this, _, _, cx| {
                            // A room code opens video; a personal code only adds a
                            // friend. Keep the clipboard actions distinct.
                            let code = cx
                                .read_from_clipboard()
                                .and_then(|item| item.text())
                                .unwrap_or_default();
                            this.join(code);
                            cx.notify();
                        })),
                    ),
            )
            .children(self.render_friend_menu(cx))
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
}
