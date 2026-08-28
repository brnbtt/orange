//! orange tray - the host-side UI.
//!
//! GPUI fits here precisely because there is no video: this is ordinary UI.
//! The viewer window stays native because GPUI's `Surface` element has no
//! Windows implementation - its only variant is macOS-gated.

// Without this the binary is a console application and Windows opens a black
// cmd window behind the UI.
#![windows_subsystem = "windows"]

mod session;
mod supervisor;
mod tray;

use gpui::{
    div, prelude::*, px, rgb, size, App, Application, Bounds, Context, FontWeight, SharedString,
    Timer, TitlebarOptions, Window, WindowBounds, WindowOptions,
};
use std::time::{Duration, Instant};
use supervisor::{LoginAttempt, Quality, Supervisor, WindowTarget, QUALITIES};

// A deliberately small palette. Three surface levels give enough depth without
// the UI turning into a gradient soup.
const BG: u32 = 0x0e0e10;
const SURFACE: u32 = 0x17171b;
const SURFACE_HOVER: u32 = 0x1f1f25;
const BORDER: u32 = 0x26262d;
const TEXT: u32 = 0xf2f2f3;
const MUTED: u32 = 0x82828c;
const FAINT: u32 = 0x5a5a63;
const ORANGE: u32 = 0xff7a00;
const ORANGE_DIM: u32 = 0x8a4400;
const INK: u32 = 0x140c04;
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

struct Orange {
    screen: Screen,
    session: Option<session::Session>,
    windows: Vec<WindowTarget>,
    quality: usize,
    stream: Option<Supervisor>,
    logging_in: Option<LoginAttempt>,
    error: Option<String>,
    server: String,
    /// Drives the transient "Copied" confirmation on the share code.
    copied_at: Option<Instant>,
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
        Self {
            screen: if session.is_some() {
                Screen::Home
            } else {
                Screen::SignedOut
            },
            session,
            windows: Vec::new(),
            quality: 1,
            stream: None,
            logging_in: None,
            error: None,
            server: std::env::var("ORANGE_SERVER").unwrap_or_else(|_| DEFAULT_SERVER.to_string()),
            copied_at: None,
        }
    }

    fn tick(&mut self, cx: &mut Context<Self>) {
        // Login happens in a child process; notice when it lands, and when it
        // dies without producing a session.
        if self.logging_in.is_some() {
            if let Some(session) = session::load() {
                self.session = Some(session);
                self.logging_in = None;
                self.screen = Screen::Home;
            } else if let Some(reason) = self.logging_in.as_mut().and_then(|a| a.failure()) {
                self.error = Some(reason);
                self.logging_in = None;
            }
        }

        // A child that exited on its own returns us to the home screen.
        if matches!(self.screen, Screen::Streaming | Screen::Watching) {
            let alive = self.stream.as_mut().map(|s| s.running()).unwrap_or(false);
            if !alive {
                if let Some(status) = self.stream.as_ref().and_then(|s| s.status.lock().ok()) {
                    self.error = status.error.clone();
                }
                self.stream = None;
                self.screen = Screen::Home;
            }
        }
        cx.notify();
    }

    fn quality(&self) -> Quality {
        QUALITIES[self.quality.min(QUALITIES.len() - 1)]
    }

    fn refresh_windows(&mut self) {
        match supervisor::list_windows() {
            Ok(mut windows) => {
                // Never offer our own windows as a capture target.
                windows.retain(|w| !w.process.to_lowercase().starts_with("orange"));
                self.windows = windows;
                self.error = None;
            }
            Err(err) => self.error = Some(err.to_string()),
        }
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
        match Supervisor::host(&target, &self.quality(), &self.server) {
            Ok(stream) => {
                self.stream = Some(stream);
                self.screen = Screen::Streaming;
                self.error = None;
            }
            Err(err) => self.error = Some(err.to_string()),
        }
    }

    fn join(&mut self, code: String) {
        let code = code.trim().to_ascii_uppercase();
        if code.is_empty() {
            self.error = Some("No code on the clipboard".into());
            return;
        }
        if !supervisor::gstreamer_available() {
            self.error = Some(
                "GStreamer was not found. Install it with: winget install gstreamerproject.gstreamer"
                    .into(),
            );
            return;
        }
        match Supervisor::watch(&code, &self.server) {
            Ok(stream) => {
                self.stream = Some(stream);
                self.screen = Screen::Watching;
                self.error = None;
            }
            Err(err) => self.error = Some(err.to_string()),
        }
    }

    fn stop(&mut self) {
        if let Some(mut stream) = self.stream.take() {
            stream.stop();
        }
        self.screen = Screen::Home;
    }

    fn code(&self) -> Option<String> {
        self.stream
            .as_ref()
            .and_then(|s| s.status.lock().ok())
            .and_then(|st| st.code.clone())
    }

    fn viewers(&self) -> Vec<String> {
        self.stream
            .as_ref()
            .and_then(|s| s.status.lock().ok())
            .map(|st| st.viewers.clone())
            .unwrap_or_default()
    }
}

