//! orange tray - the host-side UI.
//!
//! GPUI fits here precisely because there is no video: this is ordinary UI.
//! The viewer window stays native because GPUI's `Surface` element has no
//! Windows implementation - its only variant is macOS-gated.

mod session;
mod supervisor;

use gpui::{
    div, prelude::*, px, rgb, size, App, Application, Bounds, Context, FontWeight, SharedString,
    Timer, TitlebarOptions, Window, WindowBounds, WindowOptions,
};
use std::time::Duration;
use supervisor::{Quality, Supervisor, WindowTarget, QUALITIES};

const BG: u32 = 0x121214;
const PANEL: u32 = 0x1c1c20;
const TEXT: u32 = 0xeeeeee;
const MUTED: u32 = 0x8a8a92;
const ORANGE: u32 = 0xff7a00;
const DANGER: u32 = 0xd9534f;

const DEFAULT_SERVER: &str =
    "wss://orange-relay.redmushroom-80c79f12.brazilsouth.azurecontainerapps.io/ws";

#[derive(PartialEq, Clone, Copy)]
enum Screen {
    SignedOut,
    Home,
    PickWindow,
    Streaming,
    Watching,
}

struct Orange {
    screen: Screen,
    session: Option<session::Session>,
    windows: Vec<WindowTarget>,
    quality: usize,
    stream: Option<Supervisor>,
    logging_in: bool,
    error: Option<String>,
    server: String,
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
            logging_in: false,
            error: None,
            server: std::env::var("ORANGE_SERVER").unwrap_or_else(|_| DEFAULT_SERVER.to_string()),
        }
    }

    fn tick(&mut self, cx: &mut Context<Self>) {
        // Login happens in a child process; notice when it lands.
        if self.logging_in {
            if let Some(session) = session::load() {
                self.session = Some(session);
                self.logging_in = false;
                self.screen = Screen::Home;
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
        self.logging_in = true;
        self.error = None;
        if let Err(err) = supervisor::start_login() {
            self.error = Some(err.to_string());
            self.logging_in = false;
        }
    }

    fn start_stream(&mut self, target: WindowTarget) {
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

fn card() -> gpui::Div {
    div()
        .flex()
        .flex_col()
        .gap_1()
        .p_3()
        .rounded_md()
        .bg(rgb(PANEL))
}

impl Render for Orange {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let body = match self.screen {
            Screen::SignedOut => self.render_signed_out(cx).into_any_element(),
            Screen::Home => self.render_home(cx).into_any_element(),
            Screen::PickWindow => self.render_pick(cx).into_any_element(),
            Screen::Streaming => self.render_streaming(cx).into_any_element(),
            Screen::Watching => self.render_watching(cx).into_any_element(),
        };

        div()
            .flex()
            .flex_col()
            .size_full()
            .bg(rgb(BG))
            .p_4()
            .gap_3()
            .text_sm()
            .child(
                div()
                    .flex()
                    .justify_between()
                    .items_center()
                    .child(
                        label("orange", ORANGE)
                            .text_lg()
                            .font_weight(FontWeight::BOLD),
                    )
                    .child(match &self.session {
                        Some(s) => label(s.name.clone(), MUTED).text_xs().into_any_element(),
                        None => div().into_any_element(),
                    }),
            )
            .child(body)
            .children(self.error.clone().map(|err| {
                div()
                    .p_2()
                    .rounded_md()
                    .bg(rgb(0x2a1a1a))
                    .text_xs()
                    .text_color(rgb(DANGER))
                    .child(err)
            }))
    }
}

impl Orange {
    fn render_signed_out(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .flex()
            .flex_col()
            .gap_3()
            .flex_1()
            .justify_center()
            .items_center()
            .child(label("Share a window with friends", TEXT))
            .child(
                label(
                    if self.logging_in {
                        "Waiting for Discord in your browser..."
                    } else {
                        "Sign in so people can see who is streaming"
                    },
                    MUTED,
                )
                .text_xs(),
            )
            .child(
                div()
                    .id("signin")
                    .px_4()
                    .py_2()
                    .rounded_md()
                    .bg(rgb(ORANGE))
                    .text_color(rgb(0x1a1a1a))
                    .font_weight(FontWeight::SEMIBOLD)
                    .cursor_pointer()
                    .hover(|s| s.opacity(0.9))
                    .child(if self.logging_in {
                        "Waiting..."
                    } else {
                        "Sign in with Discord"
                    })
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.start_login();
                        cx.notify();
                    })),
            )
    }

    fn render_home(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .flex()
            .flex_col()
            .gap_2()
            .child(
                div()
                    .id("start")
                    .p_3()
                    .rounded_md()
                    .bg(rgb(ORANGE))
                    .text_color(rgb(0x1a1a1a))
                    .font_weight(FontWeight::SEMIBOLD)
                    .cursor_pointer()
                    .hover(|s| s.opacity(0.9))
                    .child("Start streaming")
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.refresh_windows();
                        this.screen = Screen::PickWindow;
                        cx.notify();
                    })),
            )
            .child(
                div()
                    .id("join")
                    .p_3()
                    .rounded_md()
                    .bg(rgb(PANEL))
                    .text_color(rgb(TEXT))
                    .cursor_pointer()
                    .hover(|s| s.bg(rgb(0x26262c)))
                    .child("Join a stream (paste code)")
                    .on_click(cx.listener(|this, _, _, cx| {
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
            .child(
                div()
                    .id("signout")
                    .mt_2()
                    .text_xs()
                    .text_color(rgb(MUTED))
                    .cursor_pointer()
                    .hover(|s| s.text_color(rgb(TEXT)))
                    .child("Sign out")
                    .on_click(cx.listener(|this, _, _, cx| {
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

        div()
            .flex()
            .flex_col()
            .gap_2()
            .flex_1()
            .child(label("Choose a window", TEXT).font_weight(FontWeight::SEMIBOLD))
            .child(
                div().flex().gap_2().children(
                    QUALITIES
                        .iter()
                        .enumerate()
                        .map(|(index, q)| {
                            div()
                                .id(SharedString::from(format!("q{index}")))
                                .px_3()
                                .py_1()
                                .rounded_md()
                                .cursor_pointer()
                                .bg(rgb(if index == selected { ORANGE } else { PANEL }))
                                .text_color(rgb(if index == selected { 0x1a1a1a } else { MUTED }))
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
                    format!("~{} Mbps upload per viewer", quality.mbps),
                    MUTED,
                )
                .text_xs(),
            )
            .child(
                div()
                    .id("windows")
                    .flex()
                    .flex_col()
                    .gap_1()
                    .flex_1()
                    .overflow_y_scroll()
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
                                let meta =
                                    format!("{} · {}x{}", target.app_name(), target.width, target.height);
                                card()
                                    .id(SharedString::from(format!("w{}", target.hwnd)))
                                    .cursor_pointer()
                                    .hover(|s| s.bg(rgb(0x26262c)))
                                    .child(label(title, TEXT))
                                    .child(label(meta, MUTED).text_xs())
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.start_stream(target.clone());
                                        cx.notify();
                                    }))
                            })
                            .collect::<Vec<_>>(),
                    ),
            )
            .child(
                div()
                    .id("back")
                    .text_xs()
                    .text_color(rgb(MUTED))
                    .cursor_pointer()
                    .child("Back")
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.screen = Screen::Home;
                        cx.notify();
                    })),
            )
    }

    fn render_streaming(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let code = self.code();
        let viewers = self.viewers();

        div()
            .flex()
            .flex_col()
            .gap_3()
            .flex_1()
            .child(label("You are live", TEXT).font_weight(FontWeight::SEMIBOLD))
            .child(match code.clone() {
                Some(code) => card()
                    .id("code")
                    .cursor_pointer()
                    .hover(|s| s.bg(rgb(0x26262c)))
                    .child(
                        label(code.clone(), ORANGE)
                            .text_2xl()
                            .font_weight(FontWeight::BOLD),
                    )
                    .child(label("Click to copy", MUTED).text_xs())
                    .on_click(cx.listener(move |_, _, _, cx| {
                        cx.write_to_clipboard(gpui::ClipboardItem::new_string(code.clone()));
                    }))
                    .into_any_element(),
                None => card()
                    .child(label("Starting...", MUTED))
                    .into_any_element(),
            })
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
                    .map(|name| label(name, TEXT).text_xs())
                    .collect::<Vec<_>>(),
            )
            .child(div().flex_1())
            .child(
                div()
                    .id("stop")
                    .p_3()
                    .rounded_md()
                    .bg(rgb(PANEL))
                    .text_color(rgb(DANGER))
                    .cursor_pointer()
                    .hover(|s| s.bg(rgb(0x2a1a1a)))
                    .child("Stop streaming")
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
            .child(label("Watching", TEXT).font_weight(FontWeight::SEMIBOLD))
            .child(label("The stream opens in its own window.", MUTED).text_xs())
            .child(div().flex_1())
            .child(
                div()
                    .id("leave")
                    .p_3()
                    .rounded_md()
                    .bg(rgb(PANEL))
                    .text_color(rgb(DANGER))
                    .cursor_pointer()
                    .child("Leave")
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.stop();
                        cx.notify();
                    })),
            )
    }
}

fn main() {
    Application::new().run(|cx: &mut App| {
        let bounds = Bounds::centered(None, size(px(400.0), px(540.0)), cx);
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                titlebar: Some(TitlebarOptions {
                    title: Some("orange".into()),
                    ..Default::default()
                }),
                ..Default::default()
            },
            |_, cx| cx.new(Orange::new),
        )
        .unwrap();
        cx.activate(true);
    });
}
