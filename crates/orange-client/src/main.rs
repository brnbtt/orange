//! orange client - the host-side UI.
//!
//! GPUI fits here precisely because there is no video: this is ordinary UI.
//! The viewer window stays native because GPUI's `Surface` element has no
//! Windows implementation - its only variant is macOS-gated.

// Without this the binary is a console application and Windows opens a black
// cmd window behind the UI.
#![windows_subsystem = "windows"]

mod background;
mod capture;
mod client;
mod friends;
mod presence;
mod session;
mod sound;
mod supervisor;
mod troubleshoot;
mod ui;
mod update;
mod view;

use background::{AvatarJobs, FriendAvatarJobs, PickerEvent, PickerJobs};
use gpui::{
    prelude::*, px, size, App, Application, Bounds, Context, FocusHandle, Pixels, Point, Timer,
    TitlebarOptions, Window, WindowBounds, WindowOptions,
};
use std::cell::RefCell;
use std::rc::Rc;
use std::time::{Duration, Instant};
use supervisor::{LoginAttempt, Quality, Supervisor, WindowTarget, QUALITIES};

const DEFAULT_SERVER: &str =
    "wss://orange-relay.redmushroom-80c79f12.brazilsouth.azurecontainerapps.io/ws";
const APP_USER_MODEL_ID: &str = "brnbtt.orange";

fn set_taskbar_identity() -> windows::core::Result<()> {
    let app_id = windows::core::HSTRING::from(APP_USER_MODEL_ID);
    // SAFETY: HSTRING provides a valid NUL-terminated buffer that remains alive
    // for the duration of this call.
    unsafe {
        windows::Win32::UI::Shell::SetCurrentProcessExplicitAppUserModelID(windows::core::PCWSTR(
            app_id.as_ptr(),
        ))
    }
}

#[derive(PartialEq, Clone, Copy)]
enum Screen {
    SignedOut,
    Home,
    PickWindow,
    Streaming,
    Watching,
    Settings,
}

struct WatchSession {
    code: String,
    supervisor: Supervisor,
}

struct Notice {
    text: String,
    kind: NoticeKind,
    expires_at: Instant,
}

struct FriendMenu {
    friend_id: String,
    friend_name: String,
    anchor: Point<Pixels>,
    return_focus: Option<FocusHandle>,
    menu_focus: Option<FocusHandle>,
    mute_focus: Option<FocusHandle>,
    remove_focus: Option<FocusHandle>,
}

/// Whether a notice is reporting a failure or just saying what happened.
///
/// Everything used to be a failure, which is how a host ending their stream -
/// the most ordinary thing that can happen to a viewer - came to be announced
/// in red as `Error: The stream ended`.
#[derive(PartialEq, Clone, Copy)]
enum NoticeKind {
    Ordinary,
    Failure,
}

fn poll_login(
    mut load: impl FnMut() -> anyhow::Result<Option<session::Session>>,
    mut failure: impl FnMut() -> Option<String>,
) -> Option<Result<session::Session, String>> {
    if let Ok(Some(session)) = load() {
        return Some(Ok(session));
    }
    let reason = failure()?;
    Some(match load() {
        Ok(Some(session)) => Ok(session),
        Ok(None) => Err(reason),
        Err(error) => Err(format!("Could not read session: {error}")),
    })
}

/// How long the share code shows its "Copied" confirmation.
///
/// Shared with `Digest` so the tick that retires the confirmation is the same
/// tick that repaints it away. Two copies of this number and the label would
/// linger until something else happened to trigger a render.
const COPIED_FOR: Duration = Duration::from_secs(2);

fn begin_picker_refresh(
    windows: &mut Vec<WindowTarget>,
    thumbnails: &mut std::collections::HashMap<i64, std::sync::Arc<gpui::RenderImage>>,
    jobs: &mut PickerJobs,
) {
    // Enumeration no longer holds the UI: remove last visit's selectable
    // HWNDs before the new worker can run, not only when it returns a list.
    windows.clear();
    thumbnails.clear();
    jobs.request();
}

fn collect_met_friends(
    offers: &mut Vec<session::Friend>,
    status: &std::sync::Mutex<supervisor::StreamStatus>,
) {
    if let Ok(mut status) = status.lock() {
        for friend in status.met.drain(..) {
            session::remember_friend(offers, friend);
        }
    }
}

fn next_friend_offer<'a>(
    offers: &'a [session::Friend],
    friends: &[session::Friend],
    own_id: Option<&str>,
) -> Option<&'a session::Friend> {
    let own_id = own_id?;
    offers
        .iter()
        .find(|offer| offer.id != own_id && !friends.iter().any(|friend| friend.id == offer.id))
}

fn friend_started_streaming(
    previous: Option<&presence::Presence>,
    current: &presence::Presence,
) -> bool {
    matches!(
        (previous, current),
        (
            Some(presence::Presence::Offline),
            presence::Presence::Live { .. } | presence::Presence::Full
        )
    )
}

fn friend_presence_alert_cue(
    previous: &std::collections::HashMap<String, presence::Presence>,
    entries: &[presence::Entry],
    accepted_friend_ids: &std::collections::HashSet<String>,
    muted_friend_ids: &std::collections::HashSet<String>,
) -> Option<sound::Cue> {
    entries.iter().find_map(|entry| {
        if !accepted_friend_ids.contains(&entry.id) || muted_friend_ids.contains(&entry.id) {
            return None;
        }
        friend_started_streaming(previous.get(&entry.id), &entry.presence)
            .then_some(sound::Cue::FriendLive)
    })
}

struct Orange {
    client_available: bool,
    /// Receives keyboard navigation before a control has been focused.
    root_focus: Option<FocusHandle>,
    screen: Screen,
    session: Option<session::Session>,
    windows: Vec<WindowTarget>,
    /// Thumbnails keyed by window handle, filled in asynchronously.
    thumbnails: std::collections::HashMap<i64, std::sync::Arc<gpui::RenderImage>>,
    /// Results arriving from the capture thread.
    thumbnail_job: PickerJobs,
    /// Discord avatar decoded off the UI thread.
    avatar: Option<std::sync::Arc<gpui::RenderImage>>,
    avatar_job: AvatarJobs,
    quality: usize,
    fps: u32,
    active_target: Option<WindowTarget>,
    active_preview: Option<std::sync::Arc<gpui::RenderImage>>,
    host: Option<Supervisor>,
    watches: Vec<WatchSession>,
    logging_in: Option<LoginAttempt>,
    notice: Option<Notice>,
    server: String,
    /// Scroll position of the picker grid. Read during render so the view can
    /// tell whether there is anything below the fold, which a bare
    /// `overflow_y_scroll` gives no indication of.
    picker_scroll: gpui::ScrollHandle,
    /// The same, for the settings list.
    settings_scroll: gpui::ScrollHandle,
    /// Keep the newly focused friend visible when tabbing through a long roster.
    friends_scroll: gpui::ScrollHandle,
    /// Whether the update toast is collapsed to its heading. The toast cannot
    /// be dismissed, only folded away: an available update stays actionable.
    update_collapsed: bool,
    /// Which settings sections are expanded. Not persisted: opening settings
    /// with everything visible is the better default, and it keeps a transient
    /// view detail out of the preferences file.
    settings_open: [bool; 3],
    /// Drives the transient "Copied" confirmation on the share code.
    copied_at: Option<Instant>,
    copied_code: Option<String>,
    own_codes: Vec<String>,
    /// Viewer count at the last tick, so arrivals and departures can be told
    /// apart. Reset to zero whenever no stream is running.
    viewers_seen: usize,
    /// The roster this machine wants presence for.
    friends: Vec<session::Friend>,
    /// UI-owned offers survive child teardown and change inside the tick, so
    /// the before/after digest can actually notice an arriving identity.
    friend_offers: Vec<session::Friend>,
    friend_accounts: std::collections::HashMap<String, session::AccountFriends>,
    legacy_friends: Vec<session::Friend>,
    friend_sync: friends::Sync,
    requests_open: bool,
    friend_menu: Option<FriendMenu>,
    muted_stream_alert_friend_ids: std::collections::HashSet<String>,
    friends_panel_collapsed: bool,
    /// Last answer from the relay, keyed by Discord id. Absent means "not
    /// asked yet or the poll failed", which the view renders differently from
    /// a friend who is genuinely offline.
    presence: std::collections::HashMap<String, presence::Presence>,
    presence_job: Option<presence::PresenceJob>,
    retiring_presence_job: Option<presence::PresenceJob>,
    presence_revision: Option<String>,
    presence_client: presence::PresenceClient,
    presence_due: Instant,
    /// Why the last poll failed, if it did. Surfaced rather than swallowed:
    /// silently showing every friend offline is indistinguishable from every
    /// friend actually being offline.
    presence_error: Option<presence::PresenceError>,
    /// Friend pictures decoded off the UI thread, keyed by Discord id. Same
    /// shape as `thumbnails`, and for the same reason: an async fill of a
    /// keyed cache that render reads synchronously.
    friend_avatars: std::collections::HashMap<String, std::sync::Arc<gpui::RenderImage>>,
    friend_avatar_job: FriendAvatarJobs,
    logo_epoch: u64,
    /// Whether ambient animation should run: true only while this window is
    /// the active one.
    ///
    /// Any running animation forces a full repaint at 60fps, measured at about
    /// 12% of a CPU core on this window. That is a fair price while somebody
    /// is looking at the app and pure waste the moment they alt-tab away, so
    /// the decoration stops when the window loses focus and picks up again
    /// when it comes back. Read from the window during `render`, because GPUI
    /// refreshes on activation change and has no public observer for it.
    animate: bool,
    updates: update::UpdateController,
    troubleshoot: troubleshoot::TroubleshootState,
}

