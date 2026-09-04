//! orange tray - the host-side UI.
//!
//! GPUI fits here precisely because there is no video: this is ordinary UI.
//! The viewer window stays native because GPUI's `Surface` element has no
//! Windows implementation - its only variant is macOS-gated.

// Without this the binary is a console application and Windows opens a black
// cmd window behind the UI.
#![windows_subsystem = "windows"]

mod background;
mod capture;
mod session;
mod sound;
mod supervisor;
mod tray;
mod ui;
mod update;
mod view;

use background::{
    fetch_avatar, replace_avatar_job, replace_thumbnail_job, stop_avatar_job, stop_thumbnail_job,
    AvatarJob, ThumbnailJob,
};
use gpui::{
    prelude::*, px, size, App, Application, Bounds, Context, Timer, TitlebarOptions, WindowBounds,
    WindowOptions,
};
use std::cell::RefCell;
use std::rc::Rc;
use std::sync::mpsc;
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

struct Orange {
    tray_available: bool,
    screen: Screen,
    session: Option<session::Session>,
    windows: Vec<WindowTarget>,
    /// Thumbnails keyed by window handle, filled in asynchronously.
    thumbnails: std::collections::HashMap<i64, std::sync::Arc<gpui::RenderImage>>,
    /// Results arriving from the capture thread.
    thumbnail_job: Option<ThumbnailJob>,
    /// Discord avatar decoded off the UI thread.
    avatar: Option<std::sync::Arc<gpui::RenderImage>>,
    avatar_job: Option<AvatarJob>,
    quality: usize,
    fps: u32,
    active_target: Option<WindowTarget>,
    active_preview: Option<std::sync::Arc<gpui::RenderImage>>,
    host: Option<Supervisor>,
    watches: Vec<WatchSession>,
    logging_in: Option<LoginAttempt>,
    notice: Option<Notice>,
    server: String,
    /// Last screen the window was sized for, so resize happens once per
    /// transition rather than every frame.
    sized_for: Option<Screen>,
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
/// the tray and there was nobody to show it to.
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
    has_preview: bool,
    /// The share code's "Copied" confirmation expires on a timer rather than
    /// on an event, so the tick that retires it has to be the one that
    /// repaints.
    recently_copied: bool,
    update_status: String,
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
            capturing: self.thumbnail_job.is_some(),
            has_preview: self.active_preview.is_some(),
            recently_copied: self.copied_at.is_some_and(|at| at.elapsed() < COPIED_FOR),
            update_status: self.updates.settings_detail(),
        }
    }
}

impl Orange {
    fn new(cx: &mut Context<Self>, tray_available: bool) -> Self {
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
        let mut avatar_job = None;
        replace_avatar_job(
            &mut avatar_job,
            session.as_ref().and_then(|s| s.avatar_url.clone()),
            fetch_avatar,
        );
        let updates = update::UpdateController::new();
        Self {
            tray_available,
            screen: if session.is_some() {
                Screen::Home
            } else {
                Screen::SignedOut
            },
            session,
            windows: Vec::new(),
            thumbnails: std::collections::HashMap::new(),
            thumbnail_job: None,
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
            sized_for: None,
            update_collapsed: false,
            settings_open: [true; 3],
            copied_at: None,
            copied_code: None,
            own_codes: preferences.own_codes,
            viewers_seen: 0,
            logo_epoch: 0,
            // Corrected on the first render, before anything is painted.
            animate: false,
            updates,
        }
    }

