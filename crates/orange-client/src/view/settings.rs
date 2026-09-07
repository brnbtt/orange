//! The settings screen: its collapsible sections and the cards inside them.

use crate::{
    supervisor::{FRAME_RATES, QUALITIES},
    troubleshoot::{CheckStatus, UploadUiState},
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

    fn settings_troubleshoot_card(&mut self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let running = self.troubleshoot.is_running();
        let has_result = self.troubleshoot.has_result();
        let sending = self.troubleshoot.is_uploading();
        let upload_state = self.troubleshoot.upload_state();
        let cancelling = upload_state == UploadUiState::Cancelling;
        let sent = upload_state == UploadUiState::Sent;
        let action_label = if cancelling {
            "Stopping…"
        } else if running || sending {
            "Cancel"
        } else if has_result {
            "Run again"
        } else {
            "Troubleshoot"
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
            action
                .on_click(cx.listener(|this, _, _, cx| {
                    if this.troubleshoot.is_running() || this.troubleshoot.is_uploading() {
                        this.cancel_troubleshoot();
                    } else {
                        this.start_troubleshoot();
                    }
                    cx.notify();
                }))
                .on_key_down(cx.listener(|this, event: &KeyDownEvent, _, cx| {
                    if matches!(event.keystroke.key.as_str(), "enter" | "space") {
                        if this.troubleshoot.is_running() || this.troubleshoot.is_uploading() {
                            this.cancel_troubleshoot();
                        } else {
                            this.start_troubleshoot();
                        }
                        cx.stop_propagation();
                        cx.notify();
                    }
                }))
                .into_any_element()
        };

        let mut content = card()
            .w_full()
            .min_w(px(0.0))
            .flex_shrink_0()
            .gap_2()
            .child(
                setting_row("Troubleshooting", self.troubleshoot.headline(), action)
                    .p_0()
                    .border_0()
                    .bg(rgb(SURFACE)),
            );

        if let Some(summary) = self.troubleshoot.summary() {
            content = content.child(micro("LAST RUN", MUTED));
            content = content.child(
                label(
                    summary,
                    if summary == "Everything checked looks good." {
                        SUCCESS
                    } else {
                        DANGER
                    },
                )
                .text_xs(),
            );

            for check in self.troubleshoot.friendly_checks() {
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
                        ),
                );
            }

            content = content.child(
                label(
                    "Sends these results and recent Orange logs to our team.",
                    MUTED,
                )
                .text_xs(),
            );

            let send_button = quiet(
                "send-troubleshoot-report",
                if cancelling {
                    "Stopping…"
                } else if sending {
                    "Sending…"
                } else if sent {
                    "Sent"
                } else {
                    "Send report"
                },
            )
            .tab_index(0)
            .focus(|style| style.border_color(rgb(ORANGE_DIM)).text_color(rgb(TEXT)));

            content = content.child(if sending || sent || cancelling {
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

            if let Some(message) = upload_state.message() {
                let color = match upload_state {
                    UploadUiState::Sent => SUCCESS,
                    UploadUiState::Retry | UploadUiState::SignInRequired => DANGER,
                    _ => MUTED,
                };
                content = content.child(label(message, color).text_xs());
            }

            // Keep copy as an offline fallback, after the send path.
            content = content.child(
                quiet("copy-troubleshoot", "Copy report")
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
            .child(
                label(
                    "Try a stream with a friend to check picture and sound.",
                    MUTED,
                )
                .text_xs(),
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

    pub(super) fn render_settings(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let fade = scroll_fade(&self.settings_scroll);
        let account = self.settings_account_card(cx);
        let resolution = self.settings_resolution_card(cx);
        let frame_rate = self.settings_frame_rate_card(cx);
        let updates = self.settings_updates_card(cx);
        let diagnostics = self.settings_diagnostics_card(cx);
        let troubleshoot = self.settings_troubleshoot_card(cx);

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
            vec![updates, diagnostics, troubleshoot],
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
