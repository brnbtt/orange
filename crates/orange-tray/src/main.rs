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

use gpui::{
    div, prelude::*, px, rgb, size, Animation, AnimationExt, App, Application, Bounds, Context,
    FontWeight, SharedString, Timer, TitlebarOptions, Window, WindowBounds, WindowOptions,
};
use std::time::{Duration, Instant};
use supervisor::{LoginAttempt, Quality, Supervisor, WindowTarget, QUALITIES};

// Palette from the logo exploration.
const BG: u32 = 0x0b0b0b;
const SURFACE: u32 = 0x161616;
const SURFACE_HOVER: u32 = 0x202020;
const BORDER: u32 = 0x2a2a2a;
const TEXT: u32 = 0xe6e0d1;
const MUTED: u32 = 0x99948a;
const FAINT: u32 = 0x66625b;
const ORANGE: u32 = 0xff5a1f;
const ORANGE_DIM: u32 = 0x8a3110;
const INK: u32 = 0x0b0b0b;
const DANGER: u32 = 0xe0645f;
const GREEN: u32 = 0x4ec97a;

const DEFAULT_SERVER: &str =
    "wss://orange-relay.redmushroom-80c79f12.brazilsouth.azurecontainerapps.io/ws";

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
    monitor: bool,
    supervisor: Supervisor,
}

struct Orange {
    screen: Screen,
    session: Option<session::Session>,
    windows: Vec<WindowTarget>,
    /// Thumbnails keyed by window handle, filled in asynchronously.
    thumbnails: std::collections::HashMap<i64, std::sync::Arc<gpui::RenderImage>>,
    /// Results arriving from the capture thread.
    thumb_rx: Option<std::sync::mpsc::Receiver<(i64, capture::Thumbnail)>>,
    /// Discord avatar decoded off the UI thread.
    avatar: Option<std::sync::Arc<gpui::RenderImage>>,
    avatar_rx: Option<std::sync::mpsc::Receiver<capture::Thumbnail>>,
    quality: usize,
    fps: Option<u32>,
    active_target: Option<WindowTarget>,
    active_preview: Option<std::sync::Arc<gpui::RenderImage>>,
    host: Option<Supervisor>,
    watches: Vec<WatchSession>,
    logging_in: Option<LoginAttempt>,
    error: Option<String>,
    server: String,
    /// Last screen the window was sized for, so resize happens once per
    /// transition rather than every frame.
    sized_for: Option<Screen>,
    /// Drives the transient "Copied" confirmation on the share code.
    copied_at: Option<Instant>,
    copied_code: Option<String>,
}

impl Orange {
    fn new(cx: &mut Context<Self>) -> Self {
        // The UI reflects state owned by child processes, so poll rather than
        // trying to push updates across process boundaries.
        cx.spawn(async move |this, cx| loop {
            Timer::after(Duration::from_millis(500)).await;
            if this.update(cx, |this, cx| this.tick(cx)).is_err() {
                break;
            }
        })
        .detach();

        let session = session::load();
        let preferences = session::load_preferences();
        let avatar_rx = request_avatar(session.as_ref().and_then(|s| s.avatar_url.clone()));
        Self {
            screen: if session.is_some() {
                Screen::Home
            } else {
                Screen::SignedOut
            },
            session,
            windows: Vec::new(),
            thumbnails: std::collections::HashMap::new(),
            thumb_rx: None,
            avatar: None,
            avatar_rx,
            quality: preferences.quality.min(QUALITIES.len() - 1),
            fps: preferences.fps,
            active_target: None,
            active_preview: None,
            host: None,
            watches: Vec::new(),
            logging_in: None,
            error: None,
            server: std::env::var("ORANGE_SERVER").unwrap_or_else(|_| DEFAULT_SERVER.to_string()),
            sized_for: None,
            copied_at: None,
            copied_code: None,
        }
    }

