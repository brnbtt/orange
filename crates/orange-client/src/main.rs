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
mod presence;
mod session;
mod sound;
mod supervisor;
mod ui;
mod update;
mod view;

use background::{AvatarJobs, FriendAvatarJobs, PickerEvent, PickerJobs};
use gpui::{
    prelude::*, px, size, App, Application, Bounds, Context, Timer, TitlebarOptions, WindowBounds,
    WindowOptions,
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

struct Orange {
    client_available: bool,
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
    /// The roster this machine wants presence for. Seeded by hand in
    /// `preferences.json` until the invite flow exists.
    friends: Vec<session::Friend>,
    /// Last answer from the relay, keyed by Discord id. Absent means "not
    /// asked yet or the poll failed", which the view renders differently from
    /// a friend who is genuinely offline.
    presence: std::collections::HashMap<String, presence::Presence>,
    presence_job: Option<presence::PresenceJob>,
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
}

impl Orange {
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

    /// Start a poll when one is due, and collect the answer from the last one.
    ///
    /// Runs inside the 500 ms tick but only reaches the network every
    /// `presence::INTERVAL`, the same shape as the update controller's
    /// six-hourly check.
    fn poll_presence(&mut self) {
        if let Some(job) = self.presence_job.as_ref() {
            if job.is_finished() {
                let result = job.take_result();
                if let Some(mut job) = self.presence_job.take() {
                    job.join();
                }
                match result {
                    Some(Ok(entries)) => {
                        self.presence_error = None;
                        self.absorb_profiles(&entries);
                        self.presence = entries
                            .into_iter()
                            .map(|entry| (entry.id, entry.presence))
                            .collect();
                    }
                    Some(Err(error)) => self.presence_error = Some(error),
                    // The worker was cancelled before it sent anything. Leave
                    // the previous answer standing rather than blanking the
                    // list on a race.
                    None => {}
                }
            }
            return;
        }

        if Instant::now() < self.presence_due {
            return;
        }
        self.presence_due = Instant::now() + presence::INTERVAL;

        let Some(session) = self.session.as_ref() else {
            self.presence.clear();
            return;
        };
        let Some(url) = presence::presence_url(&self.server) else {
            self.presence_error = Some(presence::PresenceError::Unreachable(format!(
                "cannot derive a presence URL from {}",
                self.server
            )));
            return;
        };
        presence::start(
            &mut self.presence_job,
            self.presence_client.clone(),
            url,
            session.token.clone(),
            &self.friends,
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
        let (preferences, preference_error) = match session::load_preferences() {
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
        Self {
            client_available,
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
            update_collapsed: false,
            settings_open: [true; 3],
            copied_at: None,
            copied_code: None,
            own_codes: preferences.own_codes,
            viewers_seen: 0,
            friends: preferences.friends,
            presence: std::collections::HashMap::new(),
            presence_job: None,
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
        }
    }

    fn tick(&mut self, cx: &mut Context<Self>) {
        let before = self.digest();
        if self.screen != Screen::PickWindow {
            self.thumbnail_job.cancel();
        }
        let picker_changed = self.drain_thumbnails();
        self.poll_updates(cx);
        self.poll_presence();
        self.poll_friend_avatars();
        if let Some(pixels) = self.avatar_job.poll() {
            self.avatar = capture::to_image(pixels);
        }

        // Login happens in a child process; notice when it lands, and when it
        // dies without producing a session.
        if let Some(attempt) = self.logging_in.as_mut() {
            match poll_login(session::load, || attempt.failure()) {
                Some(Ok(session)) => {
                    self.avatar = None;
                    self.avatar_job.request(session.avatar_url.clone());
                    self.session = Some(session);
                    if let Some(job) = self.presence_job.as_mut() {
                        job.cancel();
                    }
                    self.presence_due = Instant::now();
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
            if watch.supervisor.running() {
                true
            } else {
                if let Ok(status) = watch.supervisor.status.lock() {
                    watch_error = status.error.clone().or(watch_error.take());
                    watch_ended |= status.ended;
                }
                false
            }
        });
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

    /// Someone this session put us in contact with who is not a friend yet.
    ///
    /// Read from every live child, so it covers both directions: the host of a
    /// stream being watched, and the newest viewer of a stream being hosted.
    fn pending_friend(&self) -> Option<session::Friend> {
        let from_host = self
            .host
            .as_ref()
            .and_then(|host| host.status.lock().ok())
            .and_then(|status| status.met.clone());
        let from_watch = self.watches.iter().find_map(|watch| {
            watch
                .supervisor
                .status
                .lock()
                .ok()
                .and_then(|status| status.met.clone())
        });
        from_watch
            .or(from_host)
            .filter(|friend| !self.is_friend(&friend.id))
    }

    /// Decline the offer.
    ///
    /// Clears the child's record rather than remembering a refusal: the offer
    /// only exists while that session does, so there is nothing to remember
    /// once it is gone.
    fn dismiss_pending_friend(&mut self) {
        if let Some(host) = self.host.as_ref() {
            if let Ok(mut status) = host.status.lock() {
                status.met = None;
            }
        }
        for watch in &self.watches {
            if let Ok(mut status) = watch.supervisor.status.lock() {
                status.met = None;
            }
        }
    }

    /// Keep someone met through a code join.
    ///
    /// Deliberately not automatic. A code gets pasted into group chats, so
    /// auto-adding would hand a permanent view of when you stream to everyone
    /// who ever clicked it out of curiosity. The user decides.
    fn add_friend(&mut self, friend: session::Friend) {
        if let Some(existing) = self.friends.iter_mut().find(|f| f.id == friend.id) {
            *existing = friend;
        } else {
            let name = friend.name.clone();
            self.friends.push(friend);
            self.show_notice(NoticeKind::Ordinary, format!("Added {name}"));
        }
        self.save_preferences();
    }

    /// Forget someone, and stop showing them as live.
    ///
    /// The stale presence entry has to go with them: it is keyed by id, and
    /// re-adding the same person would otherwise show whatever state was last
    /// seen before the removal.
    fn remove_friend(&mut self, id: &str) {
        let Some(index) = self.friends.iter().position(|friend| friend.id == id) else {
            return;
        };
        let removed = self.friends.remove(index);
        self.presence.remove(id);
        self.friend_avatars.remove(id);
        self.show_notice(NoticeKind::Ordinary, format!("Removed {}", removed.name));
        self.save_preferences();
    }

    fn is_friend(&self, id: &str) -> bool {
        self.friends.iter().any(|friend| friend.id == id)
    }

    fn save_preferences(&mut self) {
        let preferences = session::Preferences {
            quality: self.quality,
            fps: Some(self.fps),
            own_codes: self.own_codes.clone(),
            friends: self.friends.clone(),
        };
        if let Err(error) = session::save_preferences(&preferences) {
            self.show_error(format!("Could not save preferences: {error}"));
        }
    }

    fn sign_out(&mut self, destination: Option<Screen>) {
        if let Err(error) = session::clear() {
            self.show_error(format!("Could not sign out: {error}"));
            return;
        }
        self.session = None;
        self.avatar = None;
        self.avatar_job.request(None);
        self.thumbnail_job.cancel();
        if let Some(job) = self.presence_job.as_mut() {
            job.cancel();
        }
        self.presence.clear();
        self.presence_error = None;
        self.presence_due = Instant::now();
        if let Some(destination) = destination {
            self.screen = destination;
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
            self.watches.remove(index);
        }
        if self.watches.is_empty() && self.host.is_none() {
            self.thumbnail_job.cancel();
            self.screen = Screen::Home;
        }
    }

    fn stop_all_watches(&mut self) {
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
