//! The share picker: the quality choice, the whole-display row, and the grid
//! of capturable windows.

use crate::{
    supervisor::{WindowTarget, QUALITIES},
    ui::*,
    Orange, Screen,
};

use gpui::{div, prelude::*, px, rgb, Context, SharedString};

impl Orange {
    pub(super) fn render_pick(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let copy = self.copy();
        let selected = self.quality;
        // Distinguishes "still capturing" from "this window refuses to draw",
        // which previously both showed as "no preview" and made every card
        // flash a failure message before its thumbnail arrived.
        let capturing = self.picker_busy();
        let loading = self.picker_loading();

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
                            .child(micro(
                                if loading {
                                    copy.pick.finding_sources.to_string()
                                } else {
                                    crate::i18n::fill(copy.pick.sources_available, count)
                                },
                                ORANGE,
                            ))
                            .child(heading(copy.pick.choose_what, 14.0))
                            .child(label(copy.pick.click_preview, FAINT).text_xs()),
                    )
                    .child(ghost("refresh", copy.pick.refresh).on_click(cx.listener(
                        |this, _, _, cx| {
                            this.refresh_windows();
                            cx.notify();
                        },
                    ))),
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
                                    card().w_full().items_center().child(
                                        label(
                                            if loading {
                                                copy.pick.finding_windows
                                            } else {
                                                copy.pick.no_windows
                                            },
                                            MUTED,
                                        )
                                        .text_xs(),
                                    ),
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
                    .child(ghost("back", copy.pick.back).on_click(cx.listener(
                        |this, _, _, cx| {
                            this.leave_picker(Screen::Home);
                            cx.notify();
                        },
                    ))),
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
        let copy = self.copy();
        let meta = if target.width > 0 && target.height > 0 {
            crate::i18n::fill2(copy.pick.includes_system_audio, target.width, target.height)
        } else {
            copy.pick.full_display_audio.to_string()
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
                        (None, false) => micro(copy.pick.no_preview, FAINT).into_any_element(),
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
                        (None, true) => label(self.copy().pick.capturing, FAINT)
                            .text_xs()
                            .into_any_element(),
                        (None, false) => label(self.copy().pick.preview_unavailable, FAINT)
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
}
