//! Smoke test: does GPUI build and open a window on Windows?
use gpui::{
    div, prelude::*, px, rgb, size, App, Application, Bounds, Context, TitlebarOptions, Window,
    WindowBounds, WindowOptions,
};

struct Probe;

impl Render for Probe {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .flex()
            .flex_col()
            .gap_2()
            .p_8()
            .bg(rgb(0x141416))
            .size_full()
            .child(div().text_xl().text_color(rgb(0xff7a00)).child("orange"))
            .child(
                div()
                    .text_color(rgb(0x9a9a9a))
                    .child("GPUI is running on Windows."),
            )
    }
}

fn main() {
    Application::new().run(|cx: &mut App| {
        let bounds = Bounds::centered(None, size(px(520.0), px(420.0)), cx);
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                titlebar: Some(TitlebarOptions {
                    title: Some("orange".into()),
                    ..Default::default()
                }),
                ..Default::default()
            },
            |_, cx| cx.new(|_| Probe),
        )
        .unwrap();
        cx.activate(true);
    });
}
