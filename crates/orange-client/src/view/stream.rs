//! The two live-session screens: hosting a stream and watching one.

use crate::{sound, ui::*, Orange, Screen};

use gpui::{div, prelude::*, px, rgb, Context, FontWeight, SharedString};
use std::time::Instant;

impl Orange {
    pub(super) fn render_streaming(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
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

    pub(super) fn render_watching(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
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
}