/// The parts of the app the view can actually see.
///
/// `tick` runs twice a second whether or not anything happened, and used to
/// end in an unconditional `cx.notify()`. That re-rendered every element in
/// the app twice a second forever, including while the window was hidden in
/// the client and there was nobody to show it to.
///
/// Comparing two of these costs a few dozen bytes and a handful of integer
/// compares, and turns a permanent background repaint into one that happens
/// when something moved. It is deliberately built from what `view.rs` reads
/// rather than from every field on `Orange`: a value the screens never render
/// cannot change what is on screen, and including it would only reintroduce
/// the wakeups this exists to remove.
#[derive(PartialEq)]
struct Digest {
    screen: Screen,
    notice: Option<(NoticeKind, String)>,
    logging_in: bool,
    signed_in_as: Option<String>,
    has_avatar: bool,
    hosting: bool,
    code: Option<String>,
    viewers: usize,
    watches: usize,
    windows: usize,
    thumbnails: usize,
    capturing: bool,
    picker_loading: bool,
    has_preview: bool,
    /// The share code's "Copied" confirmation expires on a timer rather than
    /// on an event, so the tick that retires it has to be the one that
    /// repaints.
    recently_copied: bool,
    update_status: String,
    /// Presence is polled on a timer, so the tick that learns a friend went
    /// live has to be the one that repaints the row. Without this the friends
    /// list would only refresh when something unrelated moved.
    presence: Vec<(String, Option<presence::Presence>)>,
    presence_error: Option<presence::PresenceError>,
    /// Friend pictures arrive one at a time from a background worker. Without
    /// this the rows would keep their initials until something else moved.
    friend_avatars: usize,
    /// The "keep this person?" offer appears when a child reports who it met,
    /// which is a background event with no click behind it.
    pending_friend: Option<String>,
    friend_sync: (
        friends::Snapshot,
        Option<presence::PresenceError>,
        bool,
        bool,
    ),
    friend_menu: Option<String>,
    troubleshoot_running: bool,
    troubleshoot_generation: u64,
}

impl Orange {
    fn load_friend_account(&mut self, id: &str) {
        self.close_friend_menu();
        let account = self.friend_accounts.entry(id.to_string()).or_default();
        for friend in self.legacy_friends.drain(..) {
            session::remember_friend(&mut account.suggestions, friend);
        }
        self.friends = account.friends.clone();
        self.friend_offers = account.suggestions.clone();
        self.muted_stream_alert_friend_ids = account
            .muted_stream_alert_friend_ids
            .iter()
            .cloned()
            .collect();
        self.friend_avatars.clear();
        self.presence.clear();
        self.presence_error = None;
        self.presence_revision = None;
        self.requests_open = false;
    }

    fn change_friend(&mut self, action: friends::Action, id: &str, revision: Option<String>) {
        let Some(session) = &self.session else {
            self.show_error("Sign in with Discord to manage friends.");
            return;
        };
        if session.id == id {
            self.show_error("That is your own account.");
            return;
        }
        if action == friends::Action::Request {
            if self.is_friend(id) {
                self.show_notice(NoticeKind::Ordinary, "You are already friends.");
                return;
            }
            if self
                .friend_sync
                .snapshot
                .incoming
                .iter()
                .any(|contact| contact.profile.id == id)
            {
                self.requests_open = true;
                self.screen = Screen::Home;
                self.show_notice(
                    NoticeKind::Ordinary,
                    "They already sent you a request. Accept it in Requests.",
                );
                return;
            }
            if self
                .friend_sync
                .snapshot
                .outgoing
                .iter()
                .any(|contact| contact.profile.id == id)
            {
                self.requests_open = true;
                self.screen = Screen::Home;
                self.show_notice(
                    NoticeKind::Ordinary,
                    "Your friend request is already pending.",
                );
                return;
            }
        }
        if !self.friend_sync.request(friends::Change {
            action,
            target_id: id.into(),
            revision,
        }) {
            self.show_notice(
                NoticeKind::Ordinary,
                if self.friend_sync.busy() {
                    "A friend change is being saved. Please wait."
                } else {
                    "Wait for friends to sync, then try again."
                },
            );
        }
    }

    fn open_friend_menu(
        &mut self,
        friend: &session::Friend,
        anchor: Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let return_focus = window.focused(cx);
        let menu_focus = cx.focus_handle().tab_stop(true);
        let mute_focus = cx.focus_handle().tab_stop(true);
        let remove_focus = cx.focus_handle().tab_stop(true);
        mute_focus.focus(window);
        self.friend_menu = Some(FriendMenu {
            friend_id: friend.id.clone(),
            friend_name: friend.name.clone(),
            anchor,
            return_focus,
            menu_focus: Some(menu_focus),
            mute_focus: Some(mute_focus),
            remove_focus: Some(remove_focus),
        });
    }

    fn close_friend_menu(&mut self) {
        self.friend_menu = None;
    }

    fn close_friend_menu_and_restore_focus(&mut self, window: &mut Window) {
        let focus = self
            .friend_menu
            .as_ref()
            .and_then(|menu| menu.return_focus.as_ref())
            .cloned();
        self.close_friend_menu();
        if let Some(focus) = focus {
            focus.focus(window);
        }
    }

    fn friend_menu_target(&self) -> Option<&session::Friend> {
        let id = self.friend_menu.as_ref()?.friend_id.as_str();
        self.friends.iter().find(|friend| friend.id == id)
    }

    fn remove_friend_from_menu(&mut self) -> Option<FocusHandle> {
        let id = self.friend_menu_target().map(|friend| friend.id.clone());
        let return_focus = self
            .friend_menu
            .as_ref()
            .and_then(|menu| menu.return_focus.clone());
        self.close_friend_menu();
        if let Some(id) = id {
            self.remove_friend(&id);
        }
        return_focus
    }

    fn dismiss_friend_menu_for_state(&mut self) {
        if self.screen != Screen::Home || self.requests_open || self.session.is_none() {
            self.close_friend_menu();
            return;
        }
        if self.friend_menu_target().is_none() {
            self.close_friend_menu();
        }
    }

    fn dismiss_friend_menu_if_focus_left(&mut self, window: &mut Window, cx: &App) {
        let Some(menu) = self.friend_menu.as_ref() else {
            return;
        };
        let keep_open = window.is_window_active()
            && [
                menu.menu_focus.as_ref(),
                menu.mute_focus.as_ref(),
                menu.remove_focus.as_ref(),
            ]
            .into_iter()
            .flatten()
            .any(|handle| handle.contains_focused(window, cx));
        if !keep_open {
            if window.is_window_active() {
                // Another control already owns focus; keep the user's place.
                self.close_friend_menu();
            } else {
                self.close_friend_menu_and_restore_focus(window);
            }
        }
    }

    fn friend_menu_has_focus(&self, window: &mut Window, cx: &App) -> bool {
        self.friend_menu
            .as_ref()
            .map(|menu| {
                [
                    menu.menu_focus.as_ref(),
                    menu.mute_focus.as_ref(),
                    menu.remove_focus.as_ref(),
                ]
                .into_iter()
                .flatten()
                .any(|handle| handle.contains_focused(window, cx))
            })
            .unwrap_or(false)
    }

    fn friend_stream_alerts_muted(&self, id: &str) -> bool {
        self.muted_stream_alert_friend_ids.contains(id)
    }

    fn set_friend_stream_alerts_muted(&mut self, id: &str, muted: bool) {
        if muted {
            self.muted_stream_alert_friend_ids.insert(id.to_string());
        } else {
            self.muted_stream_alert_friend_ids.remove(id);
        }
        self.save_preferences();
    }

    fn toggle_friend_stream_alerts_from_menu(&mut self) -> Option<FocusHandle> {
        let friend_id = self.friend_menu_target().map(|friend| friend.id.clone())?;
        let return_focus = self
            .friend_menu
            .as_ref()
            .and_then(|menu| menu.return_focus.clone());
        let muted = !self.friend_stream_alerts_muted(&friend_id);
        self.close_friend_menu();
        self.set_friend_stream_alerts_muted(&friend_id, muted);
        return_focus
    }

    fn toggle_friends_panel_collapsed(&mut self) {
        self.friends_panel_collapsed = !self.friends_panel_collapsed;
        self.save_preferences();
    }

