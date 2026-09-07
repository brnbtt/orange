//! The floating toast layer.
//!
//! Absolutely positioned below the titlebar so a toast never reflows the
//! screen underneath it.

use crate::{ui::*, update, NoticeKind, Orange};

use gpui::{div, prelude::*, px, rgb, Context};

impl Orange {
    /// The floating toast layer.
    ///
    /// Absolutely positioned below the titlebar so a toast never reflows the
    /// screen underneath it. The update toast collapses but cannot be
    /// dismissed, because an available update stays actionable; the error toast
    /// closes, because a read error has no follow-up.
    pub(super) fn render_toasts(&mut self, cx: &mut Context<Self>) -> Option<impl IntoElement> {
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
        let copy = self.copy();
        let action = self.updates.status().action_label(copy);
        let (heading, detail, action) = match self.updates.status() {
            update::UpdateStatus::Available(info) => (
                crate::i18n::fill(copy.update.available_heading, &info.version),
                if info.notes.is_empty() {
                    copy.update.beta_ready.to_string()
                } else {
                    info.notes.clone()
                },
                action,
            ),
            update::UpdateStatus::Downloading(info) => (
                crate::i18n::fill(copy.update.downloading_heading, &info.version),
                copy.update.will_restart.to_string(),
                None,
            ),
            update::UpdateStatus::Failed { message, .. } => (
                copy.update.paused_heading.to_string(),
                copy.update_failure(message),
                self.updates.status().action_label(copy),
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
}
