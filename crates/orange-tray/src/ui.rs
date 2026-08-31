use gpui::{div, prelude::*, px, rgb, Animation, AnimationExt, FontWeight, SharedString};
use std::time::Duration;

// Palette from the logo exploration.
pub(super) const BG: u32 = 0x0b0b0b;
pub(super) const SURFACE: u32 = 0x161616;
pub(super) const SURFACE_HOVER: u32 = 0x202020;
pub(super) const BORDER: u32 = 0x2a2a2a;
pub(super) const TEXT: u32 = 0xe6e0d1;
pub(super) const MUTED: u32 = 0x99948a;
pub(super) const FAINT: u32 = 0x66625b;
pub(super) const ORANGE: u32 = 0xff5a1f;
pub(super) const ORANGE_DIM: u32 = 0x8a3110;
pub(super) const INK: u32 = 0x0b0b0b;
pub(super) const DANGER: u32 = 0xe0645f;
pub(super) const GREEN: u32 = 0x4ec97a;
pub(super) const PICKER_PREVIEW_HEIGHT: f32 = 142.0;
pub(super) const PICKER_DETAILS_HEIGHT: f32 = 52.0;
pub(super) const PICKER_CARD_HEIGHT: f32 = PICKER_PREVIEW_HEIGHT + PICKER_DETAILS_HEIGHT;

pub(super) fn label(text: impl Into<SharedString>, color: u32) -> gpui::Div {
    div().text_color(rgb(color)).child(text.into())
}

pub(super) fn avatar(
    image: Option<std::sync::Arc<gpui::RenderImage>>,
    name: &str,
    size: f32,
) -> gpui::Div {
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
pub(super) fn micro(text: impl Into<SharedString>, color: u32) -> gpui::Div {
    label(text, color)
        .font_family("Cascadia Mono")
        .text_size(px(10.0))
        .font_weight(FontWeight::MEDIUM)
}

pub(super) fn wordmark(size: f32) -> gpui::Div {
    label("O R A N G E", ORANGE)
        .font_family("Bahnschrift")
        .text_size(px(size))
        .font_weight(FontWeight::SEMIBOLD)
}

pub(super) fn accent_rule(width: f32) -> gpui::Div {
    div().w(px(width)).h(px(2.0)).bg(rgb(ORANGE))
}

/// A raised surface with a hairline border. The border does most of the work:
/// on a dark UI, background alone reads as mush.
pub(super) fn card() -> gpui::Div {
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

pub(super) fn primary(
    id: &'static str,
    text: impl Into<SharedString>,
) -> gpui::Stateful<gpui::Div> {
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

pub(super) fn secondary(
    id: &'static str,
    text: impl Into<SharedString>,
) -> gpui::Stateful<gpui::Div> {
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

pub(super) fn quiet(id: &'static str, text: impl Into<SharedString>) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .text_xs()
        .text_color(rgb(FAINT))
        .cursor_pointer()
        .hover(|s| s.text_color(rgb(TEXT)))
        .child(text.into())
}

pub(super) fn update_action(
    id: &'static str,
    text: impl Into<SharedString>,
) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .flex()
        .items_center()
        .justify_center()
        .min_w(px(104.0))
        .h(px(36.0))
        .px_3()
        .rounded_md()
        .bg(rgb(ORANGE))
        .text_color(rgb(INK))
        .font_family("Bahnschrift")
        .text_xs()
        .font_weight(FontWeight::SEMIBOLD)
        .cursor_pointer()
        .hover(|style| style.bg(rgb(0xff6f38)))
        .active(|style| style.bg(rgb(ORANGE_DIM)))
        .child(text.into())
}

pub(super) fn option_pill(
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
fn logo_element_id(animated: bool, epoch: u64) -> SharedString {
    if animated {
        SharedString::from(format!("logo-animated-{epoch}"))
    } else {
        SharedString::from("logo-static")
    }
}

pub(super) fn logo(px_size: f32, epoch: u64) -> impl IntoElement {
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
                for pixel in raw.as_chunks_mut::<4>().0 {
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
            .id(logo_element_id(animated, epoch))
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
pub(super) fn dot(color: u32) -> gpui::Div {
    div().w(px(6.0)).h(px(6.0)).rounded_full().bg(rgb(color))
}

/// A dot that breathes, for "this is live right now".
pub(super) fn live_dot() -> impl IntoElement {
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
pub(super) fn fade_in(id: impl Into<SharedString>, element: gpui::AnyElement) -> impl IntoElement {
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

/// Square, unobtrusive control in the titlebar.
///
/// The hover colour is a parameter rather than something callers add
/// afterwards: GPUI panics if `.hover()` is applied twice to one element.
pub(super) fn titlebar_button(
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn animated_logo_identity_changes_when_the_window_reopens() {
        assert_ne!(logo_element_id(true, 0), logo_element_id(true, 1));
        assert_eq!(logo_element_id(false, 0), logo_element_id(false, 1));
    }

    #[test]
    fn picker_cards_use_content_height_instead_of_filling_the_viewport() {
        assert_eq!(
            PICKER_CARD_HEIGHT,
            PICKER_PREVIEW_HEIGHT + PICKER_DETAILS_HEIGHT
        );
    }
}