    fn tick(&mut self, cx: &mut Context<Self>) {
        self.drain_thumbnails();
        if let Some(rx) = &self.avatar_rx {
            match rx.try_recv() {
                Ok(pixels) => {
                    self.avatar = capture::to_image(pixels);
                    self.avatar_rx = None;
                }
                Err(std::sync::mpsc::TryRecvError::Disconnected) => self.avatar_rx = None,
                Err(std::sync::mpsc::TryRecvError::Empty) => {}
            }
        }

        // Login happens in a child process; notice when it lands, and when it
        // dies without producing a session.
        if self.logging_in.is_some() {
            if let Some(session) = session::load() {
                self.avatar = None;
                self.avatar_rx = request_avatar(session.avatar_url.clone());
                self.session = Some(session);
                self.logging_in = None;
                self.screen = Screen::Home;
            } else if let Some(reason) = self.logging_in.as_mut().and_then(|a| a.failure()) {
                self.error = Some(reason);
                self.logging_in = None;
            }
        }

        if self.host.as_mut().is_some_and(|host| !host.running()) {
            if let Some(status) = self.host.as_ref().and_then(|host| host.status.lock().ok()) {
                self.error = status.error.clone();
            }
            self.host = None;
            self.watches.retain(|watch| !watch.monitor);
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
        if watch_error.is_some() {
            self.error = watch_error;
        }
        if self.screen == Screen::Watching && self.watches.is_empty() {
            self.screen = Screen::Home;
        }

        // A newly-issued room code is immediately ready to paste into chat.
        if let Some(code) = self.code() {
            if self.copied_code.as_deref() != Some(&code) {
                cx.write_to_clipboard(gpui::ClipboardItem::new_string(code.clone()));
                self.copied_code = Some(code);
                self.copied_at = Some(Instant::now());
            }
        }
        cx.notify();
    }

    fn quality(&self) -> Quality {
        QUALITIES[self.quality.min(QUALITIES.len() - 1)]
    }

    fn save_preferences(&self) {
        session::save_preferences(session::Preferences {
            quality: self.quality,
            fps: self.fps,
        });
    }

    fn refresh_windows(&mut self) {
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
                        pid: 0,
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
                let (tx, rx) = std::sync::mpsc::channel();
                std::thread::spawn(move || {
                    for hwnd in handles {
                        let thumb = if hwnd == 0 {
                            capture::screen_thumbnail(320, 180)
                        } else {
                            capture::thumbnail(hwnd as isize, 320, 180)
                        };
                        if let Some(thumb) = thumb {
                            // A closed picker drops the receiver; stop early.
                            if tx.send((hwnd, thumb)).is_err() {
                                return;
                            }
                        }
                    }
                });

                self.thumbnails.clear();
                self.thumb_rx = Some(rx);
                self.windows = windows;
                self.error = None;
            }
            Err(err) => self.error = Some(err.to_string()),
        }
    }

    /// Move any captured thumbnails into the map. Runs on the UI thread, which
    /// is where GPUI's image types have to be built.
    fn drain_thumbnails(&mut self) -> bool {
        let Some(rx) = &self.thumb_rx else {
            return false;
        };
        let mut changed = false;
        loop {
            match rx.try_recv() {
                Ok((hwnd, thumb)) => {
                    if let Some(image) = capture::to_image(thumb) {
                        self.thumbnails.insert(hwnd, image);
                        changed = true;
                    }
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => break,
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    self.thumb_rx = None;
                    break;
                }
            }
        }
        changed
    }

    fn start_login(&mut self) {
        self.error = None;
        match supervisor::start_login(&self.server) {
            Ok(attempt) => self.logging_in = Some(attempt),
            Err(err) => self.error = Some(err.to_string()),
        }
    }

    fn start_stream(&mut self, target: WindowTarget) {
        if !supervisor::gstreamer_available() {
            self.error = Some(
                "GStreamer was not found. Install it with: winget install gstreamerproject.gstreamer"
                    .into(),
            );
            return;
        }
        // A self-monitor belongs to exactly one host room. Never carry one
        // into a replacement stream while its old room is winding down.
        self.watches.retain(|watch| !watch.monitor);
        let preview = self.thumbnails.get(&target.hwnd).cloned();
        match Supervisor::host(&target, &self.quality(), self.fps, &self.server) {
            Ok(stream) => {
                self.active_target = Some(target);
                self.active_preview = preview;
                self.host = Some(stream);
                self.copied_code = None;
                self.copied_at = None;
                self.screen = Screen::Streaming;
                self.error = None;
            }
            Err(err) => self.error = Some(err.to_string()),
        }
    }

    fn join(&mut self, code: String) {
        self.join_with_mode(code, false);
    }

    fn open_live_monitor(&mut self, code: String) {
        self.join_with_mode(code, true);
    }

    fn join_with_mode(&mut self, code: String, monitor: bool) {
        let code = code.trim().to_ascii_uppercase();
        if code.is_empty() {
            self.error = Some("No code on the clipboard".into());
            return;
        }
        if self.watches.iter().any(|watch| watch.code == code) {
            self.error = Some(format!("Already watching {code}"));
            return;
        }
        if !supervisor::gstreamer_available() {
            self.error = Some(
                "GStreamer was not found. Install it with: winget install gstreamerproject.gstreamer"
                    .into(),
            );
            return;
        }
        match Supervisor::watch(&code, &self.server, self.watches.len(), monitor) {
            Ok(stream) => {
                self.watches.push(WatchSession {
                    code,
                    monitor,
                    supervisor: stream,
                });
                if self.host.is_none() {
                    self.screen = Screen::Watching;
                }
                self.error = None;
            }
            Err(err) => self.error = Some(err.to_string()),
        }
    }

    fn stop_host(&mut self) {
        if let Some(mut host) = self.host.take() {
            host.stop();
        }
        self.watches.retain(|watch| !watch.monitor);
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

// --- shared pieces ----------------------------------------------------------

fn request_avatar(url: Option<String>) -> Option<std::sync::mpsc::Receiver<capture::Thumbnail>> {
    let url = url?;
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let pixels = (|| {
            let bytes = reqwest::blocking::Client::builder()
                .user_agent("orange/0.1")
                .build()
                .ok()?
                .get(url)
                .send()
                .ok()?
                .error_for_status()
                .ok()?
                .bytes()
                .ok()?;
            let image = image::load_from_memory(&bytes).ok()?.into_rgba8();
            let mut raw =
                image::imageops::resize(&image, 64, 64, image::imageops::FilterType::Lanczos3)
                    .into_raw();
            // GPUI's image renderer expects BGRA.
            for pixel in raw.chunks_exact_mut(4) {
                pixel.swap(0, 2);
            }
            Some((64, 64, raw))
        })();
        if let Some(pixels) = pixels {
            let _ = tx.send(pixels);
        }
    });
    Some(rx)
}