    fn poll_friends(&mut self) {
        let before: Vec<_> = self
            .friend_sync
            .snapshot
            .incoming
            .iter()
            .map(|contact| contact.profile.id.clone())
            .collect();
        let was_synced = self.friend_sync.synced;
        let session = self
            .session
            .as_ref()
            .filter(|_| self.logging_in.is_none())
            .map(|s| (self.server.as_str(), s.token.as_str()));
        match self.friend_sync.poll(self.presence_client.clone(), session) {
            Some(friends::Event::Snapshot) => {
                let friends: Vec<_> = self
                    .friend_sync
                    .snapshot
                    .friends
                    .iter()
                    .map(|contact| contact.profile.clone())
                    .collect();
                let changed = friends != self.friends;
                self.friends = friends;
                self.friend_avatars
                    .retain(|id, _| self.friends.iter().any(|friend| &friend.id == id));
                self.presence
                    .retain(|id, _| self.friends.iter().any(|friend| &friend.id == id));
                self.friend_offers
                    .retain(|offer| !self.friends.iter().any(|friend| friend.id == offer.id));
                if changed || !was_synced {
                    self.save_preferences();
                    self.refresh_presence_now();
                }
                if was_synced
                    && self
                        .friend_sync
                        .snapshot
                        .incoming
                        .iter()
                        .any(|contact| !before.contains(&contact.profile.id))
                {
                    self.show_notice(
                        NoticeKind::Ordinary,
                        "New friend request. Open Requests on Home to respond.",
                    );
                }
            }
            Some(friends::Event::Changed(change)) => {
                self.dismiss_friend_offer(&change.target_id);
                let message = match change.action {
                    friends::Action::Request => {
                        "Friend request sent. They can accept it in Requests."
                    }
                    friends::Action::Accept => {
                        "Friend request accepted. You are now on each other's friend lists."
                    }
                    friends::Action::Decline => "Friend request declined.",
                    friends::Action::Cancel => "Friend request cancelled.",
                    friends::Action::Remove => "Friend removed from both friend lists.",
                };
                self.show_notice(NoticeKind::Ordinary, message);
                self.save_preferences();
            }
            Some(friends::Event::Failed { mutation }) => {
                if matches!(
                    self.friend_sync.error,
                    Some(presence::PresenceError::SignedOut)
                ) {
                    self.reject_session();
                } else if mutation {
                    if let Some(error) = &self.friend_sync.error {
                        self.show_error(format!("Friend change failed: {error}"));
                    }
                }
            }
            None => {}
        }
    }

    fn digest(&self) -> Digest {
        Digest {
            screen: self.screen,
            notice: self
                .notice
                .as_ref()
                .map(|notice| (notice.kind, notice.text.clone())),
            logging_in: self.logging_in.is_some(),
            signed_in_as: self.session.as_ref().map(|session| session.name.clone()),
            has_avatar: self.avatar.is_some(),
            hosting: self.host.is_some(),
            code: self.code(),
            viewers: self.viewers().len(),
            watches: self.watches.len(),
            windows: self.windows.len(),
            thumbnails: self.thumbnails.len(),
            capturing: self.picker_busy(),
            picker_loading: self.picker_loading(),
            has_preview: self.active_preview.is_some(),
            recently_copied: self.copied_at.is_some_and(|at| at.elapsed() < COPIED_FOR),
            update_status: self.updates.settings_detail(),
            presence: self
                .friends
                .iter()
                .map(|friend| (friend.id.clone(), self.presence.get(&friend.id).cloned()))
                .collect(),
            presence_error: self.presence_error.clone(),
            friend_avatars: self.friend_avatars.len(),
            pending_friend: self.pending_friend().map(|friend| friend.id),
            friend_sync: (
                self.friend_sync.snapshot.clone(),
                self.friend_sync.error.clone(),
                self.friend_sync.synced,
                self.friend_sync.busy(),
            ),
            friend_menu: self.friend_menu_target().map(|friend| friend.id.clone()),
            troubleshoot_running: self.troubleshoot.is_running(),
            troubleshoot_generation: self.troubleshoot.generation(),
        }
    }

    /// The ids this machine is willing to be discovered by. A host tells the
    /// relay this at `Host` time; the relay never learns it any other way.
    fn visible_to(&self) -> Vec<String> {
        self.friends
            .iter()
            .map(|friend| friend.id.clone())
            .collect()
    }

    /// A roster change needs a fresh snapshot rather than waiting on the old
    /// subscription. Retain one cancelled worker while its replacement starts.
    fn refresh_presence_now(&mut self) {
        self.presence_revision = None;
        if let Some(job) = self.presence_job.as_mut() {
            job.cancel();
        }
        if self.retiring_presence_job.is_none() {
            self.retiring_presence_job = self.presence_job.take();
        }
        self.presence_due = Instant::now();
    }

    fn poll_retiring_presence_job(&mut self) {
        if self
            .retiring_presence_job
            .as_ref()
            .is_some_and(presence::PresenceJob::is_finished)
        {
            if let Some(mut job) = self.retiring_presence_job.take() {
                job.join();
            }
        }
    }

    /// Collect and renew long-polls on the 500 ms UI tick. Failures and legacy
    /// replies use the slower interval instead of spinning immediate requests.
    fn poll_presence(&mut self) {
        self.poll_retiring_presence_job();
        if self.logging_in.is_some() {
            return;
        }
        if self
            .presence_job
            .as_ref()
            .is_some_and(|job| !job.is_finished())
        {
            return;
        }
        if let Some(job) = self.presence_job.as_ref() {
            let completed_at = Instant::now();
            let result = job.take_result();
            if let Some(mut job) = self.presence_job.take() {
                job.join();
            }
            match result {
                Some(Ok(snapshot)) => {
                    let accepted_friend_ids: std::collections::HashSet<_> = self
                        .friends
                        .iter()
                        .map(|friend| friend.id.clone())
                        .collect();
                    let cue = friend_presence_alert_cue(
                        &self.presence,
                        &snapshot.friends,
                        &accepted_friend_ids,
                        &self.muted_stream_alert_friend_ids,
                    );
                    self.presence_error = None;
                    self.absorb_profiles(&snapshot.friends);
                    self.presence = snapshot
                        .friends
                        .into_iter()
                        .map(|entry| (entry.id, entry.presence))
                        .collect();
                    self.presence_revision = snapshot.revision.clone();
                    self.presence_due = if snapshot.revision.is_some() {
                        completed_at
                    } else {
                        completed_at + presence::INTERVAL
                    };
                    if let Some(cue) = cue {
                        sound::play(cue);
                    }
                }
                Some(Err(presence::PresenceError::SignedOut)) => {
                    self.reject_session();
                    return;
                }
                Some(Err(error)) => {
                    self.presence_error = Some(error);
                    self.presence_due = completed_at + presence::INTERVAL;
                }
                // The worker was cancelled before it sent anything. Leave
                // the previous answer standing rather than blanking the
                // list on a race.
                None => {}
            }
        }

        if Instant::now() < self.presence_due {
            return;
        }

        let Some(session) = self.session.as_ref() else {
            self.presence.clear();
            self.presence_revision = None;
            return;
        };
        let Some(url) = presence::presence_url(&self.server) else {
            self.presence_error = Some(presence::PresenceError::Unreachable(format!(
                "cannot derive a presence URL from {}",
                self.server
            )));
            self.presence_due = Instant::now() + presence::INTERVAL;
            return;
        };
        if self.friends.is_empty() {
            self.presence.clear();
            self.presence_revision = None;
            self.presence_due = Instant::now() + presence::INTERVAL;
            return;
        }
        presence::start(
            &mut self.presence_job,
            self.presence_client.clone(),
            url,
            session.token.clone(),
            &self.friends,
            self.presence_revision.as_deref(),
        );
    }

    /// Refresh the cached Discord profile of any friend who is currently live.
    ///
    /// `identify` only ever returns the caller's own profile, so a friend going
    /// live is the only moment their name or picture can be re-learned. Without
    /// this, a friend who changes their avatar would show the old one until
    /// they were removed and added again.
    fn absorb_profiles(&mut self, entries: &[presence::Entry]) {
        let mut changed = false;
        for entry in entries {
            let Some(friend) = self.friends.iter_mut().find(|f| f.id == entry.id) else {
                continue;
            };
            if let Some(name) = entry.name.as_ref() {
                if &friend.name != name {
                    friend.name = name.clone();
                    changed = true;
                }
            }
            if entry.avatar_url.is_some() && friend.avatar_url != entry.avatar_url {
                friend.avatar_url = entry.avatar_url.clone();
                changed = true;
            }
        }
        if changed {
            self.save_preferences();
        }
    }

    /// Fetch pictures for friends that do not have one decoded yet.
    ///
    /// Only starts when nothing is in flight, so a roster larger than one poll
    /// interval cannot pile up overlapping workers.
    fn poll_friend_avatars(&mut self) {
        let wanted: Vec<(String, String)> = self
            .friends
            .iter()
            .filter(|friend| !self.friend_avatars.contains_key(&friend.id))
            .filter_map(|friend| {
                friend
                    .avatar_url
                    .clone()
                    .map(|url| (friend.id.clone(), url))
            })
            .collect();
        for (id, _, pixels) in self.friend_avatar_job.poll(&wanted, Instant::now()) {
            if let Some(image) = pixels.and_then(capture::to_image) {
                self.friend_avatars.insert(id, image);
            }
        }
    }
}

