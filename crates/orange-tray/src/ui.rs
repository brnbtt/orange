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
/// Height of the custom titlebar. The toast layer hangs directly below it, so
/// the two have to agree.
pub(super) const TITLEBAR_HEIGHT: f32 = 44.0;

/// The app's motion vocabulary.
///
/// Two durations, not five. Every animation here used to name its own number -
/// 160, 180, 200, 260, 1600 - with no reason for any of them being different,
/// which is how the app ended up feeling assembled rather than designed.
///
/// Everything that appears shares one decelerating curve. Note that eased is
/// not slower: `ease_out_quint` front-loads the change, so 220ms eased reads
/// faster than the 200ms linear fades it replaces.
pub(super) mod motion {
    use std::time::Duration;

    /// A detail changing in place: a line of text, a value.
    pub const QUICK: Duration = Duration::from_millis(140);
    /// Anything arriving: a card, a toast, a thumbnail, a whole screen.
    pub const ENTER: Duration = Duration::from_millis(220);
    /// How much longer each successive item in a group takes. Same start,
    /// staggered finish, because GPUI has no delay primitive.
    pub const STAGGER: Duration = Duration::from_millis(60);
    /// One breath of the live indicator.
    pub const BREATH: Duration = Duration::from_millis(1_600);
}

/// Fade an element in on the shared entrance curve.
///
/// Every arrival in the app goes through here, so how the app feels is one
/// edit rather than seven, and no call site can quietly invent its own timing.
pub(super) fn appear<E>(
    id: impl Into<SharedString>,
    duration: Duration,
    element: E,
) -> gpui::AnimationElement<E>
where
    E: IntoElement + Styled + 'static,
{
    element.with_animation(
        id.into(),
        Animation::new(duration).with_easing(gpui::ease_out_quint()),
        |element, delta| element.opacity(delta),
    )
}