fn label(text: impl Into<SharedString>, color: u32) -> gpui::Div {
    div().text_color(rgb(color)).child(text.into())
}

fn avatar(image: Option<std::sync::Arc<gpui::RenderImage>>, name: &str, size: f32) -> gpui::Div {
    let initial = name
        .chars()
        .next()
        .map(|ch| ch.to_uppercase().collect::<String>())
        .unwrap_or_else(|| "?".into());
    let content = match image {
        Some(image) => gpui::img(image)
            .w(px(size - 2.0))
            .h(px(size - 2.0))
            .rounded_full()
            .overflow_hidden()
            .object_fit(gpui::ObjectFit::Cover)
            .with_animation(
                SharedString::from("avatar-in"),
                Animation::new(Duration::from_millis(180)),
                |element, delta| element.opacity(delta),
            )
            .into_any_element(),
        None => label(initial, TEXT)
            .font_family("Bahnschrift")
            .text_size(px(size * 0.42))
            .font_weight(FontWeight::SEMIBOLD)
            .into_any_element(),
    };
    div()
        .flex()
        .items_center()
        .justify_center()
        .w(px(size))
        .h(px(size))
        .flex_shrink_0()
        .rounded_full()
        .overflow_hidden()
        .bg(rgb(SURFACE_HOVER))
        .border_1()
        .border_color(rgb(BORDER))
        .child(content)
}

/// Technical microcopy from the identity board: compact, monospaced and used
/// only for orientation/status so body text remains easy to scan.
fn micro(text: impl Into<SharedString>, color: u32) -> gpui::Div {
    label(text, color)
        .font_family("Cascadia Mono")
        .text_size(px(10.0))
        .font_weight(FontWeight::MEDIUM)
}

fn wordmark(size: f32) -> gpui::Div {
    label("O R A N G E", ORANGE)
        .font_family("Bahnschrift")
        .text_size(px(size))
        .font_weight(FontWeight::SEMIBOLD)
}

fn accent_rule(width: f32) -> gpui::Div {
    div().w(px(width)).h(px(2.0)).bg(rgb(ORANGE))
}

/// A raised surface with a hairline border. The border does most of the work:
/// on a dark UI, background alone reads as mush.
fn card() -> gpui::Div {
    div()
        .flex()
        .flex_col()
        .gap_1()
        .p_3()
        .rounded_md()
        .bg(rgb(SURFACE))
        .border_1()
        .border_color(rgb(BORDER))
}

fn primary(id: &'static str, text: impl Into<SharedString>) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .flex()
        .items_center()
        .justify_center()
        .w_full()
        .px_4()
        .py_2()
        .rounded_md()
        .bg(rgb(ORANGE))
        .text_color(rgb(INK))
        .font_family("Bahnschrift")
        .text_size(px(13.0))
        .font_weight(FontWeight::SEMIBOLD)
        .cursor_pointer()
        .hover(|s| s.bg(rgb(0xff6f38)))
        // Press feedback: without it, a click feels like nothing happened
        // until the screen changes.
        .active(|s| s.bg(rgb(ORANGE_DIM)))
        .child(text.into())
}

fn secondary(id: &'static str, text: impl Into<SharedString>) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .flex()
        .items_center()
        .justify_center()
        .w_full()
        .px_4()
        .py_2()
        .rounded_md()
        .bg(rgb(SURFACE))
        .border_1()
        .border_color(rgb(BORDER))
        .text_color(rgb(TEXT))
        .font_family("Bahnschrift")
        .text_size(px(13.0))
        .cursor_pointer()
        .hover(|s| s.bg(rgb(SURFACE_HOVER)).border_color(rgb(0x3a3a3a)))
        .active(|s| s.bg(rgb(BG)))
        .child(text.into())
}

fn quiet(id: &'static str, text: impl Into<SharedString>) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .text_xs()
        .text_color(rgb(FAINT))
        .cursor_pointer()
        .hover(|s| s.text_color(rgb(TEXT)))
        .child(text.into())
}

fn option_pill(
    id: SharedString,
    text: impl Into<SharedString>,
    active: bool,
) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .px_3()
        .py_1()
        .rounded_md()
        .text_xs()
        .cursor_pointer()
        .border_1()
        .border_color(rgb(if active { ORANGE } else { BORDER }))
        .bg(rgb(if active { ORANGE } else { SURFACE }))
        .text_color(rgb(if active { INK } else { MUTED }))
        .when(active, |element| element.font_weight(FontWeight::SEMIBOLD))
        .when(!active, |element| {
            element.hover(|style| {
                style
                    .bg(rgb(SURFACE_HOVER))
                    .border_color(rgb(ORANGE_DIM))
                    .text_color(rgb(TEXT))
            })
        })
        .child(text.into())
}

