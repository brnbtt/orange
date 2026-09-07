//! UI language catalogs.
//!
//! Copy lives here, not in the views. A struct catalog makes a missing
//! translation a compile error instead of a runtime blank, which is how
//! string-key tables go stale. Adding a language is a new file and a
//! [`Locale`] arm.

mod en;
mod pt_br;

use serde::{Deserialize, Serialize};
use std::time::Duration;

/// Languages the client can render. Persist as BCP-47 tags.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Locale {
    #[serde(rename = "en")]
    En,
    #[serde(rename = "pt-BR")]
    PtBr,
}

impl Locale {
    pub fn catalog(self) -> &'static Catalog {
        match self {
            Self::En => &en::CATALOG,
            Self::PtBr => &pt_br::CATALOG,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::En => "en",
            Self::PtBr => "pt-BR",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::En => "English",
            Self::PtBr => "Português",
        }
    }

    pub const ALL: [Self; 2] = [Self::En, Self::PtBr];

    /// Map a Windows locale name such as `pt-BR` or `en-US`.
    ///
    /// Any Portuguese tag uses Brazilian Portuguese because that is the only
    /// translation we ship. Unknown tags stay English.
    pub fn from_windows_name(name: &str) -> Self {
        let tag = name.trim();
        if tag.len() >= 2 && tag.as_bytes()[..2].eq_ignore_ascii_case(b"pt") {
            Self::PtBr
        } else {
            Self::En
        }
    }

    pub fn detect() -> Self {
        Self::from_windows_name(&windows_locale_name())
    }
}

fn windows_locale_name() -> String {
    let mut name = [0u16; 85];
    // SAFETY: `name` is a writable UTF-16 buffer. The API writes a
    // NUL-terminated locale name and returns the length including NUL.
    let len = unsafe { windows::Win32::Globalization::GetUserDefaultLocaleName(&mut name) };
    if len <= 1 {
        return String::new();
    }
    String::from_utf16_lossy(&name[..len as usize - 1])
}

pub fn fill(template: &str, value: impl std::fmt::Display) -> String {
    match template.split_once("{}") {
        Some((head, tail)) => format!("{head}{value}{tail}"),
        None => template.to_string(),
    }
}

pub fn fill2(
    template: &str,
    first: impl std::fmt::Display,
    second: impl std::fmt::Display,
) -> String {
    fill(&fill(template, first), second)
}

#[derive(Clone, Copy)]
pub struct Catalog {
    pub chrome: Chrome,
    pub home: Home,
    pub requests: Requests,
    pub pick: Pick,
    pub stream: Stream,
    pub settings: Settings,
    pub update: Update,
    pub troubleshoot: Troubleshoot,
    pub notice: Notice,
}

#[derive(Clone, Copy)]
pub struct Chrome {
    pub share: &'static str,
    pub streaming: &'static str,
    pub watching: &'static str,
    pub settings: &'static str,
}

#[derive(Clone, Copy)]
pub struct Home {
    pub share_heading: &'static str,
    pub share_blurb: &'static str,
    pub waiting_discord: &'static str,
    pub sign_in_discord: &'static str,
    pub finish_in_browser: &'static str,
    pub continue_without: &'static str,
    pub streaming_now: &'static str,
    pub stream_full: &'static str,
    pub not_streaming: &'static str,
    pub checking: &'static str,
    pub alerts_muted: &'static str,
    pub watching: &'static str,
    pub join: &'static str,
    pub unmute_alerts: &'static str,
    pub mute_alerts: &'static str,
    pub remove_friend: &'static str,
    pub discord_id: &'static str,
    pub saving: &'static str,
    pub send_request: &'static str,
    pub dismiss: &'static str,
    pub friends_heading: &'static str,
    pub show: &'static str,
    pub hide: &'static str,
    pub add_friend: &'static str,
    pub copy_my_code: &'static str,
    pub friend_code_copied: &'static str,
    pub copy_their_code: &'static str,
    pub ready_to_stream: &'static str,
    pub add_friends_without: &'static str,
    pub sign_in_to_exchange: &'static str,
    pub sign_in: &'static str,
    pub tab_friends: &'static str,
    pub tab_requests: &'static str,
    pub syncing: &'static str,
    pub sign_in_again_sync: &'static str,
    pub retry: &'static str,
    pub no_friends_yet: &'static str,
    pub add_friends_above: &'static str,
    pub already_have_code: &'static str,
    pub signed_out: &'static str,
    pub relay_restarted: &'static str,
    pub could_not_reach_relay: &'static str,
    pub of_streaming: &'static str,
    pub open_watchers: &'static str,
    pub view_active_stream: &'static str,
    pub start_streaming: &'static str,
    pub join_with_code: &'static str,
    pub signed_in_as: &'static str,
    pub sign_out: &'static str,
}

