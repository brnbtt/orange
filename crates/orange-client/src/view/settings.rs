//! The settings screen: its collapsible sections and the cards inside them.

use crate::{
    i18n,
    supervisor::{FRAME_RATES, QUALITIES},
    troubleshoot::{CheckStatus, LastConnectionStatus, UploadUiState},
    ui::*,
    update, Orange, Screen,
};

use gpui::{div, prelude::*, px, rgb, Context, KeyDownEvent, SharedString};

/// Indices into `Orange::settings_open`.
const SECTION_ACCOUNT: usize = 0;
const SECTION_STREAMING: usize = 1;
const SECTION_APPLICATION: usize = 2;

impl Orange {
    fn settings_account_card(&mut self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let signed_in = self.session.as_ref().map(|s| s.name.clone());
        // The person is the content and the provider is the qualifier; the other
        // way round read like a list of connected services when there is one.
        let copy = self.copy();
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
                .child(label(copy.settings.not_signed_in, TEXT))
                .child(label(copy.settings.sign_in_to_share, MUTED).text_xs())
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
                Some(_) => ghost("so", copy.settings.sign_out)
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.sign_out(None);
                        cx.notify();
                    }))
                    .into_any_element(),
                None => ghost("si", copy.settings.sign_in)
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
            self.copy().settings.resolution,
            Some(SharedString::from(format!(
                "{} \u{d7} {}",
                chosen.max_width, chosen.max_height
            ))),
            pills,
            self.copy().quality_detail(selected),
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

        let detail_index = FRAME_RATES
            .iter()
            .position(|rate| rate.fps == selected)
            .unwrap_or(0);

        // No readout: a readout shows what an abstract label resolves to, and
        // "60" is not abstract.
        setting_choice(
            self.copy().settings.frame_rate,
            None,
            pills,
            self.copy().fps_detail(detail_index),
        )
        .into_any_element()
    }

    fn settings_language_card(&mut self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let copy = self.copy();
        let selected = self.locale;
        let pills = i18n::Locale::ALL
            .iter()
            .map(|&locale| {
                option_pill(
                    SharedString::from(format!("settings-lang-{}", locale.as_str())),
                    locale.label(),
                    locale == selected,
                )
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.locale = locale;
                    this.save_preferences();
                    cx.notify();
                }))
                .into_any_element()
            })
            .collect();
        setting_choice(
            copy.settings.language,
            None,
            pills,
            copy.settings.language_detail,
        )
        .into_any_element()
    }

    fn settings_updates_card(&mut self, cx: &mut Context<Self>) -> gpui::AnyElement {
        // The action is always rendered so the row does not reflow while a
        // check runs; dimmed and inert when nothing applies. The label follows
        // the state: an available update installs, anything else checks.
        let copy = self.copy();
        let action = self.updates.settings_action(copy);
        let button = quiet("check-updates", action.unwrap_or(copy.update.check_now));
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
        setting_row(
            copy.settings.updates,
            self.updates.settings_detail(copy),
            action,
        )
        .into_any_element()
    }

    fn settings_diagnostics_card(&mut self, cx: &mut Context<Self>) -> gpui::AnyElement {
        setting_row(
            self.copy().settings.diagnostics,
            self.copy().settings.logs_recent,
            quiet("open-diagnostics", self.copy().settings.open_folder)
                .on_click(cx.listener(|this, _, _, cx| {
                    this.open_diagnostics();
                    cx.notify();
                }))
                .into_any_element(),
        )
        .into_any_element()
    }

    fn settings_troubleshoot_card(&mut self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let copy = self.copy();
        let running = self.troubleshoot.is_running();
        let repair_running = self.troubleshoot.is_repair_running();
        let has_result = self.troubleshoot.has_result();
        let sending = self.troubleshoot.is_uploading();
        let upload_state = self.troubleshoot.upload_state();
        let cancelling = self.troubleshoot.is_cancelling();
        let sent = upload_state == UploadUiState::Sent;
        let action_label = if cancelling {
            copy.settings.stopping
        } else if running || sending {
            copy.settings.cancel
        } else if has_result {
            copy.settings.run_again
        } else {
            copy.settings.troubleshoot
        };

        let action = quiet("run-troubleshoot", action_label)
            .tab_index(0)
            .focus(|style| style.border_color(rgb(ORANGE_DIM)).text_color(rgb(TEXT)));
        let action = if cancelling {
            action
                .text_color(rgb(FAINT))
                .border_color(rgb(BORDER_DIM))
                .cursor_default()
                .into_any_element()
        } else {
            // GPUI emits keyboard clicks for this focusable control. A second
            // key handler started a check and immediately cancelled it on Space.
            action
                .on_click(cx.listener(|this, _, _, cx| {
                    if this.troubleshoot.is_running() || this.troubleshoot.is_uploading() {
                        this.cancel_troubleshoot();
                    } else {
                        this.start_troubleshoot();
                    }
                    cx.notify();
                }))
                .into_any_element()
        };

        let mut content = card()
            .w_full()
            .min_w(px(0.0))
            .flex_shrink_0()
            .gap_2()
            .child(
                setting_row(
                    copy.settings.troubleshooting,
                    self.troubleshoot.headline_for(copy),
                    action,
                )
                .p_0()
                .border_0()
                .bg(rgb(SURFACE)),
            );

        if let Some(summary) = self.troubleshoot.summary_for(copy) {
            content = content.child(micro(copy.settings.last_run, MUTED));
            content = content.child(
                label(
                    summary,
                    if summary == copy.troubleshoot.summary_ok {
                        SUCCESS
                    } else {
                        DANGER
                    },
                )
                .text_xs(),
            );

            content = content.child(micro(copy.settings.last_connection, MUTED));
            let (last_connection, last_connection_detail, color) =
                match self.troubleshoot.latest_connection() {
                    Some((LastConnectionStatus::Failed, age)) => (
                        copy.settings.last_stream_problem,
                        i18n::fill(copy.settings.last_stream_problem_detail, age),
                        DANGER,
                    ),
                    Some((LastConnectionStatus::Connected, age)) => (
                        copy.settings.connection_established,
                        format!("{age}."),
                        SUCCESS,
                    ),
                    Some((LastConnectionStatus::Unknown, age)) => {
                        (copy.settings.no_completed_check, format!("{age}."), MUTED)
                    }
                    None => (
                        copy.settings.no_completed_check,
                        copy.settings.try_stream_then.to_string(),
                        MUTED,
                    ),
                };
            content = content
                .child(label(last_connection, color).text_xs())
                .child(label(last_connection_detail, MUTED).text_xs());

            content = content.child(micro(copy.settings.current_checks, MUTED));
            for check in self.troubleshoot.friendly_checks_for(copy) {
                let color = match check.status {
                    CheckStatus::Pass => SUCCESS,
                    CheckStatus::Fail => DANGER,
                    CheckStatus::Inconclusive => DANGER,
                };
                content = content.child(
                    div()
                        .flex()
                        .flex_col()
                        .min_w(px(0.0))
                        .gap_0p5()
                        .child(
                            div().flex().items_center().gap_2().child(dot(color)).child(
                                label(format!("{} · {}", check.label, check.state_text), color)
                                    .text_xs(),
                            ),
                        )
                        .children(
                            (!check.action_text.is_empty())
                                .then(|| label(check.action_text, MUTED).text_xs()),
                        )
                        .children(check.repairable.then(|| {
                            div()
                                .flex()
                                .flex_col()
                                .gap_1()
                                .child(label(copy.settings.windows_may_ask, MUTED).text_xs())
                                .child(
                                    {
                                        let button = quiet(
                                            "repair-network",
                                            if cancelling {
                                                copy.settings.stopping
                                            } else if repair_running {
                                                copy.settings.fixing
                                            } else {
                                                copy.settings.fix_connection
                                            },
                                        )
                                        .tab_index(0)
                                        .focus(|style| {
                                            style
                                                .border_color(rgb(ORANGE_DIM))
                                                .text_color(rgb(TEXT))
                                        });
                                        if running || sending || cancelling {
                                            button
                                                .text_color(rgb(FAINT))
                                                .border_color(rgb(BORDER_DIM))
                                                .cursor_default()
                                                .into_any_element()
                                        } else {
                                            button
                                                .on_click(cx.listener(|this, _, _, cx| {
                                                    this.troubleshoot.start_repair(
                                                        &this.server,
                                                        crate::supervisor::diagnostics_directory(),
                                                    );
                                                    cx.notify();
                                                }))
                                                .on_key_down(cx.listener(
                                                    |this, event: &KeyDownEvent, _, cx| {
                                                        if matches!(
                                                            event.keystroke.key.as_str(),
                                                            "enter" | "space"
                                                        ) {
                                                            this.troubleshoot.start_repair(
                                                                &this.server,
                                                                crate::supervisor::diagnostics_directory(),
                                                            );
                                                            cx.stop_propagation();
                                                            cx.notify();
                                                        }
                                                    },
                                                ))
                                                .into_any_element()
                                        }
                                    },
                                )
                                .into_any_element()
                        })),
                );
            }

            if let Some((message, success)) = self.troubleshoot.repair_message() {
                content =
                    content.child(label(message, if success { SUCCESS } else { DANGER }).text_xs());
            }

            content = content.child(label(copy.settings.send_results, MUTED).text_xs());

            let send_button = quiet(
                "send-troubleshoot-report",
                if cancelling {
                    copy.settings.stopping
                } else if sending {
                    copy.settings.sending
                } else if sent {
                    copy.settings.sent
                } else {
                    copy.settings.send_report
                },
            )
            .tab_index(0)
            .focus(|style| style.border_color(rgb(ORANGE_DIM)).text_color(rgb(TEXT)));

            content = content.child(if running || sending || sent || cancelling {
                send_button
                    .text_color(rgb(FAINT))
                    .border_color(rgb(BORDER_DIM))
                    .cursor_default()
                    .into_any_element()
            } else {
                send_button
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.send_troubleshoot_report();
                        cx.notify();
                    }))
                    .on_key_down(cx.listener(|this, event: &KeyDownEvent, _, cx| {
                        if matches!(event.keystroke.key.as_str(), "enter" | "space") {
                            this.send_troubleshoot_report();
                            cx.stop_propagation();
                            cx.notify();
                        }
                    }))
                    .into_any_element()
            });

            if let Some(message) = upload_state.message_for(copy) {
                let color = match upload_state {
                    UploadUiState::Sent => SUCCESS,
                    UploadUiState::Retry | UploadUiState::SignInRequired => DANGER,
                    _ => MUTED,
                };
                content = content.child(label(message, color).text_xs());
            }

            // Keep copy as an offline fallback, after the send path.
            content = content.child(
                quiet("copy-troubleshoot", copy.settings.copy_report)
                    .tab_index(0)
                    .focus(|style| style.border_color(rgb(ORANGE_DIM)).text_color(rgb(TEXT)))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.copy_troubleshoot_report(cx);
                        cx.notify();
                    }))
                    .on_key_down(cx.listener(|this, event: &KeyDownEvent, _, cx| {
                        if matches!(event.keystroke.key.as_str(), "enter" | "space") {
                            this.copy_troubleshoot_report(cx);
                            cx.stop_propagation();
                            cx.notify();
                        }
                    })),
            );
        }

        content
            .child(label(copy.settings.try_stream_friend, MUTED).text_xs())
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

    pub(super) fn render_settings(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let fade = scroll_fade(&self.settings_scroll);
        let copy = self.copy();
        let account = self.settings_account_card(cx);
        let resolution = self.settings_resolution_card(cx);
        let frame_rate = self.settings_frame_rate_card(cx);
        let language = self.settings_language_card(cx);
        let updates = self.settings_updates_card(cx);
        let diagnostics = self.settings_diagnostics_card(cx);
        let troubleshoot = self.settings_troubleshoot_card(cx);

        let account = self.settings_section(
            SECTION_ACCOUNT,
            "section-account",
            copy.settings.account,
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
            copy.settings.video,
            Some(copy.settings.defaults),
            vec![resolution, frame_rate],
            cx,
        );
        let system = self.settings_section(
            SECTION_APPLICATION,
            "section-system",
            copy.settings.system,
            None,
            vec![language, updates, diagnostics, troubleshoot],
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
                crate::i18n::fill2(
                    copy.settings.version,
                    update::current_version(),
                    update::build_label(),
                ),
                FAINT,
            ))
            .child(
                ghost("back-settings", copy.settings.done)
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