    fn tick(&mut self, cx: &mut Context<Self>) {
        let before = self.digest();
        self.drain_thumbnails();
        self.poll_updates(cx);
        let avatar_result = self.avatar_job.as_ref().and_then(|job| {
            job.is_finished().then(|| match job.receiver.try_recv() {
                Ok(pixels) => Some(Some(pixels)),
                Err(mpsc::TryRecvError::Disconnected) => Some(None),
                Err(mpsc::TryRecvError::Empty) => None,
            })?
        });
        if let Some(result) = avatar_result {
            if let Some(mut job) = self.avatar_job.take() {
                job.join();
            }
            if let Some(Some(pixels)) = result {
                self.avatar = capture::to_image(pixels);
            }
        }

        // Login happens in a child process; notice when it lands, and when it
        // dies without producing a session.
        if let Some(attempt) = self.logging_in.as_mut() {
            match poll_login(session::load, || attempt.failure()) {
                Some(Ok(session)) => {
                    self.avatar = None;
                    replace_avatar_job(
                        &mut self.avatar_job,
                        session.avatar_url.clone(),
                        fetch_avatar,
                    );
                    self.session = Some(session);
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
        if self.digest() != before {
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

    fn save_preferences(&mut self) {
        let preferences = session::Preferences {
            quality: self.quality,
            fps: Some(self.fps),
            own_codes: self.own_codes.clone(),
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
        stop_avatar_job(&mut self.avatar_job);
        if let Some(destination) = destination {
            self.screen = destination;
        }
    }

    fn refresh_windows(&mut self) {
        stop_thumbnail_job(&mut self.thumbnail_job);
        match supervisor::list_windows() {
            Ok(mut windows) => {
                // Never offer our own windows as a capture target.
                windows.retain(|w| !w.process.to_lowercase().starts_with("orange"));

                // A zero handle is the sentinel for whole-screen capture, which
                // the pipeline turns into a monitor source rather than a window
                // one. It goes first because it is the common choice.
                let (screen_width, screen_height) = capture::screen_size().unwrap_or((0, 0));
                windows.insert(
                    0,
                    WindowTarget {
                        hwnd: 0,
                        title: "Entire screen".into(),
                        process: "Desktop".into(),
                        width: screen_width,
                        height: screen_height,
                    },
                );

                // Capture off the UI thread. PrintWindow is synchronous and
                // costs tens of milliseconds per window, so doing this inline
                // froze the app for as long as it took to walk the list.
                let handles: Vec<i64> = windows.iter().map(|w| w.hwnd).collect();
                replace_thumbnail_job(&mut self.thumbnail_job, handles, |hwnd| {
                    if hwnd == 0 {
                        capture::screen_thumbnail(320, 180)
                    } else {
                        capture::thumbnail(hwnd as isize, 320, 180)
                    }
                });

                self.thumbnails.clear();
                self.windows = windows;
                self.clear_error();
            }
            Err(err) => self.show_error(err.to_string()),
        }
    }

    /// Move any captured thumbnails into the map. Runs on the UI thread, which
    /// is where GPUI's image types have to be built.
    fn drain_thumbnails(&mut self) -> bool {
        let Some(job) = &self.thumbnail_job else {
            return false;
        };
        let mut changed = false;
        let mut terminal = false;
        loop {
            match job.receiver.try_recv() {
                Ok((hwnd, thumb)) => {
                    if let Some(image) = capture::to_image(thumb) {
                        self.thumbnails.insert(hwnd, image);
                        changed = true;
                    }
                }
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => {
                    terminal = job.is_finished();
                    break;
                }
            }
        }
        if terminal {
            if let Some(mut job) = self.thumbnail_job.take() {
                job.join();
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
        stop_thumbnail_job(&mut self.thumbnail_job);
        if !supervisor::gstreamer_available() {
            self.show_error(supervisor::MEDIA_RUNTIME_MISSING.to_string());
            return;
        }
        let preview = self.thumbnails.get(&target.hwnd).cloned();
        match Supervisor::host(&target, &self.quality(), self.fps, &self.server) {
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
                if self.host.is_none() {
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
            self.screen = Screen::Home;
        }
    }

    fn stop_all_watches(&mut self) {
        self.watches.clear();
        if self.host.is_none() {
            self.screen = Screen::Home;
        }
    }

    fn leave_picker(&mut self, destination: Screen) {
        stop_thumbnail_job(&mut self.thumbnail_job);
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
        stop_thumbnail_job(&mut self.thumbnail_job);
        stop_avatar_job(&mut self.avatar_job);
    }
}

type TrayOwner = Rc<RefCell<Option<tray::Tray>>>;

fn shutdown_owned_tray(owner: &TrayOwner) -> anyhow::Result<()> {
    shutdown_owned_tray_with(owner, tray::Tray::shutdown)
}

fn finish_owned_tray(owner: &TrayOwner) -> anyhow::Result<()> {
    shutdown_owned_tray_with(owner, tray::Tray::shutdown_final)
}

fn shutdown_owned_tray_with(
    owner: &TrayOwner,
    shutdown: impl FnOnce(&mut tray::Tray) -> anyhow::Result<()>,
) -> anyhow::Result<()> {
    let Some(mut tray) = owner.borrow_mut().take() else {
        return Ok(());
    };
    if let Err(error) = shutdown(&mut tray) {
        owner.borrow_mut().replace(tray);
        return Err(error);
    }
    Ok(())
}

fn main() {
    // Must precede every window so the tray and viewer processes share one
    // taskbar group despite being different executables.
    if let Err(error) = set_taskbar_identity() {
        eprintln!("[tray] could not set taskbar identity: {error}");
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
    // start a rival process. Checked before the tray so the loser exits without
    // ever adding a second icon.
    if tray::defer_to_running_instance() {
        return;
    }

    // Installed before the UI so a failure here is visible as a missing icon
    // rather than a half-started app.
    let tray_owner = Rc::new(RefCell::new(tray::Tray::install().ok()));
    let tray_available = tray_owner.borrow().is_some();
    let app_tray_owner = Rc::clone(&tray_owner);

    Application::new().run(move |cx: &mut App| {
        let quit_owner = Rc::clone(&app_tray_owner);
        cx.on_app_quit(move |_| {
            let quit_owner = Rc::clone(&quit_owner);
            async move {
                if let Err(error) = shutdown_owned_tray(&quit_owner) {
                    eprintln!("[tray] app-quit shutdown failed: {error:#}");
                }
            }
        })
        // GPUI subscriptions cancel on Drop; detaching retains this observer
        // until the App emitter itself is dropped.
        .detach();

        let bounds = Bounds::centered(None, size(px(400.0), px(540.0)), cx);
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
                    window_min_size: Some(size(px(360.0), px(480.0))),
                    is_resizable: false,
                    ..Default::default()
                },
                |_, cx| cx.new(|cx| Orange::new(cx, tray_available)),
            )
            .unwrap();
        cx.activate(true);

        // Closing the window hides it instead of quitting: a tray app should
        // keep streaming when its window is dismissed. Quit lives in the tray
        // menu.
        let _ = window.update(cx, |_, window, cx| {
            window.on_window_should_close(cx, move |window, _cx| {
                if tray_available {
                    window.minimize_window();
                    false
                } else {
                    true
                }
            });
        });

        // The tray runs its own Win32 message loop on another thread, so its
        // events arrive over a channel and are drained on a timer here.
        if tray_available {
            let event_owner = Rc::clone(&app_tray_owner);
            cx.spawn(async move |cx| loop {
                Timer::after(Duration::from_millis(200)).await;
                loop {
                    let event = match event_owner.borrow().as_ref().map(tray::Tray::try_recv) {
                        None => return,
                        Some(Ok(event)) => event,
                        Some(Err(std::sync::mpsc::TryRecvError::Empty)) => break,
                        Some(Err(std::sync::mpsc::TryRecvError::Disconnected)) => return,
                    };
                    match event {
                        tray::TrayEvent::Show => {
                            // The window may be hidden rather than merely
                            // unfocused, so un-hide before activating.
                            tray::show_main_window();
                            let _ = cx.update(|cx| {
                                let _ = window.update(cx, |view, window, cx| {
                                    view.logo_epoch = view.logo_epoch.wrapping_add(1);
                                    cx.notify();
                                    window.activate_window();
                                });
                            });
                        }
                        tray::TrayEvent::Quit => {
                            if let Err(error) = shutdown_owned_tray(&event_owner) {
                                eprintln!("[tray] tray-quit shutdown failed: {error:#}");
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

    if let Err(error) = finish_owned_tray(&tray_owner) {
        tray::fail_fast(
            "final app shutdown could not clean up tray ownership",
            &error,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn icon_frame(size: u16) -> image::RgbaImage {
        let source = include_bytes!("../icon.ico");
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