/// The mark: a scanline eclipse crescent, from the logo exploration.
///
/// Embedded as a PNG rather than drawn, because the scanline texture cannot be
/// expressed with GPUI primitives without hundreds of elements. Hero-sized
/// instances receive a periodic transmit sweep that pushes their scanlines
/// outward as rays; titlebar-sized instances stay static because animation at
/// 18px would only read as flicker.
fn logo(px_size: f32) -> impl IntoElement {
    static STATIC: std::sync::OnceLock<Option<std::sync::Arc<gpui::RenderImage>>> =
        std::sync::OnceLock::new();
    static ANIMATED: std::sync::OnceLock<Option<std::sync::Arc<gpui::RenderImage>>> =
        std::sync::OnceLock::new();

    let animated = px_size >= 80.0;
    let image = (if animated { &ANIMATED } else { &STATIC })
        .get_or_init(|| {
            let bytes = include_bytes!("../logo.png");
            let decoded = image::load_from_memory(bytes).ok()?.into_rgba8();
            let base = if animated {
                // Reserve real canvas to the left of the mark. Extending rays
                // inside the original tightly-cropped PNG only clipped them.
                let scaled = image::imageops::resize(
                    &decoded,
                    96,
                    96,
                    image::imageops::FilterType::Triangle,
                );
                let mut canvas = image::RgbaImage::new(128, 128);
                image::imageops::overlay(&mut canvas, &scaled, 28, 16);
                canvas.into_raw()
            } else {
                decoded.into_raw()
            };
            // A short transmit sweep followed by a hold. Constant motion made
            // the mark feel like a loading spinner; Routine's interfaces use
            // sparse, stateful motion that settles back into stillness.
            let frame_count = if animated { 44 } else { 1 };
            let mut frames = Vec::with_capacity(frame_count);
            for frame_index in 0..frame_count {
                let mut raw = base.clone();
                if animated && frame_index < 20 {
                    let sweep_y = frame_index as f32 / 19.0 * 127.0;
                    for y in 0..128usize {
                        let strength = (1.0 - (y as f32 - sweep_y).abs() / 15.0).max(0.0);
                        if strength <= 0.0 {
                            continue;
                        }
                        let first = (0..128usize).find(|x| base[(y * 128 + x) * 4 + 3] > 16);
                        let Some(first) = first else { continue };
                        let source = (y * 128 + first) * 4;
                        let extension = (strength * 42.0).round() as usize;
                        for distance in 1..=extension.min(first) {
                            let target = (y * 128 + first - distance) * 4;
                            let taper = 1.0 - distance as f32 / (extension + 1) as f32;
                            raw[target] = base[source];
                            raw[target + 1] = base[source + 1];
                            raw[target + 2] = base[source + 2];
                            let ray_alpha = strength.sqrt() * (0.78 + 0.22 * taper);
                            raw[target + 3] = (base[source + 3] as f32 * ray_alpha).round() as u8;
                        }
                    }
                }
                // GPUI wants BGRA; the PNG decodes as RGBA.
                for pixel in raw.chunks_exact_mut(4) {
                    pixel.swap(0, 2);
                }
                let buffer = image::RgbaImage::from_raw(128, 128, raw)?;
                frames.push(if animated {
                    image::Frame::from_parts(buffer, 0, 0, image::Delay::from_numer_denom_ms(45, 1))
                } else {
                    image::Frame::new(buffer)
                });
            }
            Some(std::sync::Arc::new(gpui::RenderImage::new(frames)))
        })
        .clone();

    match image {
        Some(image) => gpui::img(image)
            .id(if animated {
                "logo-animated"
            } else {
                "logo-static"
            })
            .w(px(px_size))
            .h(px(px_size))
            .into_any_element(),
        // If the asset ever fails to decode, a plain disc beats nothing.
        None => div()
            .w(px(px_size))
            .h(px(px_size))
            .rounded_full()
            .bg(rgb(ORANGE))
            .into_any_element(),
    }
}

/// A small coloured dot, for status.
fn dot(color: u32) -> gpui::Div {
    div().w(px(6.0)).h(px(6.0)).rounded_full().bg(rgb(color))
}

/// A dot that breathes, for "this is live right now".
fn live_dot() -> impl IntoElement {
    dot(GREEN).with_animation(
        SharedString::from("live-pulse"),
        Animation::new(Duration::from_millis(1600)).repeat(),
        |el, delta| {
            // Triangle wave, so it fades out and back rather than snapping at
            // the loop point.
            let t = if delta < 0.5 {
                delta * 2.0
            } else {
                (1.0 - delta) * 2.0
            };
            el.opacity(0.35 + 0.65 * t)
        },
    )
}

/// Fade content in. Keyed per screen so navigation reads as a transition
/// rather than an instant swap.
fn fade_in(id: impl Into<SharedString>, element: gpui::AnyElement) -> impl IntoElement {
    div()
        .flex()
        .flex_col()
        .flex_1()
        .min_h(px(0.0))
        .child(element)
        .with_animation(
            id.into(),
            Animation::new(Duration::from_millis(200)),
            |el, delta| el.opacity(delta),
        )
}

impl Render for Orange {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // The picker needs room for a two-column grid; every other screen is a
        // narrow column. Resizing on transition keeps both comfortable rather
        // than compromising on one size for all of them.
        let wanted = match self.screen {
            Screen::PickWindow => size(px(576.0), px(660.0)),
            Screen::Streaming => size(px(480.0), px(640.0)),
            _ => size(px(400.0), px(540.0)),
        };
        if self.sized_for != Some(self.screen) {
            self.sized_for = Some(self.screen);
            window.resize(wanted);
        }