impl Orange {
    fn new(cx: &mut Context<Self>, client_available: bool) -> Self {
        update::cleanup_helpers();
        // The UI reflects state owned by child processes, so poll rather than
        // trying to push updates across process boundaries.
        cx.spawn(async move |this, cx| loop {
            Timer::after(Duration::from_millis(500)).await;
            if this.update(cx, |this, cx| this.tick(cx)).is_err() {
                break;
            }
        })
        .detach();

        let (session, session_error) = match session::load() {
            Ok(session) => (session, None),
            Err(error) => (None, Some(error)),
        };
        let (mut preferences, preference_error) = match session::load_preferences() {
            Ok(preferences) => (preferences, None),
            Err(error) => (session::Preferences::default(), Some(error)),
        };
        let startup_error = match (session_error, preference_error) {
            (Some(session_error), Some(preference_error)) => Some(format!(
                "Could not load session: {session_error}; could not load preferences: {preference_error}"
            )),
            (Some(error), None) => Some(format!("Could not load session: {error}")),
            (None, Some(error)) => Some(format!("Could not load preferences: {error}")),
            (None, None) => None,
        };
        let mut avatar_job = AvatarJobs::default();
        avatar_job.request(session.as_ref().and_then(|s| s.avatar_url.clone()));
        avatar_job.poll();
        let updates = update::UpdateController::new();
        let account = session
            .as_ref()
            .map(|session| preferences.friend_account(&session.id))
            .unwrap_or_default();
        Self {
            client_available,
            root_focus: None,
            screen: if session.is_some() {
                Screen::Home
            } else {
                Screen::SignedOut
            },
            session,
            windows: Vec::new(),
            thumbnails: std::collections::HashMap::new(),
            thumbnail_job: PickerJobs::default(),
            avatar: None,
            avatar_job,
            quality: preferences.quality.min(QUALITIES.len() - 1),
            fps: supervisor::supported_frame_rate(preferences.fps),
            active_target: None,
            active_preview: None,
            host: None,
            watches: Vec::new(),
            logging_in: None,
            notice: startup_error.map(|text| Notice {
                text,
                kind: NoticeKind::Failure,
                expires_at: Instant::now() + Duration::from_secs(4),
            }),
            server: std::env::var("ORANGE_SERVER").unwrap_or_else(|_| DEFAULT_SERVER.to_string()),
            picker_scroll: gpui::ScrollHandle::new(),
            settings_scroll: gpui::ScrollHandle::new(),
            friends_scroll: gpui::ScrollHandle::new(),
            update_collapsed: false,
            settings_open: [true; 3],
            copied_at: None,
            copied_code: None,
            own_codes: preferences.own_codes,
            viewers_seen: 0,
            friends: account.friends,
            friend_offers: account.suggestions,
            friend_accounts: preferences.friend_accounts,
            legacy_friends: preferences.friends,
            friend_sync: friends::Sync::default(),
            requests_open: false,
            friend_menu: None,
            muted_stream_alert_friend_ids: account
                .muted_stream_alert_friend_ids
                .into_iter()
                .collect(),
            friends_panel_collapsed: preferences.friends_panel_collapsed,
            presence: std::collections::HashMap::new(),
            presence_job: None,
            retiring_presence_job: None,
            presence_revision: None,
            presence_client: presence::PresenceClient::default(),
            // Ask immediately on startup rather than after one interval, so a
            // friend who is already live is on screen when the window opens.
            presence_due: Instant::now(),
            presence_error: None,
            friend_avatars: std::collections::HashMap::new(),
            friend_avatar_job: FriendAvatarJobs::default(),
            logo_epoch: 0,
            // Corrected on the first render, before anything is painted.
            animate: false,
            updates,
            troubleshoot: troubleshoot::TroubleshootState::default(),
        }
    }

    fn tick(&mut self, cx: &mut Context<Self>) {
        let before = self.digest();
        if self.screen != Screen::PickWindow {
            self.thumbnail_job.cancel();
        }
        let picker_changed = self.drain_thumbnails();
        self.poll_updates(cx);
        self.poll_friends();
        self.poll_presence();
        self.poll_friend_avatars();
        self.troubleshoot.poll();
        if let Some(pixels) = self.avatar_job.poll() {
            self.avatar = capture::to_image(pixels);
        }

        // Login happens in a child process; notice when it lands, and when it
        // dies without producing a session.
        if let Some(attempt) = self.logging_in.as_mut() {
            match poll_login(session::load, || attempt.failure()) {
                Some(Ok(session)) => {
                    if self.session.as_ref().map(|current| current.id.as_str())
                        != Some(session.id.as_str())
                    {
                        self.stop_host();
                        self.stop_all_watches();
                        self.troubleshoot.clear();
                        self.save_preferences();
                    }
                    // Reauthentication can finish after old child events have
                    // queued. Do not offer a previous account's encounters.
                    self.collect_friend_offers();
                    self.friend_offers.clear();
                    self.friend_sync.reset();
                    self.load_friend_account(&session.id);
                    self.avatar = None;
                    self.avatar_job.request(session.avatar_url.clone());
                    self.session = Some(session);
                    self.refresh_presence_now();
                    self.logging_in = None;
                    self.screen = Screen::Home;
                }
                Some(Err(error)) => {
                    self.show_error(error);
                    self.logging_in = None;
                }
                None => {}
            }
        }

        let host_notice = self
            .host
            .as_ref()
            .and_then(|host| host.status.lock().ok())
            .and_then(|mut status| status.notice.take());
        if let Some(notice) = host_notice {
            self.show_notice(NoticeKind::Ordinary, notice);
        }
        if self.host.as_mut().is_some_and(|host| !host.running()) {
            self.collect_friend_offers();
            let host_error = self
                .host
                .as_ref()
                .and_then(|host| host.status.lock().ok())
                .and_then(|status| status.error.clone());
            if let Some(error) = host_error {
                self.show_error(error);
            }
            self.host = None;
            self.active_target = None;
            self.active_preview = None;
            self.screen = if self.watches.is_empty() {
                Screen::Home
            } else {
                Screen::Watching
            };
        }

        let mut watch_error = None;
        let mut watch_ended = false;
        self.watches.retain_mut(|watch| {
            let running = watch.supervisor.running();
            collect_met_friends(&mut self.friend_offers, &watch.supervisor.status);
            if running {
                true
            } else {
                if let Ok(status) = watch.supervisor.status.lock() {
                    watch_error = status.error.clone().or(watch_error.take());
                    watch_ended |= status.ended;
                }
                false
            }
        });
        self.collect_friend_offers();
        if self.session.is_none() {
            self.friend_offers.clear();
        }
        if let Some(error) = watch_error {
            self.show_error(error);
        } else if watch_ended {
            // The host stopping is the ordinary way to stop watching. Said
            // plainly, in the same voice as everything else, because there is
            // nothing here for anyone to fix.
            sound::play(sound::Cue::Ended);
            self.show_notice(NoticeKind::Ordinary, "Stream ended");
        }
        if self.screen == Screen::Watching && self.watches.is_empty() {
            self.screen = Screen::Home;
        }

        // Viewers arriving and leaving is the one thing that happens entirely
        // while the user is looking at their game, so it is the moment sound
        // was added for. Counting is enough: two people swapping within the
        // same half-second is not worth a peer-id diff. This only runs while a
        // host is alive, or a stream ending would fire a departure cue for
        // everyone who was still watching.
        if self.host.is_some() {
            let viewers = self.viewers().len();
            match viewers.cmp(&self.viewers_seen) {
                std::cmp::Ordering::Greater => sound::play(sound::Cue::Joined),
                std::cmp::Ordering::Less => sound::play(sound::Cue::Left),
                std::cmp::Ordering::Equal => {}
            }
            self.viewers_seen = viewers;
        } else {
            self.viewers_seen = 0;
        }
        // A newly-issued room code is immediately ready to paste into chat.
        if let Some(code) = self.code() {
            if self.copied_code.as_deref() != Some(&code) {
                cx.write_to_clipboard(gpui::ClipboardItem::new_string(code.clone()));
                if !self.own_codes.contains(&code) {
                    self.own_codes.push(code.clone());
                    self.save_preferences();
                }
                self.copied_code = Some(code);
                self.copied_at = Some(Instant::now());
                // Going live is not instant - the child has to reach the relay
                // and be given a code - so by the time it happens the user has
                // usually looked away. Same reason the code is auto-copied here.
                sound::play(sound::Cue::Live);
            }
        }
        if self
            .notice
            .as_ref()
            .is_some_and(|notice| Instant::now() >= notice.expires_at)
        {
            self.notice = None;
        }
        // Only when something the screens can see actually moved. A poll that
        // finds nothing is not a reason to redraw the app.
        // Child exit can navigate away from a picker opened while watching or
        // hosting. Those transitions do not go through the Back button.
        if self.screen != Screen::PickWindow {
            self.thumbnail_job.cancel();
        }
        if picker_changed || self.digest() != before {
            cx.notify();
        }
    }

    fn poll_updates(&mut self, cx: &mut Context<Self>) {
        if let Some((info, installer)) = self.updates.poll_event() {
            match update::launch_updater(&info, &installer) {
                Ok(()) => {
                    self.stop_host();
                    self.stop_all_watches();
                    cx.quit();
                }
                Err(_) => self.updates.updater_launch_failed(),
            }
        }
        self.updates.schedule_periodic();
    }

    fn quality(&self) -> Quality {
        QUALITIES[self.quality.min(QUALITIES.len() - 1)]
    }

    /// Every failure the user is told about goes through here, which makes it
    /// the one place the alert cue is due.
    fn show_error(&mut self, message: impl Into<String>) {
        sound::play(sound::Cue::Alert);
        self.show_notice(NoticeKind::Failure, message);
    }

    /// State a fact without claiming anything went wrong. Silent by default:
    /// the caller knows which cue, if any, belongs to the thing it is reporting.
    fn show_notice(&mut self, kind: NoticeKind, message: impl Into<String>) {
        self.notice = Some(Notice {
            text: message.into(),
            kind,
            expires_at: Instant::now() + Duration::from_secs(4),
        });
    }

    fn clear_error(&mut self) {
        self.notice = None;
    }

    /// Open the folder holding this install's session diagnostics. Testers
    /// attach the contents to bug reports; nothing is uploaded automatically.
    fn open_diagnostics(&mut self) {
        let Some(directory) = supervisor::diagnostics_directory() else {
            self.show_error("Could not locate the diagnostics folder.");
            return;
        };
        if let Err(error) = supervisor::open_directory(&directory) {
            self.show_error(format!("Could not open diagnostics: {error}"));
        }
    }

    fn start_troubleshoot(&mut self) {
        if self
            .troubleshoot
            .start(&self.server, supervisor::diagnostics_directory())
        {
            self.clear_error();
        }
    }

