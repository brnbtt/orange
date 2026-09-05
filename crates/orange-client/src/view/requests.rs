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
        let actions: Vec<_> = if incoming {
            vec![(Action::Accept, "Accept"), (Action::Decline, "Decline")]
        } else {
            vec![(Action::Cancel, "Cancel")]
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
                                        "Wants to be friends"
                                    } else {
                                        "Request pending"
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
        let snapshot = &self.friend_sync.snapshot;
        div()
            .id("request-list")
            .flex()
            .flex_col()
            .gap_2()
            .flex_1()
            .min_h(px(0.0))
            .overflow_y_scroll()
            .child(micro("INCOMING", ORANGE))
            .when(snapshot.incoming.is_empty(), |element| {
                element.child(label("No incoming requests", MUTED).text_xs())
            })
            .children(
                snapshot
                    .incoming
                    .iter()
                    .map(|contact| self.request_row(contact, true, cx)),
            )
            .child(micro("SENT", MUTED))
            .when(snapshot.outgoing.is_empty(), |element| {
                element.child(label("No pending requests sent", MUTED).text_xs())
            })
            .children(
                snapshot
                    .outgoing
                    .iter()
                    .map(|contact| self.request_row(contact, false, cx)),
            )
    }
}
