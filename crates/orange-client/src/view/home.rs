//! The signed-out screen and the home screen with its friends list.

use crate::{
    presence::{Presence, PresenceError},
    ui::*,
    Orange, Screen,
};

use gpui::{div, prelude::*, px, rgb, Context, FontWeight, SharedString};

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
            (Some(Presence::Offline), _) | (None, true) => ("Not streaming", FAINT),
            (None, false) => ("Checking\u{2026}", FAINT),
        };
        let joinable = match &state {
            Some(Presence::Live { code }) => Some(code.clone()),
            _ => None,
        };
        let already_watching = joinable
            .as_ref()
            .is_some_and(|code| self.watches.iter().any(|watch| &watch.code == code));
        let id = friend.id.clone();

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
                            .id(SharedString::from(format!("join-friend-{index}")))
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
                    .child(
                        // Removing has to be as easy as adding: a roster you
                        // cannot prune only ever grows, and every name on it
                        // can see when you go live.
                        div()
                            .id(SharedString::from(format!("remove-friend-{index}")))
                            .text_xs()
                            .text_color(rgb(FAINT))
                            .cursor_pointer()
                            .hover(|style| style.text_color(rgb(DANGER)))
                            .child("Remove")
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.remove_friend(&id);
                                cx.notify();
                            })),
                    ),
            )
    }

    pub(super) fn render_home(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
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
        // Offered rather than added: a code gets pasted into group chats, so
        // silently keeping everyone who clicks it would hand strangers a
        // permanent view of when this user streams.
        let offer = self.pending_friend().map(|friend| {
            let name = friend.name.clone();
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
                                .child(label("Keep as a friend?", FAINT).text_xs()),
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
                                .px_3()
                                .py_1p5()
                                .rounded_md()
                                .bg(rgb(ORANGE))
                                .text_color(rgb(INK))
                                .text_xs()
                                .font_weight(FontWeight::SEMIBOLD)
                                .cursor_pointer()
                                .hover(|style| style.bg(rgb(ORANGE_HOT)))
                                .child("Add")
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    if let Some(friend) = this.pending_friend() {
                                        this.add_friend(friend);
                                    }
                                    cx.notify();
                                })),
                        )
                        .child(
                            div()
                                .id("dismiss-friend")
                                .text_xs()
                                .text_color(rgb(FAINT))
                                .cursor_pointer()
                                .hover(|style| style.text_color(rgb(TEXT)))
                                .child("No")
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.dismiss_pending_friend();
                                    cx.notify();
                                })),
                        ),
                )
        });

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
            .children(offer)
            .child(if self.friends.is_empty() {
                // Nothing to list yet, so the space explains how a list comes
                // to exist rather than showing an empty box. This is the only
                // moment the app can teach the flow, because once one friend
                // exists the screen never looks like this again.
                div()
                    .relative()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_h(px(0.0))
                    .items_center()
                    .justify_center()
                    .gap_3()
                    .child(corner_brackets(14.0, FRAME))
                    .child(div().absolute().left(px(0.0)).child(crosshair(9.0, FRAME)))
                    .child(div().absolute().right(px(0.0)).child(crosshair(9.0, FRAME)))
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
                                label("Paste a friend's code below to watch them.", MUTED)
                                    .text_xs()
                                    .text_center(),
                            )
                            .child(
                                label(
                                    "Afterwards you can keep them, and their \
                                     streams show up here automatically.",
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
                secondary("join-code", "Join with a code").on_click(cx.listener(
                    |this, _, _, cx| {
                        // Still the only way to reach someone you have never
                        // watched. It is also how the roster starts, so it is
                        // a first-class action rather than a fallback.
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