    fn send_troubleshoot_report(&mut self) {
        if self.troubleshoot.send_report(
            &self.server,
            self.session.as_ref().map(|session| session.token.as_str()),
            supervisor::diagnostics_directory(),
            update::current_version(),
            update::build_label(),
        ) {
            self.clear_error();
        }
    }

    fn cancel_troubleshoot(&mut self) {
        self.troubleshoot.cancel();
    }

    fn troubleshoot_report(&self) -> Option<String> {
        self.troubleshoot
            .report_text(update::current_version(), update::build_label())
    }

    fn copy_troubleshoot_report(&mut self, cx: &mut Context<Self>) {
        let Some(report) = self.troubleshoot_report() else {
            return;
        };
        cx.write_to_clipboard(gpui::ClipboardItem::new_string(report));
        self.show_notice(NoticeKind::Ordinary, "Troubleshooting report copied.");
    }

    /// Offers belong to the UI, not to a playback process that may already
    /// have exited by the time the user comes back to Home.
    fn pending_friend(&self) -> Option<session::Friend> {
        let offers: Vec<_> = self
            .friend_offers
            .iter()
            .filter(|offer| {
                !self
                    .friend_sync
                    .snapshot
                    .incoming
                    .iter()
                    .chain(&self.friend_sync.snapshot.outgoing)
                    .any(|contact| contact.profile.id == offer.id)
            })
            .cloned()
            .collect();
        next_friend_offer(
            &offers,
            &self.friends,
            self.session.as_ref().map(|s| s.id.as_str()),
        )
        .cloned()
    }

    fn collect_friend_offers(&mut self) {
        if let Some(host) = self.host.as_ref() {
            collect_met_friends(&mut self.friend_offers, &host.status);
        }
        for watch in &self.watches {
            collect_met_friends(&mut self.friend_offers, &watch.supervisor.status);
        }
    }

    fn dismiss_friend_offer(&mut self, id: &str) {
        self.friend_offers.retain(|friend| friend.id != id);
    }

    fn offer_friend_code(&mut self, code: &str) {
        let Some(session) = self.session.as_ref() else {
            self.show_error("Sign in with Discord to add friends.");
            return;
        };
        match session::Friend::from_code(code) {
            Ok(friend) if friend.id == session.id => {
                self.show_error("That is your own friend code.")
            }
            Ok(friend) if self.is_friend(&friend.id) => {
                self.show_notice(
                    NoticeKind::Ordinary,
                    format!("{} is already a friend.", friend.name),
                );
            }
            Ok(friend)
                if self
                    .friend_sync
                    .snapshot
                    .incoming
                    .iter()
                    .chain(&self.friend_sync.snapshot.outgoing)
                    .any(|contact| contact.profile.id == friend.id) =>
            {
                self.requests_open = true;
                self.show_notice(
                    NoticeKind::Ordinary,
                    "There is already a pending request. Open it here to respond or cancel.",
                );
            }
            Ok(friend) => {
                self.requests_open = false;
                self.dismiss_friend_offer(&friend.id);
                self.friend_offers.insert(0, friend);
                self.friend_offers.truncate(100);
                self.clear_error();
            }
            Err(error) => self.show_error(format!("Could not add friend: {error}")),
        }
    }

    /// Keep someone met through a code join.
    ///
    /// Deliberately not automatic. A code gets pasted into group chats, so
    /// auto-adding would hand a permanent view of when you stream to everyone
    /// who ever clicked it out of curiosity. The user decides.
    fn add_friend(&mut self, friend: session::Friend) {
        self.change_friend(friends::Action::Request, &friend.id, None);
    }

    /// Forget someone, and stop showing them as live.
    ///
    /// The stale presence entry has to go with them: it is keyed by id, and
    /// re-adding the same person would otherwise show whatever state was last
    /// seen before the removal.
    fn remove_friend(&mut self, id: &str) {
        let revision = self
            .friend_sync
            .snapshot
            .friends
            .iter()
            .find(|contact| contact.profile.id == id)
            .map(|contact| contact.revision.clone());
        self.change_friend(friends::Action::Remove, id, revision);
    }

    fn is_friend(&self, id: &str) -> bool {
        self.friends.iter().any(|friend| friend.id == id)
    }

    fn save_preferences(&mut self) -> bool {
        if let Some(session) = &self.session {
            let mut muted_stream_alert_friend_ids: Vec<String> =
                self.muted_stream_alert_friend_ids.iter().cloned().collect();
            muted_stream_alert_friend_ids.sort();
            self.friend_accounts.insert(
                session.id.clone(),
                session::AccountFriends {
                    friends: self.friends.clone(),
                    suggestions: self.friend_offers.clone(),
                    muted_stream_alert_friend_ids,
                },
            );
        }
        let preferences = session::Preferences {
            quality: self.quality,
            fps: Some(self.fps),
            own_codes: self.own_codes.clone(),
            friends: self.legacy_friends.clone(),
            friend_accounts: self.friend_accounts.clone(),
            friends_panel_collapsed: self.friends_panel_collapsed,
        };
        if let Err(error) = session::save_preferences(&preferences) {
            self.show_error(format!("Could not save preferences: {error}"));
            return false;
        }
        true
    }

    fn sign_out(&mut self, destination: Option<Screen>) {
        if let Err(error) = session::clear() {
            self.show_error(format!("Could not sign out: {error}"));
            return;
        }
        // Children authenticate once at startup. Keeping them across signout
        // would attribute encounters from account A's stream to account B.
        self.stop_host();
        self.stop_all_watches();
        self.save_preferences();
        self.session = None;
        self.friend_sync.reset();
        self.friends.clear();
        self.friend_avatars.clear();
        self.requests_open = false;
        self.friend_offers.clear();
        self.avatar = None;
        self.avatar_job.request(None);
        self.thumbnail_job.cancel();
        self.refresh_presence_now();
        if let Some(job) = self.retiring_presence_job.as_mut() {
            job.cancel();
        }
        self.presence.clear();
        self.presence_error = None;
        self.presence_revision = None;
        self.muted_stream_alert_friend_ids.clear();
        if let Some(destination) = destination {
            self.screen = destination;
        }
        self.troubleshoot.clear();
    }

    fn reject_session(&mut self) {
        self.sign_out(Some(Screen::SignedOut));
        if self.session.is_none() {
            self.show_notice(
                NoticeKind::Ordinary,
                "Your session expired. Sign in with Discord to reconnect.",
            );
        }
    }

    fn picker_busy(&self) -> bool {
        self.thumbnail_job.is_busy()
    }

    fn picker_loading(&self) -> bool {
        self.thumbnail_job.is_loading()
    }

    fn refresh_windows(&mut self) {
        begin_picker_refresh(
            &mut self.windows,
            &mut self.thumbnails,
            &mut self.thumbnail_job,
        );
        self.drain_thumbnails();
    }

    /// Move any captured thumbnails into the map. Runs on the UI thread, which
    /// is where GPUI's image types have to be built.
    fn drain_thumbnails(&mut self) -> bool {
        let mut changed = false;
        for event in self.thumbnail_job.poll() {
            match event {
                PickerEvent::Windows(windows) => {
                    self.windows = windows;
                    self.thumbnails.clear();
                    self.clear_error();
                    changed = true;
                }
                PickerEvent::Thumbnail(hwnd, thumb) => {
                    if let Some(image) = capture::to_image(thumb) {
                        self.thumbnails.insert(hwnd, image);
                        changed = true;
                    }
                }
                PickerEvent::Failed(error) => {
                    self.show_error(error);
                    changed = true;
                }
            }
        }
        changed
    }

    fn start_login(&mut self) {
        if self.logging_in.is_some() {
            return;
        }
        self.friend_sync.reset();
        self.refresh_presence_now();
        self.clear_error();
        match supervisor::start_login(&self.server) {
            Ok(attempt) => self.logging_in = Some(attempt),
            Err(err) => self.show_error(err.to_string()),
        }
    }

    fn start_stream(&mut self, target: WindowTarget) {
        self.thumbnail_job.cancel();
        if !supervisor::gstreamer_available() {
            self.show_error(supervisor::MEDIA_RUNTIME_MISSING.to_string());
            return;
        }
        let preview = self.thumbnails.get(&target.hwnd).cloned();
        match Supervisor::host(
            &target,
            &self.quality(),
            self.fps,
            &self.server,
            &self.visible_to(),
        ) {
            Ok(stream) => {
                self.active_target = Some(target);
                self.active_preview = preview;
                self.host = Some(stream);
                self.copied_code = None;
                self.copied_at = None;
                self.screen = Screen::Streaming;
                self.clear_error();
            }
            Err(err) => self.show_error(err.to_string()),
        }
    }

    fn join(&mut self, code: String) {
        let code = code.trim().to_ascii_uppercase();
        if code.is_empty() {
            self.show_error("No code on the clipboard");
            return;
        }
        if self.watches.iter().any(|watch| watch.code == code) {
            self.show_error(format!("Already watching {code}"));
            return;
        }
        if self.own_codes.contains(&code) {
            self.show_error("That's your own active or previous stream code.");
            return;
        }
        if !supervisor::gstreamer_available() {
            self.show_error(supervisor::MEDIA_RUNTIME_MISSING.to_string());
            return;
        }
        match Supervisor::watch(&code, &self.server, self.watches.len()) {
            Ok(stream) => {
                self.watches.push(WatchSession {
                    code,
                    supervisor: stream,
                });
                if self.host.is_none() && self.screen != Screen::Home {
                    // Joining from the friends list on Home stays there, so a
                    // second friend is one more click rather than a click and
                    // a Back. The row itself flips to "Watching", which is the
                    // feedback the screen change used to provide.
                    self.thumbnail_job.cancel();
                    self.screen = Screen::Watching;
                }
                self.clear_error();
                // The viewer window takes a moment to negotiate and appear, so
                // this says the code was accepted before there is anything to
                // look at.
                sound::play(sound::Cue::Live);
            }
            Err(err) => self.show_error(err.to_string()),
        }
    }