#[derive(Clone, Copy)]
pub struct Requests {
    pub accept: &'static str,
    pub decline: &'static str,
    pub cancel: &'static str,
    pub wants_friends: &'static str,
    pub request_pending: &'static str,
    pub incoming: &'static str,
    pub no_incoming: &'static str,
    pub sent: &'static str,
    pub no_pending_sent: &'static str,
}

#[derive(Clone, Copy)]
pub struct Pick {
    pub finding_sources: &'static str,
    pub sources_available: &'static str,
    pub choose_what: &'static str,
    pub click_preview: &'static str,
    pub refresh: &'static str,
    pub finding_windows: &'static str,
    pub no_windows: &'static str,
    pub back: &'static str,
    pub includes_system_audio: &'static str,
    pub full_display_audio: &'static str,
    pub no_preview: &'static str,
    pub capturing: &'static str,
    pub preview_unavailable: &'static str,
}

#[derive(Clone, Copy)]
pub struct Stream {
    pub selected_source: &'static str,
    pub streaming: &'static str,
    pub starting: &'static str,
    pub source_preview_unavailable: &'static str,
    pub source_preview: &'static str,
    pub share_code: &'static str,
    pub copied: &'static str,
    pub click_to_copy: &'static str,
    pub connecting: &'static str,
    pub nobody_watching: &'static str,
    pub n_watching: &'static str,
    pub back: &'static str,
    pub stop_streaming: &'static str,
    pub open_own_viewer: &'static str,
    pub close: &'static str,
    pub watching_friends: &'static str,
    pub watch_another: &'static str,
    pub close_all: &'static str,
}

