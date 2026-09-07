//! Incoming and outgoing friend requests share the Home list's scroll area.

use crate::{
    friends::{Action, Contact},
    ui::*,
    Orange,
};
use gpui::{div, prelude::*, px, rgb, Context, SharedString};

impl Orange {
    fn request_row(&self, contact: &Contact, incoming: bool, cx: &mut Context<Self>) -> gpui::Div {
        let id = contact.profile.id.clone();
        let revision = contact.revision.clone();
        let copy = self.copy();
        let actions: Vec<_> = if incoming {
            vec![
                (Action::Accept, copy.requests.accept),
                (Action::Decline, copy.requests.decline),
            ]
        } else {
            vec![(Action::Cancel, copy.requests.cancel)]
        };
        let busy = self.friend_sync.busy();
        card()
            .flex_shrink_0()
            .gap_2()
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(avatar(None, &contact.profile.name, 28.0))
                    .child(
                        div()
                            .flex_col()
                            .flex()
                            .min_w(px(0.0))
                            .child(label(contact.profile.name.clone(), TEXT).text_ellipsis())
                            .child(
                                label(
                                    if incoming {
                                        copy.requests.wants_friends
                                    } else {
                                        copy.requests.request_pending
                                    },
                                    MUTED,
                                )
                                .text_xs(),
                            ),
                    ),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .children(actions.into_iter().map(|(action, text)| {
                        let id = id.clone();
                        let revision = revision.clone();
                        quiet("request-action", text)
                            .id(SharedString::from(format!("request-{id}-{text}")))
                            .tab_index(0)
                            .focus(|style| style.border_color(rgb(ORANGE)))
                            .when(busy, |element| element.opacity(0.5).cursor_default())
                            .on_click(cx.listener(move |this, _, _, cx| {
                                if !this.friend_sync.busy() {
                                    this.change_friend(action, &id, Some(revision.clone()));
                                }
                                cx.notify();
                            }))
                    })),
            )
    }

    pub(super) fn render_requests(&self, cx: &mut Context<Self>) -> gpui::Stateful<gpui::Div> {
        let copy = self.copy();
        let snapshot = &self.friend_sync.snapshot;
        div()
            .id("request-list")
            .flex()
            .flex_col()
            .gap_2()
            .flex_1()
            .min_h(px(0.0))
            .overflow_y_scroll()
            .child(micro(copy.requests.incoming, ORANGE))
            .when(snapshot.incoming.is_empty(), |element| {
                element.child(label(copy.requests.no_incoming, MUTED).text_xs())
            })
            .children(
                snapshot
                    .incoming
                    .iter()
                    .map(|contact| self.request_row(contact, true, cx)),
            )
            .child(micro(copy.requests.sent, MUTED))
            .when(snapshot.outgoing.is_empty(), |element| {
                element.child(label(copy.requests.no_pending_sent, MUTED).text_xs())
            })
            .children(
                snapshot
                    .outgoing
                    .iter()
                    .map(|contact| self.request_row(contact, false, cx)),
            )
    }
}
