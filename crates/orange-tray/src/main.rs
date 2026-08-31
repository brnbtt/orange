//! orange tray - the host-side UI.
//!
//! GPUI fits here precisely because there is no video: this is ordinary UI.
//! The viewer window stays native because GPUI's `Surface` element has no
//! Windows implementation - its only variant is macOS-gated.

// Without this the binary is a console application and Windows opens a black
// cmd window behind the UI.
#![windows_subsystem = "windows"]

mod capture;
mod session;
mod supervisor;
mod tray;
mod ui;
mod update;
mod view;

use gpui::{
    prelude::*, px, size, App, Application, Bounds, Context, Timer, TitlebarOptions, WindowBounds,
    WindowOptions,
};
use std::cell::RefCell;
use std::io::Read;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};
use supervisor::{LoginAttempt, Quality, Supervisor, WindowTarget, QUALITIES};

const DEFAULT_SERVER: &str =
    "wss://orange-relay.redmushroom-80c79f12.brazilsouth.azurecontainerapps.io/ws";
const AVATAR_MAX_BYTES: usize = 4 * 1024 * 1024;
const THUMBNAIL_JOIN_TIMEOUT: Duration = Duration::from_secs(5);
const AVATAR_JOIN_TIMEOUT: Duration = Duration::from_secs(35);

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
    expires_at: Instant,
}

struct ThumbnailJob {
    cancel: Arc<AtomicBool>,
    receiver: mpsc::Receiver<(i64, capture::Thumbnail)>,
    worker: Option<JoinHandle<()>>,
}

impl ThumbnailJob {
    fn is_finished(&self) -> bool {
        self.worker.as_ref().is_none_or(JoinHandle::is_finished)
    }

    fn join(&mut self) {
        if let Some(worker) = self.worker.take() {
            join_background_worker(worker, THUMBNAIL_JOIN_TIMEOUT, "thumbnail capture");
        }
    }
}

impl Drop for ThumbnailJob {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Release);
        self.join();
    }
}

struct AvatarJob {
    cancel: Arc<AtomicBool>,
    receiver: mpsc::Receiver<Option<capture::Thumbnail>>,
    worker: Option<JoinHandle<()>>,
}

impl AvatarJob {
    fn is_finished(&self) -> bool {
        self.worker.as_ref().is_none_or(JoinHandle::is_finished)
    }

    fn join(&mut self) {
        if let Some(worker) = self.worker.take() {
            join_background_worker(worker, AVATAR_JOIN_TIMEOUT, "avatar fetch");
        }
    }
}

impl Drop for AvatarJob {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Release);
        self.join();
    }
}