#[derive(Clone, Copy)]
pub struct Settings {
    pub not_signed_in: &'static str,
    pub sign_in_to_share: &'static str,
    pub sign_out: &'static str,
    pub sign_in: &'static str,
    pub resolution: &'static str,
    pub frame_rate: &'static str,
    pub quality_details: [&'static str; 4],
    pub fps_details: [&'static str; 2],
    pub updates: &'static str,
    pub diagnostics: &'static str,
    pub logs_recent: &'static str,
    pub open_folder: &'static str,
    pub language: &'static str,
    pub language_detail: &'static str,
    pub troubleshooting: &'static str,
    pub last_run: &'static str,
    pub last_connection: &'static str,
    pub last_stream_problem: &'static str,
    pub last_stream_problem_detail: &'static str,
    pub connection_established: &'static str,
    pub no_completed_check: &'static str,
    pub try_stream_then: &'static str,
    pub current_checks: &'static str,
    pub windows_may_ask: &'static str,
    pub stopping: &'static str,
    pub cancel: &'static str,
    pub run_again: &'static str,
    pub troubleshoot: &'static str,
    pub fixing: &'static str,
    pub fix_connection: &'static str,
    pub send_results: &'static str,
    pub sending: &'static str,
    pub sent: &'static str,
    pub send_report: &'static str,
    pub copy_report: &'static str,
    pub try_stream_friend: &'static str,
    pub account: &'static str,
    pub video: &'static str,
    pub defaults: &'static str,
    pub system: &'static str,
    pub version: &'static str,
    pub done: &'static str,
}

#[derive(Clone, Copy)]
pub struct Update {
    pub update_now: &'static str,
    pub check_again: &'static str,
    pub check_now: &'static str,
    pub available_heading: &'static str,
    pub beta_ready: &'static str,
    pub downloading_heading: &'static str,
    pub will_restart: &'static str,
    pub paused_heading: &'static str,
    pub auto_off: &'static str,
    pub checking: &'static str,
    pub downloading: &'static str,
    pub is_available: &'static str,
    pub up_to_date: &'static str,
    pub up_to_date_ago: &'static str,
    pub just_now: &'static str,
    pub minute_ago: &'static str,
    pub minutes_ago: &'static str,
    pub hour_ago: &'static str,
    pub hours_ago: &'static str,
    pub could_not_check: &'static str,
    pub download_failed: &'static str,
    pub could_not_start: &'static str,
}

#[derive(Clone, Copy)]
pub struct Troubleshoot {
    pub headline_stopping: &'static str,
    pub headline_repair: &'static str,
    pub headline_running: &'static str,
    pub headline_found: &'static str,
    pub headline_idle: &'static str,
    pub summary_attention: &'static str,
    pub summary_ok: &'static str,
    pub summary_needs: &'static str,
    pub summary_incomplete: &'static str,
    pub looks_good: &'static str,
    pub needs_attention: &'static str,
    pub could_not_check: &'static str,
    pub firewall_fail_action: &'static str,
    pub check_runtime: &'static str,
    pub check_capture: &'static str,
    pub check_encoder: &'static str,
    pub check_decoder: &'static str,
    pub check_audio: &'static str,
    pub check_signalling: &'static str,
    pub check_stun: &'static str,
    pub check_ice: &'static str,
    pub check_firewall: &'static str,
    pub fail_reinstall: &'static str,
    pub fail_driver: &'static str,
    pub fail_internet: &'static str,
    pub fail_network: &'static str,
    pub fail_firewall: &'static str,
    pub fail_retry: &'static str,
    pub inconclusive_network: &'static str,
    pub inconclusive_firewall: &'static str,
    pub inconclusive_retry: &'static str,
    pub upload_sending: &'static str,
    pub upload_stopping: &'static str,
    pub upload_sent: &'static str,
    pub upload_retry: &'static str,
    pub upload_sign_in: &'static str,
}

#[derive(Clone, Copy)]
pub struct Notice {
    pub sign_in_manage_friends: &'static str,
    pub own_account: &'static str,
    pub already_friends: &'static str,
    pub they_sent_request: &'static str,
    pub request_pending: &'static str,
    pub change_saving: &'static str,
    pub wait_sync: &'static str,
    pub new_request: &'static str,
    pub request_sent: &'static str,
    pub request_accepted: &'static str,
    pub request_declined: &'static str,
    pub request_cancelled: &'static str,
    pub friend_removed: &'static str,
    pub friend_change_failed: &'static str,
    pub load_session: &'static str,
    pub load_preferences: &'static str,
    pub load_both: &'static str,
    pub diagnostics_missing: &'static str,
    pub diagnostics_open_failed: &'static str,
    pub report_copied: &'static str,
    pub already_a_friend: &'static str,
    pub sign_in_add_friends: &'static str,
    pub own_friend_code: &'static str,
    pub pending_request: &'static str,
    pub add_friend_failed: &'static str,
    pub copy_code_failed: &'static str,
    pub sign_in_share_code: &'static str,
    pub save_preferences: &'static str,
    pub sign_out_failed: &'static str,
    pub session_expired: &'static str,
    pub media_runtime_missing: &'static str,
    pub stream_ended: &'static str,
    pub no_clipboard_code: &'static str,
    pub already_watching: &'static str,
    pub own_stream_code: &'static str,
    pub watch_retry: &'static str,
    pub watch_retry_failed: &'static str,
}

impl Catalog {
    pub fn quality_detail(&self, index: usize) -> &'static str {
        self.settings
            .quality_details
            .get(index)
            .copied()
            .unwrap_or("")
    }

    pub fn fps_detail(&self, index: usize) -> &'static str {
        self.settings.fps_details.get(index).copied().unwrap_or("")
    }