        let key = match self.screen {
            Screen::SignedOut => "signedout",
            Screen::Home => "home",
            Screen::PickWindow => "pick",
            Screen::Streaming => "streaming",
            Screen::Watching => "watching",
            Screen::Settings => "settings",
        };

        let body = match self.screen {
            Screen::SignedOut => self.render_signed_out(cx).into_any_element(),
            Screen::Home => self.render_home(cx).into_any_element(),
            Screen::PickWindow => self.render_pick(cx).into_any_element(),
            Screen::Streaming => self.render_streaming(cx).into_any_element(),
            Screen::Watching => self.render_watching(cx).into_any_element(),
            Screen::Settings => self.render_settings(cx).into_any_element(),
        };

        div()
            .flex()
            .flex_col()
            .size_full()
            .bg(rgb(BG))
            .text_sm()
            .font_family("Segoe UI")
            .child(self.render_titlebar(cx))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_h(px(0.0))
                    // 20px gutters, 16 top, 16 bottom.
                    .px_5()
                    .pt_4()
                    .pb_4()
                    .gap_4()
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .flex_1()
                            .min_h(px(0.0))
                            .child(fade_in(key, body)),
                    )
                    .children(self.error.clone().map(|err| {
                        fade_in(
                            SharedString::from("error"),
                            div()
                                .flex()
                                .gap_2()
                                .items_start()
                                .p_3()
                                .rounded_lg()
                                .bg(rgb(0x241514))
                                .border_1()
                                .border_color(rgb(0x3d211f))
                                .text_xs()
                                .text_color(rgb(DANGER))
                                .child(err)
                                .into_any_element(),
                        )
                    })),
            )
    }
}

impl Orange {
    /// Custom titlebar. GPUI hides the system one via `appears_transparent`,
    /// which its source documents as supported on Windows.
    ///
    /// Dragging needs `window_control_area(Drag)` rather than a mouse handler:
    /// a borderless window is moved by the OS through hit-testing, so the
    /// draggable regions have to be declared. The buttons are deliberately
    /// left out of those regions, or the hit test would swallow their clicks.
    fn render_titlebar(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let breadcrumb = match self.screen {
            Screen::PickWindow => Some("/ SHARE"),
            Screen::Streaming => Some("/ STREAMING"),
            Screen::Watching => Some("/ WATCHING"),
            Screen::Settings => Some("/ SETTINGS"),
            _ => None,
        };

        div()
            .flex()
            .items_center()
            .h(px(44.0))
            .pl_4()
            .pr_1()
            .border_b_1()
            .border_color(rgb(BORDER))
            .child(
                div()
                    .id("titlebar-brand")
                    .flex()
                    .items_center()
                    .gap_2()
                    .window_control_area(gpui::WindowControlArea::Drag)
                    .child(logo(18.0))
                    .child(
                        label("O R A N G E", TEXT)
                            .font_family("Bahnschrift")
                            .text_xs()
                            .font_weight(FontWeight::SEMIBOLD),
                    )
                    .children(breadcrumb.map(|text| micro(text, ORANGE_DIM))),
            )
            // The empty middle is draggable too, so the whole bar behaves as
            // people expect - except where the controls are.
            .child(
                div()
                    .id("titlebar-drag")
                    .flex_1()
                    .h_full()
                    .window_control_area(gpui::WindowControlArea::Drag),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .child(
                        titlebar_button("settings", "⚙︎", SURFACE_HOVER).on_click(cx.listener(
                            |this, _, _, cx| {
                                this.screen = if this.screen == Screen::Settings {
                                    Screen::Home
                                } else {
                                    Screen::Settings
                                };
                                cx.notify();
                            },
                        )),
                    )
                    .child(titlebar_button("minimise", "−", SURFACE_HOVER).on_click(
                        |_, window, _| {
                            window.minimize_window();
                        },
                    ))
                    // Close hides the window completely - no taskbar entry -
                    // while the app keeps running in the tray. Minimise is a
                    // normal minimise; the two should not do the same thing.
                    .child(titlebar_button("close", "×", 0x8c2b28).on_click(|_, _, _| {
                        tray::hide_main_window();
                    })),
            )
    }
}

/// Square, unobtrusive control in the titlebar.
///
/// The hover colour is a parameter rather than something callers add
/// afterwards: GPUI panics if `.hover()` is applied twice to one element.
fn titlebar_button(
    id: &'static str,
    glyph: &'static str,
    hover_bg: u32,
) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .flex()
        .items_center()
        .justify_center()
        .w(px(38.0))
        .h(px(36.0))
        .rounded_md()
        .font_family("Segoe UI Symbol")
        .text_size(px(15.0))
        .text_color(rgb(MUTED))
        .cursor_pointer()
        .hover(move |s| s.bg(rgb(hover_bg)).text_color(rgb(TEXT)))
        .child(glyph)
}