fn join_background_worker(worker: JoinHandle<()>, timeout: Duration, name: &str) {
    let deadline = Instant::now() + timeout;
    while !worker.is_finished() {
        if Instant::now() >= deadline {
            tray::fail_fast(
                &format!("{name} worker did not terminate before its deadline"),
                &anyhow::anyhow!("timeout after {timeout:?}"),
            );
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    if worker.join().is_err() {
        eprintln!("[tray] {name} worker panicked");
    }
}

fn replace_thumbnail_job(
    job: &mut Option<ThumbnailJob>,
    handles: Vec<i64>,
    capture: impl Fn(i64) -> Option<capture::Thumbnail> + Send + 'static,
) {
    stop_thumbnail_job(job);
    let cancel = Arc::new(AtomicBool::new(false));
    let worker_cancel = Arc::clone(&cancel);
    let (sender, receiver) = mpsc::channel();
    let worker = std::thread::spawn(move || {
        for hwnd in handles {
            if worker_cancel.load(Ordering::Acquire) {
                return;
            }
            let thumbnail = capture(hwnd);
            if worker_cancel.load(Ordering::Acquire) {
                return;
            }
            if let Some(thumbnail) = thumbnail {
                if worker_cancel.load(Ordering::Acquire) {
                    return;
                }
                if sender.send((hwnd, thumbnail)).is_err() {
                    return;
                }
            }
        }
    });
    *job = Some(ThumbnailJob {
        cancel,
        receiver,
        worker: Some(worker),
    });
}

fn stop_thumbnail_job(job: &mut Option<ThumbnailJob>) {
    drop(job.take());
}

fn replace_avatar_job(
    job: &mut Option<AvatarJob>,
    url: Option<String>,
    fetch: impl FnOnce(&str) -> Option<Vec<u8>> + Send + 'static,
) {
    stop_avatar_job(job);
    let Some(url) = url else {
        return;
    };
    let cancel = Arc::new(AtomicBool::new(false));
    let worker_cancel = Arc::clone(&cancel);
    let (sender, receiver) = mpsc::channel();
    let worker = std::thread::spawn(move || {
        if worker_cancel.load(Ordering::Acquire) {
            return;
        }
        let bytes = fetch(&url);
        if worker_cancel.load(Ordering::Acquire) {
            return;
        }
        let pixels = bytes.and_then(decode_avatar);
        if worker_cancel.load(Ordering::Acquire) {
            return;
        }
        if !worker_cancel.load(Ordering::Acquire) {
            let _ = sender.send(pixels);
        }
    });
    *job = Some(AvatarJob {
        cancel,
        receiver,
        worker: Some(worker),
    });
}

fn stop_avatar_job(job: &mut Option<AvatarJob>) {
    drop(job.take());
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
    fps: Option<u32>,
    active_target: Option<WindowTarget>,
    active_preview: Option<std::sync::Arc<gpui::RenderImage>>,
    host: Option<Supervisor>,
    watches: Vec<WatchSession>,
    logging_in: Option<LoginAttempt>,
    notice: Option<Notice>,
    server: String,
    /// Last screen the window was sized for, so resize happens once per
    /// transition rather than every frame.
    sized_for: Option<(Screen, bool)>,
    /// Drives the transient "Copied" confirmation on the share code.
    copied_at: Option<Instant>,
    copied_code: Option<String>,
    own_codes: Vec<String>,
    logo_epoch: u64,
    updates: update::UpdateController,
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
            fps: preferences.fps,
            active_target: None,
            active_preview: None,
            host: None,
            watches: Vec::new(),
            logging_in: None,
            notice: startup_error.map(|text| Notice {
                text,
                expires_at: Instant::now() + Duration::from_secs(4),
            }),
            server: std::env::var("ORANGE_SERVER").unwrap_or_else(|_| DEFAULT_SERVER.to_string()),
            sized_for: None,
            copied_at: None,
            copied_code: None,
            own_codes: preferences.own_codes,
            logo_epoch: 0,
            updates,
        }
    }

    fn tick(&mut self, cx: &mut Context<Self>) {
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
        self.watches.retain_mut(|watch| {
            if watch.supervisor.running() {
                true
            } else {
                watch_error = watch
                    .supervisor
                    .status
                    .lock()
                    .ok()
                    .and_then(|status| status.error.clone())
                    .or(watch_error.take());
                false
            }
        });
        if let Some(error) = watch_error {
            self.show_error(error);
        }
        if self.screen == Screen::Watching && self.watches.is_empty() {
            self.screen = Screen::Home;
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
            }
        }
        if self
            .notice
            .as_ref()
            .is_some_and(|notice| Instant::now() >= notice.expires_at)
        {
            self.notice = None;
        }
        cx.notify();
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

    fn show_error(&mut self, message: impl Into<String>) {
        self.notice = Some(Notice {
            text: message.into(),
            expires_at: Instant::now() + Duration::from_secs(4),
        });
    }

    fn clear_error(&mut self) {
        self.notice = None;
    }

    fn save_preferences(&mut self) {
        let preferences = session::Preferences {
            quality: self.quality,
            fps: self.fps,
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
            self.show_error(
                "GStreamer was not found. Install it with: winget install gstreamerproject.gstreamer"
                    .to_string(),
            );
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
            self.show_error(
                "GStreamer was not found. Install it with: winget install gstreamerproject.gstreamer"
                    .to_string(),
            );
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

// --- shared pieces ----------------------------------------------------------

fn fetch_avatar(url: &str) -> Option<Vec<u8>> {
    let mut response = reqwest::blocking::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(30))
        .user_agent(concat!("orange/", env!("CARGO_PKG_VERSION")))
        .build()
        .ok()?
        .get(url)
        .send()
        .ok()?
        .error_for_status()
        .ok()?;
    if response
        .content_length()
        .is_some_and(|length| length > AVATAR_MAX_BYTES as u64)
    {
        return None;
    }
    read_avatar_response(&mut response)
}

fn read_avatar_response(reader: &mut impl std::io::Read) -> Option<Vec<u8>> {
    let mut bytes = Vec::new();
    reader
        .take(AVATAR_MAX_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .ok()?;
    (bytes.len() <= AVATAR_MAX_BYTES).then_some(bytes)
}

fn decode_avatar(bytes: Vec<u8>) -> Option<capture::Thumbnail> {
    let mut reader = image::ImageReader::new(std::io::Cursor::new(bytes));
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(4096);
    limits.max_image_height = Some(4096);
    limits.max_alloc = Some(64 * 1024 * 1024);
    reader.limits(limits);
    let image = reader
        .with_guessed_format()
        .ok()?
        .decode()
        .ok()?
        .into_rgba8();
    let mut raw =
        image::imageops::resize(&image, 64, 64, image::imageops::FilterType::Lanczos3).into_raw();
    // GPUI's image renderer expects BGRA.
    for pixel in raw.as_chunks_mut::<4>().0 {
        pixel.swap(0, 2);
    }
    Some((64, 64, raw))
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
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{mpsc, Arc};

    fn login_session() -> session::Session {
        session::Session {
            name: "Orange User".into(),
            id: "123".into(),
            avatar_url: None,
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

    #[test]
    fn thumbnail_background_job_replacement_cancels_and_joins_the_old_worker() {
        let (entered_tx, entered_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let old_capture_returned = Arc::new(AtomicBool::new(false));
        let returned = Arc::clone(&old_capture_returned);
        let mut job = None;

        replace_thumbnail_job(&mut job, vec![1, 2], move |_| {
            entered_tx.send(()).unwrap();
            release_rx.recv().unwrap();
            returned.store(true, Ordering::SeqCst);
            Some((1, 1, vec![0; 4]))
        });
        entered_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("old capture did not start");
        let releaser = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(25));
            release_tx.send(()).unwrap();
        });

        replace_thumbnail_job(&mut job, vec![3], |_| None);

        releaser.join().unwrap();
        assert!(old_capture_returned.load(Ordering::SeqCst));
        stop_thumbnail_job(&mut job);
    }

    #[test]
    fn avatar_background_job_replacement_cancels_and_joins_the_old_worker() {
        let (entered_tx, entered_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let old_fetch_returned = Arc::new(AtomicBool::new(false));
        let returned = Arc::clone(&old_fetch_returned);
        let mut job = None;

        replace_avatar_job(
            &mut job,
            Some("https://example.com/old.png".into()),
            move |_| {
                entered_tx.send(()).unwrap();
                release_rx.recv().unwrap();
                returned.store(true, Ordering::SeqCst);
                Some(Vec::new())
            },
        );
        entered_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("old fetch did not start");
        let releaser = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(25));
            release_tx.send(()).unwrap();
        });

        replace_avatar_job(&mut job, Some("https://example.com/new.png".into()), |_| {
            None
        });

        releaser.join().unwrap();
        assert!(old_fetch_returned.load(Ordering::SeqCst));
        stop_avatar_job(&mut job);
    }

    #[test]
    fn avatar_response_is_rejected_before_exceeding_its_memory_cap() {
        let mut oversized = std::io::Cursor::new(vec![0; AVATAR_MAX_BYTES + 1]);

        assert!(read_avatar_response(&mut oversized).is_none());
    }
}