// --- shared pieces ----------------------------------------------------------

fn label(text: impl Into<SharedString>, color: u32) -> gpui::Div {
    div().text_color(rgb(color)).child(text.into())
}

/// A raised surface with a hairline border. The border does most of the work:
/// on a dark UI, background alone reads as mush.
fn card() -> gpui::Div {
    div()
        .flex()
        .flex_col()
        .gap_1()
        .p_3()
        .rounded_lg()
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
        .py_2p5()
        .rounded_lg()
        .bg(rgb(ORANGE))
        .text_color(rgb(INK))
        .font_weight(FontWeight::SEMIBOLD)
        .cursor_pointer()
        .hover(|s| s.bg(rgb(0xff8c1f)))
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
        .py_2p5()
        .rounded_lg()
        .bg(rgb(SURFACE))
        .border_1()
        .border_color(rgb(BORDER))
        .text_color(rgb(TEXT))
        .cursor_pointer()
        .hover(|s| s.bg(rgb(SURFACE_HOVER)))
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

/// The mark: an eclipse. A ring that thickens downward, drawn as two circles
/// rather than an imported asset so there is no icon pipeline for one shape.
///
/// Placeholder — the real logo is still being designed.
fn logo(px_size: f32) -> gpui::Div {
    div()
        .relative()
        .w(px(px_size))
        .h(px(px_size))
        .child(
            div()
                .absolute()
                .top_0()
                .left_0()
                .w(px(px_size))
                .h(px(px_size))
                .rounded_full()
                .bg(rgb(ORANGE)),
        )
        .child(
            // The eclipsing body, offset upward so the crescent gathers below.
            div()
                .absolute()
                .top(px(-px_size * 0.16))
                .left(px(px_size * 0.08))
                .w(px(px_size * 0.84))
                .h(px(px_size * 0.84))
                .rounded_full()
                .bg(rgb(BG)),
        )
}

/// A small coloured dot, for status.
fn dot(color: u32) -> gpui::Div {
    div().w(px(6.0)).h(px(6.0)).rounded_full().bg(rgb(color))
}

impl Render for Orange {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
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
                    .px_5()
                    .pt_4()
                    .pb_5()
                    .gap_3()
                    .child(body)
                    .children(self.error.clone().map(|err| {
                        div()
                            .p_3()
                            .rounded_lg()
                            .bg(rgb(0x241514))
                            .border_1()
                            .border_color(rgb(0x3d211f))
                            .text_xs()
                            .text_color(rgb(DANGER))
                            .child(err)
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
            Screen::PickWindow => Some("· share"),
            Screen::Settings => Some("· settings"),
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
                    .gap_2p5()
                    .window_control_area(gpui::WindowControlArea::Drag)
                    .child(logo(16.0))
                    .child(
                        div()
                            .text_xs()
                            .font_weight(FontWeight::BOLD)
                            .text_color(rgb(TEXT))
                            .child("ORANGE"),
                    )
                    .children(breadcrumb.map(|text| {
                        div().text_xs().text_color(rgb(FAINT)).child(text)
                    })),
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
                    .child(titlebar_button("settings", "⚙", SURFACE_HOVER).on_click(
                        cx.listener(|this, _, _, cx| {
                            this.screen = if this.screen == Screen::Settings {
                                Screen::Home
                            } else {
                                Screen::Settings
                            };
                            cx.notify();
                        }),
                    ))
                    .child(
                        titlebar_button("minimise", "—", SURFACE_HOVER).on_click(
                            |_, window, _| {
                                window.minimize_window();
                            },
                        ),
                    )
                    // Close hides the window completely - no taskbar entry -
                    // while the app keeps running in the tray. Minimise is a
                    // normal minimise; the two should not do the same thing.
                    .child(
                        titlebar_button("close", "✕", 0x8c2b28).on_click(|_, _, _| {
                            tray::hide_main_window();
                        }),
                    ),
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
        .text_xs()
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
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_1p5()
                    .items_center()
                    .child(
                        label("Share a window", TEXT)
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
            .children(self.logging_in.is_some().then(|| {
                label("Finish in your browser, then come back here.", FAINT).text_xs()
            }))
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
        div()
            .flex()
            .flex_col()
            .gap_2()
            .flex_1()
            .child(
                primary("start", "Start streaming").on_click(cx.listener(|this, _, _, cx| {
                    this.refresh_windows();
                    this.screen = Screen::PickWindow;
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
            .child(div().flex_1())
            .child(
                quiet("signout", "Sign out").on_click(cx.listener(|this, _, _, cx| {
                    session::clear();
                    this.session = None;
                    this.screen = Screen::SignedOut;
                    cx.notify();
                })),
            )
    }

    fn render_pick(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let quality = self.quality();
        let selected = self.quality;
        let count = self.windows.len();

        div()
            .flex()
            .flex_col()
            .gap_3()
            .flex_1()
            .min_h(px(0.0))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .child(label("Quality", MUTED).text_xs())
                    .child(
                        div().flex().gap_1p5().children(
                            QUALITIES
                                .iter()
                                .enumerate()
                                .map(|(index, q)| {
                                    let active = index == selected;
                                    div()
                                        .id(SharedString::from(format!("q{index}")))
                                        .flex_1()
                                        .flex()
                                        .justify_center()
                                        .px_3()
                                        .py_1p5()
                                        .rounded_lg()
                                        .cursor_pointer()
                                        .border_1()
                                        .border_color(rgb(if active { ORANGE } else { BORDER }))
                                        .bg(rgb(if active { ORANGE } else { SURFACE }))
                                        .text_color(rgb(if active { INK } else { MUTED }))
                                        .when(active, |d| d.font_weight(FontWeight::SEMIBOLD))
                                        .hover(|s| s.border_color(rgb(ORANGE)))
                                        .child(q.label)
                                        .on_click(cx.listener(move |this, _, _, cx| {
                                            this.quality = index;
                                            cx.notify();
                                        }))
                                })
                                .collect::<Vec<_>>(),
                        ),
                    )
                    .child(
                        label(
                            format!("about {} Mbps upload for each viewer", quality.mbps),
                            FAINT,
                        )
                        .text_xs(),
                    ),
            )
            .child(
                div()
                    .flex()
                    .justify_between()
                    .items_center()
                    .child(label("Window", MUTED).text_xs())
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
                    .flex_col()
                    .gap_1p5()
                    .flex_1()
                    .min_h(px(0.0))
                    .overflow_y_scroll()
                    .when(count == 0, |d| {
                        d.child(
                            card()
                                .items_center()
                                .child(label("No windows found", MUTED).text_xs()),
                        )
                    })
                    .children(
                        self.windows
                            .iter()
                            .cloned()
                            .map(|target| {
                                let title = if target.title.is_empty() {
                                    target.app_name()
                                } else {
                                    target.title.clone()
                                };
                                let meta = format!(
                                    "{} · {}×{}",
                                    target.app_name(),
                                    target.width,
                                    target.height
                                );
                                card()
                                    .id(SharedString::from(format!("w{}", target.hwnd)))
                                    .gap_0p5()
                                    .py_2p5()
                                    .cursor_pointer()
                                    .hover(|s| {
                                        s.bg(rgb(SURFACE_HOVER)).border_color(rgb(ORANGE_DIM))
                                    })
                                    .child(
                                        div()
                                            .overflow_hidden()
                                            .text_color(rgb(TEXT))
                                            .child(title),
                                    )
                                    .child(label(meta, FAINT).text_xs())
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.start_stream(target.clone());
                                        cx.notify();
                                    }))
                            })
                            .collect::<Vec<_>>(),
                    ),
            )
            .child(
                quiet("back", "← Back").on_click(cx.listener(|this, _, _, cx| {
                    this.screen = Screen::Home;
                    cx.notify();
                })),
            )
    }

    fn render_streaming(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let code = self.code();
        let viewers = self.viewers();
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
                    .gap_1p5()
                    .child(dot(if code.is_some() { GREEN } else { MUTED }))
                    .child(
                        label(if code.is_some() { "Live" } else { "Starting…" }, TEXT)
                            .font_weight(FontWeight::SEMIBOLD),
                    ),
            )
            .child(match code.clone() {
                Some(code) => card()
                    .id("code")
                    .items_center()
                    .py_4()
                    .gap_2()
                    .cursor_pointer()
                    .hover(|s| s.bg(rgb(SURFACE_HOVER)).border_color(rgb(ORANGE_DIM)))
                    .child(
                        div()
                            .text_color(rgb(ORANGE))
                            .text_size(px(30.0))
                            .font_weight(FontWeight::BOLD)
                            .child(code.clone()),
                    )
                    .child(
                        label(
                            if just_copied {
                                "Copied to clipboard"
                            } else {
                                "Click to copy"
                            },
                            if just_copied { GREEN } else { FAINT },
                        )
                        .text_xs(),
                    )
                    .on_click(cx.listener(move |this, _, _, cx| {
                        cx.write_to_clipboard(gpui::ClipboardItem::new_string(code.clone()));
                        this.copied_at = Some(Instant::now());
                        cx.notify();
                    }))
                    .into_any_element(),
                None => card()
                    .items_center()
                    .py_4()
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
                    .child(label(
                        if viewers.is_empty() {
                            "Nobody watching yet".to_string()
                        } else {
                            format!("{} watching", viewers.len())
                        },
                        MUTED,
                    ).text_xs())
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
                secondary("stop", "Stop streaming")
                    .text_color(rgb(DANGER))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.stop();
                        cx.notify();
                    })),
            )
    }

    fn render_watching(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .flex()
            .flex_col()
            .gap_3()
            .flex_1()
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_1p5()
                    .child(dot(GREEN))
                    .child(label("Watching", TEXT).font_weight(FontWeight::SEMIBOLD)),
            )
            .child(
                card()
                    .items_center()
                    .py_4()
                    .child(label("The stream is in its own window", MUTED).text_xs())
                    .child(label("Esc closes it · drag anywhere to move", FAINT).text_xs()),
            )
            .child(div().flex_1())
            .child(
                secondary("leave", "Leave")
                    .text_color(rgb(DANGER))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.stop();
                        cx.notify();
                    })),
            )
    }

    fn render_settings(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let signed_in = self.session.as_ref().map(|s| s.name.clone());

        div()
            .flex()
            .flex_col()
            .gap_3()
            .flex_1()
            .child(
                card()
                    .flex_row()
                    .items_center()
                    .justify_between()
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap_0p5()
                            .child(label("Discord", TEXT))
                            .child(
                                label(
                                    signed_in.clone().unwrap_or_else(|| "Not signed in".into()),
                                    FAINT,
                                )
                                .text_xs(),
                            ),
                    )
                    .child(match signed_in {
                        Some(_) => quiet("so", "Sign out")
                            .on_click(cx.listener(|this, _, _, cx| {
                                session::clear();
                                this.session = None;
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
                    .child(label("Relay", TEXT))
                    .child(label(self.server.clone(), FAINT).text_xs()),
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