/// Small square affordance on a toast: collapse, expand, or dismiss.
pub(super) fn toast_toggle(id: &'static str, glyph: &'static str) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .flex()
        .flex_shrink_0()
        .items_center()
        .justify_center()
        .w(px(20.0))
        .h(px(20.0))
        .rounded_md()
        .text_xs()
        .text_color(rgb(MUTED))
        .cursor_pointer()
        .hover(|style| style.bg(rgb(SURFACE_HOVER)).text_color(rgb(TEXT)))
        .child(glyph)
}

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
        Some(image) => appear(
            "avatar-in",
            motion::ENTER,
            gpui::img(image)
                .w(px(size - 2.0))
                .h(px(size - 2.0))
                .rounded_full()
                .overflow_hidden()
                .object_fit(gpui::ObjectFit::Cover),
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

/// A secondary action: Sign out, Check now, Open folder, Back.
///
/// Bordered rather than bare text. As plain FAINT text it was styled
/// identically to the caption beside it, so on the Diagnostics row "Logs from
/// your recent sessions" and "Open folder" were indistinguishable until hover -
/// one inert, one the only control in the row.
pub(super) fn quiet(id: &'static str, text: impl Into<SharedString>) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        // Without this a long description in the same row squeezes the action
        // until its text is clipped, which is how "Open folder" became
        // "Open folde".
        .flex_shrink_0()
        .px_2()
        .py_1()
        .rounded_md()
        .border_1()
        .border_color(rgb(BORDER))
        .text_xs()
        .text_color(rgb(MUTED))
        .cursor_pointer()
        .hover(|s| {
            s.bg(rgb(SURFACE_HOVER))
                .border_color(rgb(ORANGE_DIM))
                .text_color(rgb(TEXT))
        })
        .child(text.into())
}

/// A settings row: a title with supporting text, and an action on the right.
///
/// Shared rather than hand-assembled per row, because hand-assembling is how
/// the Updates and Diagnostics rows drifted apart: one had `min_w(0)` on its
/// text column and the other did not, so only one of them clipped.
pub(super) fn setting_row(
    title: impl Into<SharedString>,
    detail: impl Into<SharedString>,
    action: gpui::AnyElement,
) -> gpui::Div {
    card()
        .flex_shrink_0()
        .flex_row()
        .items_center()
        .justify_between()
        .gap_3()
        .child(
            div()
                .flex()
                .flex_col()
                .gap_0p5()
                // Lets the column shrink below its content, so the description
                // wraps instead of pushing the action off the card.
                .min_w(px(0.0))
                .child(label(title, TEXT))
                .child(label(detail, MUTED).text_xs()),
        )
        .child(action)
}

/// A settings row whose control is a strip of pills below the title.
///
/// The title row carries an optional readout - what the abstract label actually
/// resolves to - in the machine voice, matching the streaming header. Derived
/// numbers belong there rather than welded into the prose below, where changing
/// resolution could push the bitrate string past a wrap point and so change the
/// height of a card two rows further down.
///
/// The detail line describes the *selected* option rather than the setting as a
/// whole. A single line covering every option has to be written vaguely enough
/// to fit them all, which is how "Auto follows the refresh rate" ended up
/// reading as a claim about the whole setting instead of about Auto.
pub(super) fn setting_choice(
    title: impl Into<SharedString>,
    readout: Option<SharedString>,
    pills: Vec<gpui::AnyElement>,
    detail: impl Into<SharedString>,
) -> gpui::Div {
    let title = title.into();
    let detail = detail.into();
    // The animation id carries the text, so picking a different option replays
    // the fade instead of swapping the line instantly. Prefixed with the title
    // because two cards could otherwise share a detail string and an id.
    let key = SharedString::from(format!("{title}/{detail}"));

    let heading = match readout {
        Some(readout) => div()
            .flex()
            .items_center()
            .gap_2()
            .child(label(title.clone(), TEXT))
            .child(div().flex_1().h(px(1.0)).bg(rgb(BORDER)))
            .child(appear(
                SharedString::from(format!("{title}={readout}")),
                motion::QUICK,
                micro(readout, MUTED),
            )),
        None => div().flex().child(label(title, TEXT)),
    };

    card()
        .flex_shrink_0()
        .gap_2()
        .child(heading)
        .child(div().flex().flex_wrap().gap_1p5().children(pills))
        .child(
            // One line of reserved height, so a longer string in some future
            // state cannot reflow the list while a fade is still running.
            div()
                .min_h(px(16.0))
                .child(appear(key, motion::QUICK, label(detail, MUTED).text_xs())),
        )
}

/// The contents of an expanded section.
///
/// Each card fades on its own, all starting together but taking progressively
/// longer, so they arrive in order. That is a stagger without a delay
/// primitive, which GPUI does not have. Movement is deliberately not animated:
/// every offset GPUI can express is a margin or padding, so a slide would
/// reflow the scroll container on every frame.
pub(super) fn section_body(id: &str, children: Vec<gpui::AnyElement>) -> impl IntoElement {
    let animated: Vec<gpui::AnyElement> = children
        .into_iter()
        .enumerate()
        .map(|(index, child)| {
            appear(
                format!("{id}-{index}"),
                motion::ENTER + motion::STAGGER * index as u32,
                div().flex_shrink_0().child(child),
            )
            .into_any_element()
        })
        .collect();

    div()
        .flex()
        .flex_col()
        .flex_shrink_0()
        .gap_3()
        .children(animated)
}

/// A collapsible section heading: a scanline, the title, a rule, an optional
/// readout, and a chevron showing state.
///
/// The marker is a bar rather than a glyph. Nothing that sits in the same row
/// as its own text label can disambiguate anything, and the three glyphs this
/// replaces were not even a family - a concentric ring, nested squares and a
/// striped square, one row apart. A 12x2 bar is a scanline, which is the atom
/// the mark itself is built from, identical across every section so it makes no
/// claim it cannot keep, and it cannot fall back to tofu.
///
/// Typography sits on the row itself rather than on each child, so the whole
/// heading brightens as one on hover; `micro` would pin each child's colour and
/// defeat that.
pub(super) fn section_header(
    id: &'static str,
    title: &'static str,
    readout: Option<&'static str>,
    open: bool,
) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .flex()
        .flex_shrink_0()
        .items_center()
        .gap_2()
        // 26px of target instead of 18: this row exists to be clicked.
        .py_2()
        .cursor_pointer()
        .font_family("Cascadia Mono")
        .text_size(px(10.0))
        .font_weight(FontWeight::MEDIUM)
        // Full ORANGE at rest. A section heading is the primary structure of
        // the screen, not chrome; ORANGE_DIM on this background is 2.4:1, which
        // made the top of the hierarchy the second-dimmest thing on it.
        .text_color(rgb(ORANGE))
        .hover(|style| style.text_color(rgb(0xff6f38)))
        .child(accent_rule(12.0))
        .child(div().child(title))
        // The same rule the streaming header uses between its label and its
        // readout, so headings across the app read as one pattern. It also
        // pushes the tail to the far edge and makes the whole strip a target.
        .child(div().flex_1().h(px(1.0)).bg(rgb(BORDER)))
        .children(readout.map(|text| div().text_color(rgb(MUTED)).child(text)))
        // Bigger than the label, because this is the only glyph in the row that
        // encodes anything: whether the section is open.
        .child(
            div()
                .text_size(px(12.0))
                .child(if open { "\u{25BE}" } else { "\u{25B8}" }),
        )
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
        // GPUI ships this curve: a sine breath that eases at both ends. The
        // hand-rolled triangle wave it replaces snapped at the turn.
        Animation::new(motion::BREATH)
            .repeat()
            .with_easing(gpui::pulsating_between(0.35, 1.0)),
        |el, delta| el.opacity(delta),
    )
}

/// Fade content in. Keyed per screen so navigation reads as a transition
/// rather than an instant swap.
pub(super) fn fade_in(id: impl Into<SharedString>, element: gpui::AnyElement) -> impl IntoElement {
    appear(
        id,
        motion::ENTER,
        div()
            .flex()
            .flex_col()
            .flex_1()
            .min_h(px(0.0))
            .child(element),
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