    fn stop_host(&mut self) {
        self.thumbnail_job.cancel();
        if let Some(mut host) = self.host.take() {
            host.stop();
            collect_met_friends(&mut self.friend_offers, &host.status);
        }
        self.active_target = None;
        self.active_preview = None;
        self.copied_code = None;
        self.copied_at = None;
        self.screen = if self.watches.is_empty() {
            Screen::Home
        } else {
            Screen::Watching
        };
    }

    fn stop_watch(&mut self, index: usize) {
        if index < self.watches.len() {
            let mut watch = self.watches.remove(index);
            watch.supervisor.stop();
            collect_met_friends(&mut self.friend_offers, &watch.supervisor.status);
        }
        if self.watches.is_empty() && self.host.is_none() {
            self.thumbnail_job.cancel();
            self.screen = Screen::Home;
        }
    }

    fn stop_all_watches(&mut self) {
        for watch in &mut self.watches {
            watch.supervisor.stop();
            collect_met_friends(&mut self.friend_offers, &watch.supervisor.status);
        }
        self.watches.clear();
        if self.host.is_none() {
            self.thumbnail_job.cancel();
            self.screen = Screen::Home;
        }
    }

    fn leave_picker(&mut self, destination: Screen) {
        self.thumbnail_job.cancel();
        self.screen = destination;
    }

    fn code(&self) -> Option<String> {
        self.host
            .as_ref()
            .and_then(|s| s.status.lock().ok())
            .and_then(|st| st.code.clone())
    }

    fn viewers(&self) -> Vec<String> {
        self.host
            .as_ref()
            .and_then(|s| s.status.lock().ok())
            .map(|st| st.viewers.clone())
            .unwrap_or_default()
    }
}

impl Drop for Orange {
    fn drop(&mut self) {
        // Request every cancellation before field Drops join, so a slow picker
        // cannot leave an entire avatar batch running during shutdown.
        self.thumbnail_job.cancel();
        self.avatar_job.request(None);
        self.friend_avatar_job.cancel();
        if let Some(job) = self.presence_job.as_mut() {
            job.cancel();
        }
        if let Some(job) = self.retiring_presence_job.as_mut() {
            job.cancel();
        }
        self.troubleshoot.cancel();
    }
}

type ClientOwner = Rc<RefCell<Option<client::Client>>>;

fn shutdown_owned_client(owner: &ClientOwner) -> anyhow::Result<()> {
    shutdown_owned_client_with(owner, client::Client::shutdown)
}

fn finish_owned_client(owner: &ClientOwner) -> anyhow::Result<()> {
    shutdown_owned_client_with(owner, client::Client::shutdown_final)
}

fn shutdown_owned_client_with(
    owner: &ClientOwner,
    shutdown: impl FnOnce(&mut client::Client) -> anyhow::Result<()>,
) -> anyhow::Result<()> {
    let Some(mut client) = owner.borrow_mut().take() else {
        return Ok(());
    };
    if let Err(error) = shutdown(&mut client) {
        owner.borrow_mut().replace(client);
        return Err(error);
    }
    Ok(())
}