impl Orange {
    fn render_signed_out(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .flex()
            .flex_col()
            .gap_5()
            .flex_1()
            .justify_center()
            .items_center()
            .px_2()
            .child(logo(56.0))
            .child(wordmark(20.0))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_1p5()
                    .items_center()
                    .child(
                        label("Share games directly with friends", TEXT)
                            .font_family("Bahnschrift")
                            .text_xl()
                            .font_weight(FontWeight::SEMIBOLD),
                    )
                    .child(
                        div()
                            .max_w(px(260.0))
                            .text_center()
                            .text_xs()
                            .text_color(rgb(MUTED))
                            .child(
                                "High bitrate, low overhead, straight to your friends. \
                                 Sign in so people know whose stream they are opening.",
                            ),
                    ),
            )
            .child(
                div().w_full().max_w(px(260.0)).child(
                    primary(
                        "signin",
                        if self.logging_in.is_some() {
                            "Waiting for Discord…"
                        } else {
                            "Sign in with Discord"
                        },
                    )
                    .when(self.logging_in.is_some(), |d| d.bg(rgb(ORANGE_DIM)))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.start_login();
                        cx.notify();
                    })),
                ),
            )
            .children(
                self.logging_in.is_some().then(|| {
                    label("Finish in your browser, then come back here.", FAINT).text_xs()
                }),
            )
            // Identity is optional in the protocol - it only attaches a name.
            // Blocking streaming behind it would be a self-imposed limit.
            .child(
                quiet("skip", "Continue without signing in").on_click(cx.listener(
                    |this, _, _, cx| {
                        this.logging_in = None;
                        this.screen = Screen::Home;
                        cx.notify();
                    },
                )),
            )
    }

    fn render_home(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let hosting = self.host.is_some();
        let defaults = format!(
            "{} · {}",
            self.quality().label.to_ascii_uppercase(),
            self.fps
                .map(|fps| format!("{fps} FPS"))
                .unwrap_or_else(|| "AUTO FPS".into())
        );
        let user_name = self
            .session
            .as_ref()
            .map(|session| session.name.clone())
            .unwrap_or_else(|| "Anonymous".into());
        let avatar_image = self.avatar.clone();

        div()
            .flex()
            .flex_col()
            .gap_4()
            .flex_1()
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(micro("READY TO STREAM", GREEN))
                    .child(div().flex_1().h(px(1.0)).bg(rgb(BORDER)))
                    .child(micro(defaults, MUTED)),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .items_center()
                    .justify_center()
                    .gap_3()
                    .child(logo(112.0))
                    .child(wordmark(23.0))
                    .child(accent_rule(28.0))
                    .child(
                        label("Pick a window, share the code, keep playing.", MUTED)
                            .font_family("Cascadia Mono")
                            .text_size(px(10.0)),
                    ),
            )
            .child(
                primary(
                    "start",
                    if hosting {
                        "View active stream"
                    } else {
                        "Start streaming"
                    },
                )
                .on_click(cx.listener(move |this, _, _, cx| {
                    if hosting {
                        this.screen = Screen::Streaming;
                    } else {
                        this.refresh_windows();
                        this.screen = Screen::PickWindow;
                    }
                    cx.notify();
                })),
            )
            .child(
                secondary("join", "Join a stream").on_click(cx.listener(|this, _, _, cx| {
                    // A proper text field is deferred; the code is always
                    // copied from Discord anyway, so paste is the flow.
                    let code = cx
                        .read_from_clipboard()
                        .and_then(|item| item.text())
                        .unwrap_or_default();
                    this.join(code);
                    cx.notify();
                })),
            )
            .child(label("Paste a code first — it joins from your clipboard.", FAINT).text_xs())
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .pt_3()
                    .border_t_1()
                    .border_color(rgb(BORDER))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .child(avatar(avatar_image, &user_name, 26.0))
                            .child(
                                div()
                                    .flex()
                                    .flex_col()
                                    .gap_0p5()
                                    .child(micro("SIGNED IN AS", FAINT))
                                    .child(label(user_name, TEXT).text_xs()),
                            ),
                    )
                    .child(
                        quiet("signout", "Sign out").on_click(cx.listener(|this, _, _, cx| {
                            session::clear();
                            this.session = None;
                            this.avatar = None;
                            this.avatar_rx = None;
                            this.screen = Screen::SignedOut;
                            cx.notify();
                        })),
                    ),
            )
    }

    fn render_pick(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let quality = self.quality();
        let selected = self.quality;
        let count = self.windows.len();
        // Distinguishes "still capturing" from "this window refuses to draw",
        // which previously both showed as "no preview" and made every card
        // flash a failure message before its thumbnail arrived.
        let capturing = self.thumb_rx.is_some();

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
                            .child(micro(format!("{} SOURCES AVAILABLE", count), ORANGE))
                            .child(
                                label("Choose what to share", TEXT)
                                    .font_family("Bahnschrift")
                                    .font_weight(FontWeight::SEMIBOLD),
                            )
                            .child(
                                label("Click a preview to start streaming immediately.", FAINT)
                                    .text_xs(),
                            ),
                    )
                    .child(
                        quiet("refresh", "Refresh").on_click(cx.listener(|this, _, _, cx| {
                            this.refresh_windows();
                            cx.notify();
                        })),
                    ),
            )
            .child(
                div()
                    .id("windows")
                    .flex()
                    .flex_row()
                    .flex_wrap()
                    .gap_3()
                    .flex_1()
                    .min_h(px(0.0))
                    .overflow_y_scroll()
                    .when(count == 0, |d| {
                        d.child(
                            card()
                                .w_full()
                                .items_center()
                                .child(label("No windows found", MUTED).text_xs()),
                        )
                    })
                    .children(
                        self.windows
                            .iter()
                            .cloned()
                            .map(|target| {
                                let is_screen = target.hwnd == 0;
                                let title = if target.title.is_empty() {
                                    target.app_name()
                                } else {
                                    target.title.clone()
                                };
                                let meta = if is_screen {
                                    "Full display · includes notifications".to_string()
                                } else {
                                    format!(
                                        "{} · {}×{}",
                                        target.app_name(),
                                        target.width,
                                        target.height
                                    )
                                };
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
                                    .w(px(252.0))
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
                                            .flex_shrink_0()
                                            .h(px(142.0))
                                            .w_full()
                                            .flex()
                                            .items_center()
                                            .justify_center()
                                            .bg(rgb(0x08070a))
                                            .overflow_hidden()
                                            .child(match (thumb, capturing) {
                                                (Some(image), _) => gpui::img(image)
                                                    .h(px(142.0))
                                                    .with_animation(
                                                        SharedString::from(format!("fade{hwnd}")),
                                                        Animation::new(Duration::from_millis(260)),
                                                        |el, delta| el.opacity(delta),
                                                    )
                                                    .into_any_element(),
                                                (None, true) => label("capturing…", FAINT)
                                                    .text_xs()
                                                    .into_any_element(),
                                                (None, false) => label("no preview", FAINT)
                                                    .text_xs()
                                                    .into_any_element(),
                                            }),
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
                                            .h(px(52.0))
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
                                                    .group_hover(group.clone(), |s| {
                                                        s.text_color(rgb(ORANGE))
                                                    })
                                                    .child(
                                                        div()
                                                            .flex_1()
                                                            .overflow_hidden()
                                                            .whitespace_nowrap()
                                                            .text_ellipsis()
                                                            .child(title),
                                                    )
                                                    .child(
                                                        micro("→", ORANGE)
                                                            .opacity(0.0)
                                                            .group_hover(group.clone(), |s| {
                                                                s.opacity(1.0)
                                                            }),
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
                            })
                            .collect::<Vec<_>>(),
                    ),
            )
            // Quality is a setting, not the task, so it sits in a footer rather
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
                        div()
                            .flex()
                            .items_center()
                            .gap_3()
                            .child(
                                div().flex().gap_1p5().children(
                                    QUALITIES
                                        .iter()
                                        .enumerate()
                                        .map(|(index, q)| {
                                            let active = index == selected;
                                            div()
                                                .id(SharedString::from(format!("q{index}")))
                                                .px_3()
                                                .py_1()
                                                .rounded_md()
                                                .text_xs()
                                                .cursor_pointer()
                                                .border_1()
                                                .border_color(rgb(if active {
                                                    ORANGE
                                                } else {
                                                    BORDER
                                                }))
                                                .bg(rgb(if active { ORANGE } else { SURFACE }))
                                                .text_color(rgb(if active { INK } else { MUTED }))
                                                .when(active, |d| {
                                                    d.font_weight(FontWeight::SEMIBOLD)
                                                })
                                                .when(!active, |d| {
                                                    d.hover(|s| {
                                                        s.bg(rgb(SURFACE_HOVER))
                                                            .text_color(rgb(TEXT))
                                                    })
                                                })
                                                .child(q.label)
                                                .on_click(cx.listener(move |this, _, _, cx| {
                                                    this.quality = index;
                                                    this.save_preferences();
                                                    cx.notify();
                                                }))
                                        })
                                        .collect::<Vec<_>>(),
                                ),
                            )
                            .child(
                                label(format!("~{} Mbps per viewer", quality.mbps), FAINT)
                                    .text_xs(),
                            ),
                    )
                    .child(
                        quiet("back", "← Back").on_click(cx.listener(|this, _, _, cx| {
                            this.screen = Screen::Home;
                            cx.notify();
                        })),
                    ),
            )
    }

    fn render_streaming(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let code = self.code();
        let monitor_code = code.clone();
        let monitor_open = code
            .as_ref()
            .is_some_and(|code| self.watches.iter().any(|watch| &watch.code == code));
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
        let stream_details = format!(
            "{} · {}",
            quality.label,
            self.fps
                .map(|fps| format!("{fps} fps"))
                .unwrap_or_else(|| "display refresh".into())
        );
        let preview = self.active_preview.clone();
        let just_copied = self
            .copied_at
            .map(|t| t.elapsed() < Duration::from_secs(2))
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
                            .h(px(220.0))
                            .flex_shrink_0()
                            .overflow_hidden()
                            .bg(rgb(BG))
                            .child(match preview {
                                Some(image) => {
                                    gpui::img(image).w_full().h(px(220.0)).into_any_element()
                                }
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
                                    .gap_3()
                                    .child(micro("SOURCE PREVIEW", ORANGE))
                                    .children(monitor_code.map(|code| {
                                        if monitor_open {
                                            micro("LIVE MONITOR OPEN", GREEN).into_any_element()
                                        } else {
                                            quiet("live-monitor", "Open live monitor")
                                                .on_click(cx.listener(move |this, _, _, cx| {
                                                    this.open_live_monitor(code.clone());
                                                    cx.notify();
                                                }))
                                                .into_any_element()
                                        }
                                    })),
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
                                    if just_copied { GREEN } else { FAINT },
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
                        cx.notify();
                    })),
            )
    }

    fn render_watching(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
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
                            .child(
                                label(
                                    if watch.monitor {
                                        "Live monitor · bottom-right"
                                    } else {
                                        "Open in its own viewer window"
                                    },
                                    FAINT,
                                )
                                .text_xs(),
                            ),
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
                GREEN,
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
                secondary("leave-all", "Close all viewer windows")
                    .text_color(rgb(DANGER))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.stop_all_watches();
                        cx.notify();
                    })),
            )
    }

    fn render_settings(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let signed_in = self.session.as_ref().map(|s| s.name.clone());
        let selected_quality = self.quality;
        let selected_fps = self.fps;
        let identity = match &signed_in {
            Some(name) => div()
                .flex()
                .items_center()
                .gap_2()
                .child(avatar(self.avatar.clone(), name, 32.0))
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .gap_0p5()
                        .child(label("Discord", TEXT))
                        .child(label(name.clone(), FAINT).text_xs()),
                )
                .into_any_element(),
            None => div()
                .flex()
                .flex_col()
                .gap_0p5()
                .child(label("Discord", TEXT))
                .child(label("Not signed in", FAINT).text_xs())
                .into_any_element(),
        };

        div()
            .flex()
            .flex_col()
            .gap_3()
            .flex_1()
            .child(micro("ACCOUNT AND RELAY", ORANGE))
            .child(
                card()
                    .flex_row()
                    .items_center()
                    .justify_between()
                    .child(identity)
                    .child(match signed_in {
                        Some(_) => quiet("so", "Sign out")
                            .on_click(cx.listener(|this, _, _, cx| {
                                session::clear();
                                this.session = None;
                                this.avatar = None;
                                this.avatar_rx = None;
                                cx.notify();
                            }))
                            .into_any_element(),
                        None => quiet("si", "Sign in")
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.screen = Screen::SignedOut;
                                cx.notify();
                            }))
                            .into_any_element(),
                    }),
            )
            .child(
                card()
                    .gap_2()
                    .child(label("Default quality", TEXT))
                    .child(
                        div().flex().gap_1p5().children(
                            QUALITIES
                                .iter()
                                .enumerate()
                                .map(|(index, quality)| {
                                    option_pill(
                                        SharedString::from(format!("settings-quality-{index}")),
                                        quality.label,
                                        index == selected_quality,
                                    )
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.quality = index;
                                        this.save_preferences();
                                        cx.notify();
                                    }))
                                })
                                .collect::<Vec<_>>(),
                        ),
                    )
                    .child(
                        label("Used when a new stream starts. You can still change it before sharing.", FAINT)
                            .text_xs(),
                    ),
            )
            .child(
                card()
                    .gap_2()
                    .child(label("Frame rate", TEXT))
                    .child(
                        div().flex().gap_1p5().children(
                            [
                                (None, "Auto"),
                                (Some(60), "60"),
                                (Some(120), "120"),
                                (Some(240), "240"),
                            ]
                            .into_iter()
                            .map(|(fps, text)| {
                                option_pill(
                                    SharedString::from(format!(
                                        "settings-fps-{}",
                                        fps.unwrap_or(0)
                                    )),
                                    text,
                                    fps == selected_fps,
                                )
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.fps = fps;
                                    this.save_preferences();
                                    cx.notify();
                                }))
                            })
                            .collect::<Vec<_>>(),
                        ),
                    )
                    .child(
                        label("Auto follows the refresh rate of the display being captured.", FAINT)
                            .text_xs(),
                    ),
            )
            .child(div().flex_1())
            .child(
                secondary("back-settings", "Done").on_click(cx.listener(|this, _, _, cx| {
                    this.screen = Screen::Home;
                    cx.notify();
                })),
            )
    }
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
    let tray_events = tray::install().ok();

    Application::new().run(move |cx: &mut App| {
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
                    ..Default::default()
                },
                |_, cx| cx.new(Orange::new),
            )
            .unwrap();
        cx.activate(true);

        // Closing the window hides it instead of quitting: a tray app should
        // keep streaming when its window is dismissed. Quit lives in the tray
        // menu.
        let _ = window.update(cx, |_, window, cx| {
            window.on_window_should_close(cx, |window, _cx| {
                window.minimize_window();
                false
            });
        });

        // The tray runs its own Win32 message loop on another thread, so its
        // events arrive over a channel and are drained on a timer here.
        if let Some(events) = tray_events {
            cx.spawn(async move |cx| loop {
                Timer::after(Duration::from_millis(200)).await;
                while let Ok(event) = events.try_recv() {
                    match event {
                        tray::TrayEvent::Show => {
                            // The window may be hidden rather than merely
                            // unfocused, so un-hide before activating.
                            tray::show_main_window();
                            let _ = cx.update(|cx| {
                                let _ = window.update(cx, |_, window, _| {
                                    window.activate_window();
                                });
                            });
                        }
                        tray::TrayEvent::Quit => {
                            let _ = cx.update(|cx| cx.quit());
                            return;
                        }
                    }
                }
            })
            .detach();
        }
    });
}