    pub fn check_label(&self, id: &str) -> &'static str {
        match id {
            "runtime" => self.troubleshoot.check_runtime,
            "capture" => self.troubleshoot.check_capture,
            "encoder" => self.troubleshoot.check_encoder,
            "decoder" => self.troubleshoot.check_decoder,
            "audio" => self.troubleshoot.check_audio,
            "signalling" => self.troubleshoot.check_signalling,
            "stun" => self.troubleshoot.check_stun,
            "ice" => self.troubleshoot.check_ice,
            "firewall" => self.troubleshoot.check_firewall,
            _ => "",
        }
    }

    pub fn fail_action(&self, id: &str) -> &'static str {
        match id {
            "runtime" | "capture" | "audio" => self.troubleshoot.fail_reinstall,
            "encoder" | "decoder" => self.troubleshoot.fail_driver,
            "signalling" => self.troubleshoot.fail_internet,
            "stun" | "ice" => self.troubleshoot.fail_network,
            "firewall" => self.troubleshoot.fail_firewall,
            _ => self.troubleshoot.fail_retry,
        }
    }

    pub fn inconclusive_action(&self, id: &str) -> &'static str {
        match id {
            "stun" | "ice" => self.troubleshoot.inconclusive_network,
            "firewall" => self.troubleshoot.inconclusive_firewall,
            _ => self.troubleshoot.inconclusive_retry,
        }
    }

    pub fn checked_ago(&self, elapsed: Duration) -> String {
        let seconds = elapsed.as_secs();
        match seconds {
            0..=59 => self.update.just_now.to_string(),
            60..=3599 => {
                let n = seconds / 60;
                if n == 1 {
                    self.update.minute_ago.to_string()
                } else {
                    fill(self.update.minutes_ago, n)
                }
            }
            _ => {
                let n = seconds / 3600;
                if n == 1 {
                    self.update.hour_ago.to_string()
                } else {
                    fill(self.update.hours_ago, n)
                }
            }
        }
    }

    pub fn update_failure(&self, message: &str) -> String {
        if message == en::CATALOG.update.could_not_check
            || message == pt_br::CATALOG.update.could_not_check
        {
            self.update.could_not_check.to_string()
        } else if message == en::CATALOG.update.download_failed
            || message == pt_br::CATALOG.update.download_failed
        {
            self.update.download_failed.to_string()
        } else if message == en::CATALOG.update.could_not_start
            || message == pt_br::CATALOG.update.could_not_start
        {
            self.update.could_not_start.to_string()
        } else {
            message.to_string()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn windows_locale_names_select_portuguese_or_english() {
        // First launch follows the OS language. pt-PT still maps to pt-BR
        // because that is the only Portuguese catalog.
        assert_eq!(Locale::from_windows_name("pt-BR"), Locale::PtBr);
        assert_eq!(Locale::from_windows_name("pt-br"), Locale::PtBr);
        assert_eq!(Locale::from_windows_name("pt-PT"), Locale::PtBr);
        assert_eq!(Locale::from_windows_name("en-US"), Locale::En);
        assert_eq!(Locale::from_windows_name(""), Locale::En);
    }

    #[test]
    fn locale_round_trips_through_preferences_json() {
        assert_eq!(serde_json::to_string(&Locale::En).unwrap(), "\"en\"");
        assert_eq!(serde_json::to_string(&Locale::PtBr).unwrap(), "\"pt-BR\"");
        assert_eq!(
            serde_json::from_str::<Locale>("\"pt-BR\"").unwrap(),
            Locale::PtBr
        );
    }

    #[test]
    fn captions_in_every_language_fit_the_settings_card() {
        // Same 48-character budget as the English supervisor captions: a wrap
        // changes card height and shoves the list while a fade is running.
        const BUDGET: usize = 48;
        for locale in Locale::ALL {
            let copy = locale.catalog();
            for detail in copy
                .settings
                .quality_details
                .iter()
                .chain(copy.settings.fps_details.iter())
            {
                assert!(
                    detail.chars().count() <= BUDGET,
                    "{locale:?} caption over budget: {detail:?}"
                );
                assert!(
                    !detail.ends_with('.'),
                    "{locale:?} caption has a terminal period: {detail:?}"
                );
                assert!(!detail.is_empty(), "{locale:?} has an empty caption");
            }
        }
    }

    #[test]
    fn ago_wording_matches_the_settings_caption_style() {
        let en = Locale::En.catalog();
        assert_eq!(en.checked_ago(Duration::from_secs(0)), "just now");
        assert_eq!(en.checked_ago(Duration::from_secs(120)), "2 minutes ago");
        let pt = Locale::PtBr.catalog();
        assert_eq!(pt.checked_ago(Duration::from_secs(120)), "há 2 minutos");
    }
}