fn main() {
    // Must precede every window so the client and viewer processes share one
    // taskbar group despite being different executables.
    if let Err(error) = set_taskbar_identity() {
        eprintln!("[client] could not set taskbar identity: {error}");
    }

    // Diagnostic: capture every window and report, since a windowsgui binary
    // has no console to print to.
    if std::env::args().any(|a| a == "--test-capture") {
        let mut report = String::new();
        match supervisor::list_windows() {
            Ok(windows) => {
                for w in windows {
                    match capture::thumbnail(w.hwnd as isize, 320, 180) {
                        Some((tw, th, bytes)) => report.push_str(&format!(
                            "OK    {tw}x{th} {} bytes   {}\n",
                            bytes.len(),
                            w.title
                        )),
                        None => {
                            report.push_str(&format!("FAIL                      {}\n", w.title))
                        }
                    }
                }
            }
            Err(err) => report.push_str(&format!("list failed: {err}\n")),
        }
        let path = std::env::temp_dir().join("orange-capture-test.txt");
        let _ = std::fs::write(path, report);
        return;
    }

    // A second launch should surface the window that already exists rather than
    // start a rival process. Checked before the client so the loser exits without
    // ever adding a second icon.
    if client::defer_to_running_instance() {
        return;
    }

    // Installed before the UI so a failure here is visible as a missing icon
    // rather than a half-started app.
    let client_owner = Rc::new(RefCell::new(client::Client::install().ok()));
    let client_available = client_owner.borrow().is_some();
    let app_client_owner = Rc::clone(&client_owner);

    Application::new().run(move |cx: &mut App| {
        let quit_owner = Rc::clone(&app_client_owner);
        cx.on_app_quit(move |_| {
            let quit_owner = Rc::clone(&quit_owner);
            async move {
                if let Err(error) = shutdown_owned_client(&quit_owner) {
                    eprintln!("[client] app-quit shutdown failed: {error:#}");
                }
            }
        })
        // GPUI subscriptions cancel on Drop; detaching retains this observer
        // until the App emitter itself is dropped.
        .detach();

        let bounds = Bounds::centered(None, size(px(ui::WINDOW_WIDTH), px(ui::WINDOW_HEIGHT)), cx);
        let window = cx
            .open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(bounds)),
                    titlebar: Some(TitlebarOptions {
                        title: Some("orange".into()),
                        // Hide the system titlebar so we can draw our own.
                        // GPUI documents this as supported on Windows.
                        appears_transparent: true,
                        traffic_light_position: None,
                    }),
                    // Every screen is laid out for exactly this size and the
                    // window never changes size again, so there is no minimum
                    // to declare - `window_min_size` was set to 360x480 here,
                    // below every size the app actually used, and was inert
                    // anyway because the window is not resizable.
                    is_resizable: false,
                    ..Default::default()
                },
                |_, cx| cx.new(|cx| Orange::new(cx, client_available)),
            )
            .unwrap();
        cx.activate(true);

        // Closing the window hides it instead of quitting: a client app should
        // keep streaming when its window is dismissed. Quit lives in the client
        // menu.
        let _ = window.update(cx, |_, window, cx| {
            window.on_window_should_close(cx, move |window, _cx| {
                if client_available {
                    window.minimize_window();
                    false
                } else {
                    true
                }
            });
        });

        // The client runs its own Win32 message loop on another thread, so its
        // events arrive over a channel and are drained on a timer here.
        if client_available {
            let event_owner = Rc::clone(&app_client_owner);
            cx.spawn(async move |cx| loop {
                Timer::after(Duration::from_millis(200)).await;
                loop {
                    let event = match event_owner.borrow().as_ref().map(client::Client::try_recv) {
                        None => return,
                        Some(Ok(event)) => event,
                        Some(Err(std::sync::mpsc::TryRecvError::Empty)) => break,
                        Some(Err(std::sync::mpsc::TryRecvError::Disconnected)) => return,
                    };
                    match event {
                        client::ClientEvent::Show => {
                            // The window may be hidden rather than merely
                            // unfocused, so un-hide before activating.
                            client::show_main_window();
                            let _ = cx.update(|cx| {
                                let _ = window.update(cx, |view, window, cx| {
                                    view.logo_epoch = view.logo_epoch.wrapping_add(1);
                                    cx.notify();
                                    window.activate_window();
                                });
                            });
                        }
                        client::ClientEvent::Quit => {
                            if let Err(error) = shutdown_owned_client(&event_owner) {
                                eprintln!("[client] client-quit shutdown failed: {error:#}");
                            }
                            let _ = cx.update(|cx| cx.quit());
                            return;
                        }
                    }
                }
            })
            .detach();
        }
    });

    if let Err(error) = finish_owned_client(&client_owner) {
        client::fail_fast(
            "final app shutdown could not clean up client ownership",
            &error,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn friend_test_app() -> Orange {
        Orange {
            client_available: false,
            root_focus: None,
            screen: Screen::Home,
            session: Some(login_session()),
            windows: Vec::new(),
            thumbnails: Default::default(),
            thumbnail_job: Default::default(),
            avatar: None,
            avatar_job: Default::default(),
            quality: 1,
            fps: 60,
            active_target: None,
            active_preview: None,
            host: None,
            watches: Vec::new(),
            logging_in: None,
            notice: None,
            server: DEFAULT_SERVER.into(),
            picker_scroll: gpui::ScrollHandle::new(),
            settings_scroll: gpui::ScrollHandle::new(),
            friends_scroll: gpui::ScrollHandle::new(),
            update_collapsed: false,
            settings_open: [true; 3],
            copied_at: None,
            copied_code: None,
            own_codes: Vec::new(),
            viewers_seen: 0,
            friends: Vec::new(),
            friend_offers: Vec::new(),
            friend_accounts: Default::default(),
            legacy_friends: Vec::new(),
            friend_sync: friends::Sync::default(),
            requests_open: false,
            friend_menu: None,
            muted_stream_alert_friend_ids: Default::default(),
            friends_panel_collapsed: false,
            presence: Default::default(),
            presence_job: None,
            retiring_presence_job: None,
            presence_revision: None,
            presence_client: Default::default(),
            presence_due: Instant::now(),
            presence_error: None,
            friend_avatars: Default::default(),
            friend_avatar_job: Default::default(),
            logo_epoch: 0,
            animate: false,
            updates: update::UpdateController::new(),
            troubleshoot: troubleshoot::TroubleshootState::default(),
        }
    }

    #[test]
    fn sending_a_request_does_not_locally_create_a_friendship() {
        // The first implementation immediately saved a one-sided friendship.
        // Only the relay's accepted snapshot may now populate the roster.
        let mut app = friend_test_app();
        app.friend_sync.synced = true;
        app.offer_friend_code("orange-friend:1:42:Friend");
        let offered = app.pending_friend().unwrap();
        app.add_friend(offered.clone());
        assert!(app.friends.is_empty());
        assert_eq!(app.pending_friend(), Some(offered.clone()));
        assert!(app.friend_sync.busy());
        assert!(app.watches.is_empty());
        assert!(app.host.is_none());
    }

    #[test]
    fn rejected_friend_sessions_sign_out_but_storage_outages_keep_the_login() {
        // Automatic logout must clear the real CLI session file, but tests
        // must never remove the developer's credentials or change APPDATA for
        // concurrent tests in this process.
        const CHILD: &str = "ORANGE_TEST_FRIEND_AUTH";
        if std::env::var_os(CHILD).is_none() {
            let dir = tempfile::tempdir().unwrap();
            let mut child = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "tests::rejected_friend_sessions_sign_out_but_storage_outages_keep_the_login",
                    "--nocapture",
                ])
                .env(CHILD, "1")
                .env("APPDATA", dir.path())
                .spawn()
                .unwrap();
            let deadline = Instant::now() + Duration::from_secs(10);
            loop {
                if let Some(status) = child.try_wait().unwrap() {
                    assert!(status.success());
                    return;
                }
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    panic!("friend auth child timed out");
                }
                std::thread::sleep(Duration::from_millis(10));
            }
        }
        let path = std::path::PathBuf::from(std::env::var_os("APPDATA").unwrap())
            .join("orange/session.json");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            &path,
            r#"{"id":"123","name":"Orange User","token":"token","avatar_url":null}"#,
        )
        .unwrap();
        let server = background::tests::HttpServer::new(vec![
            (503, b"storage unavailable".to_vec()),
            (401, vec![]),
        ]);
        let mut app = friend_test_app();
        app.server = server.url("/ws").replacen("http://", "ws://", 1);
        let deadline = Instant::now() + Duration::from_secs(5);
        while app.friend_sync.error.is_none() {
            app.poll_friends();
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(1));
        }
        assert!(app.session.is_some());
        assert!(session::load().unwrap().is_some());
        app.friend_sync.refresh();
        while app.session.is_some() {
            app.poll_friends();
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(1));
        }
        assert!(app.screen == Screen::SignedOut);
        assert!(session::load().unwrap().is_none());
        assert!(!app.friend_sync.synced);
        server.finish();
    }

    fn friend(id: &str) -> session::Friend {
        session::Friend {
            id: id.into(),
            name: format!("Friend {id}"),
            avatar_url: None,
        }
    }

    fn presence_entry(id: &str, presence: presence::Presence) -> presence::Entry {
        presence::Entry {
            id: id.into(),
            name: None,
            avatar_url: None,
            presence,
        }
    }

    #[test]
    fn stopping_children_keeps_their_last_friend_offer_available() {
        // Exercise real stdout readers and each explicit teardown path. A
        // mutex-only test cannot catch dropping the supervisor before draining
        // its final output, including output from an already-exited child.
        for mode in ["watch", "all", "host", "exited"] {
            let mut app = friend_test_app();
            let mut child = supervisor::friend_test_child(mode != "exited");
            let deadline = Instant::now() + Duration::from_secs(10);
            loop {
                let running = child.running();
                let ready = !child.status.lock().unwrap().met.is_empty();
                if ready && (mode != "exited" || !running) {
                    break;
                }
                assert!(
                    Instant::now() < deadline,
                    "friend child timed out in {mode}"
                );
                std::thread::sleep(Duration::from_millis(10));
            }
            if mode == "host" {
                app.host = Some(child);
                app.stop_host();
            } else {
                app.watches.push(WatchSession {
                    code: "ABC-234".into(),
                    supervisor: child,
                });
                if mode == "all" {
                    app.stop_all_watches();
                } else {
                    app.stop_watch(0);
                }
            }
            assert_eq!(
                app.pending_friend().unwrap().id,
                "42",
                "lost offer in {mode}"
            );
            assert!(app.host.is_none());
            assert!(app.watches.is_empty());
            app.dismiss_friend_offer("42");
            assert!(app.pending_friend().is_none());
        }
    }

    #[test]
    fn an_existing_friend_does_not_hide_another_streams_add_offer() {
        // The old find_map selected the first host and only then filtered
        // known friends, hiding every later unknown host and joining viewer.
        let offers = vec![friend("self"), friend("known"), friend("new")];
        let known = vec![friend("known")];
        assert_eq!(
            next_friend_offer(&offers, &known, Some("self")).unwrap().id,
            "new"
        );
        assert!(next_friend_offer(&offers, &known, None).is_none());
    }

    #[test]
    fn friend_menu_stays_bound_to_id_when_list_order_changes() {
        // A poll can reorder rows while their contextual action is open.
        let mut app = friend_test_app();
        app.friends = vec![friend("a"), friend("b")];
        app.friend_menu = Some(FriendMenu {
            friend_id: "a".into(),
            friend_name: "Friend a".into(),
            anchor: gpui::point(px(40.0), px(40.0)),
            return_focus: None,
            menu_focus: None,
            mute_focus: None,
            remove_focus: None,
        });

        app.friends.swap(0, 1);

        assert_eq!(
            app.friend_menu_target().map(|friend| friend.id.as_str()),
            Some("a")
        );
    }

    #[test]
    fn friend_menu_dismisses_when_target_no_longer_exists() {
        // Removal on another device must retire an already-open menu here.
        let mut app = friend_test_app();
        app.friends = vec![friend("a")];
        app.friend_menu = Some(FriendMenu {
            friend_id: "a".into(),
            friend_name: "Friend a".into(),
            anchor: gpui::point(px(40.0), px(40.0)),
            return_focus: None,
            menu_focus: None,
            mute_focus: None,
            remove_focus: None,
        });

        app.friends.clear();
        app.dismiss_friend_menu_for_state();

        assert!(app.friend_menu.is_none());
    }

    #[test]
    fn removing_from_friend_menu_uses_selected_id_and_closes_the_menu() {
        // Only the selected friend has a server revision; targeting the other
        // row after a reorder would fail to enqueue this removal.
        let mut app = friend_test_app();
        let selected = friend("a");
        app.friends = vec![selected.clone(), friend("b")];
        app.friend_sync.synced = true;
        app.friend_sync.snapshot.friends = vec![friends::Contact {
            profile: selected,
            revision: "rev-a".into(),
        }];
        app.friend_menu = Some(FriendMenu {
            friend_id: "a".into(),
            friend_name: "Friend a".into(),
            anchor: gpui::point(px(40.0), px(40.0)),
            return_focus: None,
            menu_focus: None,
            mute_focus: None,
            remove_focus: None,
        });

        app.friends.swap(0, 1);
        let _ = app.remove_friend_from_menu();

        assert!(app.friend_menu.is_none());
        assert!(app.friend_sync.busy());
    }

    #[test]
    fn muting_from_friend_menu_uses_selected_id_after_reorder() {
        let mut app = friend_test_app();
        app.friends = vec![friend("a"), friend("b")];
        app.friend_menu = Some(FriendMenu {
            friend_id: "a".into(),
            friend_name: "Friend a".into(),
            anchor: gpui::point(px(40.0), px(40.0)),
            return_focus: None,
            menu_focus: None,
            mute_focus: None,
            remove_focus: None,
        });

        app.friends.swap(0, 1);
        let _ = app.toggle_friend_stream_alerts_from_menu();

        assert!(app.friend_menu.is_none());
        assert!(app.friend_stream_alerts_muted("a"));
        assert!(!app.friend_stream_alerts_muted("b"));
    }

    #[test]
    fn friend_live_alert_only_fires_on_offline_to_live_or_full_transitions() {
        let previous =
            std::collections::HashMap::from([("42".to_string(), presence::Presence::Offline)]);
        let accepted = std::collections::HashSet::from(["42".to_string()]);
        let muted = std::collections::HashSet::new();

        assert_eq!(
            friend_presence_alert_cue(
                &previous,
                &[presence_entry(
                    "42",
                    presence::Presence::Live {
                        code: "ABC-234".into(),
                    }
                )],
                &accepted,
                &muted,
            ),
            Some(sound::Cue::FriendLive)
        );
        assert_eq!(
            friend_presence_alert_cue(
                &previous,
                &[presence_entry("42", presence::Presence::Full)],
                &accepted,
                &muted,
            ),
            Some(sound::Cue::FriendLive)
        );
        assert_eq!(
            friend_presence_alert_cue(
                &std::collections::HashMap::new(),
                &[presence_entry("42", presence::Presence::Full)],
                &accepted,
                &muted,
            ),
            None
        );
        assert_eq!(
            friend_presence_alert_cue(
                &std::collections::HashMap::from([(
                    "42".to_string(),
                    presence::Presence::Live {
                        code: "ABC-234".into(),
                    }
                )]),
                &[presence_entry("42", presence::Presence::Full)],
                &accepted,
                &muted,
            ),
            None
        );
    }

    #[test]
    fn friend_live_alert_ignores_muted_and_unknown_friends() {
        let previous =
            std::collections::HashMap::from([("42".to_string(), presence::Presence::Offline)]);
        assert_eq!(
            friend_presence_alert_cue(
                &previous,
                &[presence_entry(
                    "42",
                    presence::Presence::Live {
                        code: "ABC-234".into(),
                    }
                )],
                &std::collections::HashSet::new(),
                &std::collections::HashSet::new(),
            ),
            None
        );
        assert_eq!(
            friend_presence_alert_cue(
                &previous,
                &[presence_entry(
                    "42",
                    presence::Presence::Live {
                        code: "ABC-234".into(),
                    }
                )],
                &std::collections::HashSet::from(["42".to_string()]),
                &std::collections::HashSet::from(["42".to_string()]),
            ),
            None
        );
    }

    #[test]
    fn revision_presence_completion_can_start_next_poll_without_waiting_for_tick_interval() {
        let server = background::tests::HttpServer::new(vec![
            (
                200,
                br#"{"friends":[{"id":"42","state":"offline"}],"revision":"0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"}"#.to_vec(),
            ),
            (
                200,
                br#"{"friends":[{"id":"42","state":"offline"}],"revision":"fedcba9876543210fedcba9876543210fedcba9876543210fedcba9876543210"}"#.to_vec(),
            ),
        ]);
        let mut app = friend_test_app();
        app.server = server.url("/ws").replacen("http://", "ws://", 1);
        app.friends = vec![friend("42")];
        app.presence_due = Instant::now();

        app.poll_presence();
        app.presence_job.as_mut().unwrap().join();
        app.poll_presence();

        assert!(
            app.presence_job.is_some(),
            "next long poll did not start immediately"
        );
        let _ = server.finish();
    }

    #[test]
    fn legacy_presence_reply_uses_interval_backoff_instead_of_immediate_renewal() {
        let server = background::tests::HttpServer::new(vec![(
            200,
            br#"{"friends":[{"id":"42","state":"offline"}]}"#.to_vec(),
        )]);
        let mut app = friend_test_app();
        app.server = server.url("/ws").replacen("http://", "ws://", 1);
        app.friends = vec![friend("42")];
        app.presence_due = Instant::now();

        app.poll_presence();
        app.presence_job.as_mut().unwrap().join();
        app.poll_presence();

        assert!(app.presence_job.is_none());
        assert!(app.presence_revision.is_none());
        assert!(app.presence_due > Instant::now());
        let _ = server.finish();
    }

    #[test]
    fn refreshing_presence_can_replace_one_waiting_worker_without_growth() {
        let (first_entered_tx, first_entered_rx) = std::sync::mpsc::channel();
        let (first_release_tx, first_release_rx) = std::sync::mpsc::channel();
        let mut app = friend_test_app();
        app.presence_job = Some(presence::start_job(Default::default(), move |_| {
            let _ = first_entered_tx.send(());
            let _ = first_release_rx.recv_timeout(Duration::from_secs(3));
            Ok(presence::Snapshot {
                friends: Vec::new(),
                revision: None,
            })
        }));
        first_entered_rx
            .recv_timeout(Duration::from_secs(3))
            .unwrap();

        app.refresh_presence_now();
        assert!(app.presence_job.is_none());
        assert!(app.retiring_presence_job.is_some());

        let (second_entered_tx, second_entered_rx) = std::sync::mpsc::channel();
        let (second_release_tx, second_release_rx) = std::sync::mpsc::channel();
        app.presence_job = Some(presence::start_job(Default::default(), move |_| {
            let _ = second_entered_tx.send(());
            let _ = second_release_rx.recv_timeout(Duration::from_secs(3));
            Ok(presence::Snapshot {
                friends: Vec::new(),
                revision: None,
            })
        }));
        second_entered_rx
            .recv_timeout(Duration::from_secs(3))
            .unwrap();

        app.refresh_presence_now();
        assert!(app.presence_job.is_some());
        assert!(app.retiring_presence_job.is_some());

        let _ = first_release_tx.send(());
        let _ = second_release_tx.send(());
        if let Some(mut job) = app.presence_job.take() {
            job.join();
        }
        if let Some(mut job) = app.retiring_presence_job.take() {
            job.join();
        }
    }

    #[test]
    fn a_background_identity_changes_ui_state_and_survives_child_teardown() {
        // Previously both digests read the already-updated child mutex, so an
        // identity arriving before tick never caused a repaint. Child exit
        // also destroyed the only copy before Home could offer it.
        let mut offers = Vec::new();
        let status = std::sync::Mutex::new(supervisor::StreamStatus::default());
        status.lock().unwrap().met.push(friend("42"));
        let before = next_friend_offer(&offers, &[], Some("self")).cloned();
        collect_met_friends(&mut offers, &status);
        assert!(status.lock().unwrap().met.is_empty());
        drop(status);
        let after = next_friend_offer(&offers, &[], Some("self")).cloned();
        assert_ne!(before, after);
        assert_eq!(after.unwrap().id, "42");
    }

    #[test]
    fn reopening_the_picker_removes_old_selectable_targets_before_enumeration() {
        // A closed HWND from the previous visit must not remain selectable
        // while the replacement list is waiting on a slow enumeration worker.
        let mut windows = vec![WindowTarget {
            hwnd: 7,
            title: "Closed window".into(),
            process: "test.exe".into(),
            width: 1,
            height: 1,
        }];
        let image = capture::to_image((1, 1, vec![0; 4])).unwrap();
        let mut thumbnails = std::collections::HashMap::from([(7, image)]);
        let mut jobs = PickerJobs::default();

        begin_picker_refresh(&mut windows, &mut thumbnails, &mut jobs);

        assert!(windows.is_empty(), "stale HWNDs are still selectable");
        assert!(thumbnails.is_empty(), "stale previews are still visible");
        assert!(jobs.is_loading());
        assert!(jobs.is_busy());
    }

    fn icon_frame(size: u16) -> image::RgbaImage {
        let source = include_bytes!("../../../assets/icon.ico");
        let count = u16::from_le_bytes([source[4], source[5]]) as usize;
        for index in 0..count {
            let start = 6 + index * 16;
            let width = if source[start] == 0 {
                256
            } else {
                u16::from(source[start])
            };
            let height = if source[start + 1] == 0 {
                256
            } else {
                u16::from(source[start + 1])
            };
            if (width, height) != (size, size) {
                continue;
            }

            let length = u32::from_le_bytes(
                source[start + 8..start + 12]
                    .try_into()
                    .expect("ICO entry length"),
            ) as usize;
            let offset = u32::from_le_bytes(
                source[start + 12..start + 16]
                    .try_into()
                    .expect("ICO entry offset"),
            ) as usize;
            let mut single = Vec::with_capacity(22 + length);
            single.extend_from_slice(&source[..4]);
            single.extend_from_slice(&1u16.to_le_bytes());
            single.extend_from_slice(&source[start..start + 12]);
            single.extend_from_slice(&22u32.to_le_bytes());
            single.extend_from_slice(&source[offset..offset + length]);
            return image::load_from_memory_with_format(&single, image::ImageFormat::Ico)
                .expect("valid icon frame")
                .into_rgba8();
        }
        panic!("icon has no {size}px frame");
    }

    fn most_scanline_runs(image: &image::RgbaImage) -> usize {
        (0..image.width())
            .map(|x| {
                let mut previous = false;
                let mut runs = 0;
                for y in 0..image.height() {
                    let [r, g, b, a] = image.get_pixel(x, y).0;
                    let orange = r >= 200 && (45..=150).contains(&g) && b <= 80 && a >= 128;
                    if orange && !previous {
                        runs += 1;
                    }
                    previous = orange;
                }
                runs
            })
            .max()
            .unwrap_or_default()
    }

    fn login_session() -> session::Session {
        session::Session {
            name: "Orange User".into(),
            id: "123".into(),
            avatar_url: None,
            token: "token".into(),
        }
    }

    #[test]
    fn taskbar_identity_matches_viewer_process() {
        assert_eq!(APP_USER_MODEL_ID, "brnbtt.orange");
        set_taskbar_identity().expect("taskbar identity should be accepted by Windows");
    }

    #[test]
    fn app_icon_contains_optically_tuned_windows_frames() {
        let frames = [
            (16, 3, 7),
            (20, 4, 8),
            (24, 5, 9),
            (32, 5, 10),
            (48, 6, 11),
            (64, 7, 12),
            (128, 10, 17),
            (256, 12, 18),
        ];
        for (size, minimum, maximum) in frames {
            let frame = icon_frame(size);
            assert_eq!(frame.dimensions(), (u32::from(size), u32::from(size)));
            let runs = most_scanline_runs(&frame);
            assert!(
                (minimum..=maximum).contains(&runs),
                "{size}px mark has {runs} scanlines"
            );
        }
    }

    #[test]
    fn login_poll_preserves_session_success_across_child_exit_races() {
        let mut loads = 0;
        let session = poll_login(
            || {
                loads += 1;
                Ok((loads == 2).then(login_session))
            },
            || Some("child exited".into()),
        )
        .expect("login should be terminal")
        .expect("second load should win");
        assert_eq!(session.name, "Orange User");
        assert_eq!(loads, 2);

        let mut failure_checks = 0;
        let session = poll_login(
            || Ok(Some(login_session())),
            || {
                failure_checks += 1;
                Some("child exited".into())
            },
        )
        .expect("login should be terminal")
        .expect("initial load should win");
        assert_eq!(session.name, "Orange User");
        assert_eq!(failure_checks, 0);
    }
}
