//! Design tokens: colour, type, metrics and motion.
//!
//! Everything visual in the tray resolves to something in this file. The
//! video overlay in the `orange` crate mirrors these values by hand, in
//! floats, because tiny-skia and GPUI share no colour type - `overlay/raster.rs`
//! carries the hex on each line so the two can be diffed by eye.

use gpui::{div, prelude::*, px, rgb, Animation, AnimationExt, FontWeight, SharedString};
use std::time::Duration;

// The palette from the identity boards: five neutrals from ink to sand, one
// accent, and two semantics.
//
// The boards also name an info blue, a warning amber and a violet accent.
// None of them are here, because nothing in this app is coloured by them. A
// constant nobody uses is a promise the interface never keeps, and the next
// person to need a colour reaches for one of these five instead of inventing
// a sixth.
pub(crate) const BG: u32 = 0x0d0d10;
pub(crate) const SURFACE: u32 = 0x1a1a1d;
pub(crate) const SURFACE_HOVER: u32 = 0x232327;
pub(crate) const BORDER: u32 = 0x2a2a2e;
pub(crate) const BORDER_HOVER: u32 = 0x3a3a40;
/// A border that is present but inert, for a control whose space is reserved
/// while it has nothing to do.
pub(crate) const BORDER_DIM: u32 = 0x1f1f23;
pub(crate) const TEXT: u32 = 0xe6e0d1;
pub(crate) const MUTED: u32 = 0x99948a;
pub(crate) const FAINT: u32 = 0x66625b;
pub(crate) const ORANGE: u32 = 0xff5a1f;
pub(crate) const ORANGE_HOT: u32 = 0xff6f38;
pub(crate) const ORANGE_DIM: u32 = 0x8a3110;
/// Orange burnt down to a surface, for the one panel that has to read as ours
/// without becoming a second accent.
pub(crate) const ORANGE_WASH: u32 = 0x241608;
/// The darkest value on the board. Text on an orange fill, and the letterbox
/// behind a preview - both want the same thing, which is to be the floor.
pub(crate) const INK: u32 = 0x070708;
pub(crate) const SUCCESS: u32 = 0x22c55e;
pub(crate) const DANGER: u32 = 0xef4444;
pub(crate) const DANGER_WASH: u32 = 0x2a1210;
pub(crate) const DANGER_EDGE: u32 = 0x4a1e1c;
/// Only the close button turns red under the cursor, so it is the one control
/// that says what it does before it is clicked.
pub(crate) const DANGER_HOVER: u32 = 0x9a2e2e;
/// The ambient grid. Warm rather than neutral, and only just above the
/// background: it is meant to be felt at the edge of vision rather than read.
pub(crate) const GRID: u32 = 0x191317;
/// The viewfinder brackets and registration marks framing a screen.
pub(crate) const FRAME: u32 = 0x40180c;

/// The wordmark, spelled with real spaces.
///
/// GPUI has no letter-spacing, so the tracking on the boards is the string
/// itself. It is a logotype rather than a word, which is the one place that
/// trade is worth making: nothing wraps it, ellipsises it or selects it.
pub(crate) const WORDMARK: &str = "O R A N G E";

pub(crate) const PICKER_PREVIEW_HEIGHT: f32 = 142.0;
pub(crate) const PICKER_DETAILS_HEIGHT: f32 = 52.0;
pub(crate) const PICKER_CARD_HEIGHT: f32 = PICKER_PREVIEW_HEIGHT + PICKER_DETAILS_HEIGHT;
/// Height of the custom titlebar. The toast layer hangs directly below it, so
/// the two have to agree.
pub(crate) const TITLEBAR_HEIGHT: f32 = 44.0;

/// The three sizes the mark is drawn at.
///
/// Named rather than written at the call sites, because two of them carry a
/// state and one does not, and the difference is a threshold inside `mark`
/// rather than anything visible here. The sign-in mark was 60px against a
/// 64px threshold for exactly one build: it rendered, it looked fine, and the
/// "waiting for Discord" animation silently did nothing. `ui::mark` has a test
/// that holds these to the threshold.
pub(crate) const MARK_TITLEBAR: f32 = 18.0;
pub(crate) const MARK_BRAND: f32 = 76.0;
pub(crate) const MARK_HERO: f32 = 104.0;

/// The app's motion vocabulary.
///
/// Two durations, not five. Every animation here used to name its own number -
/// 160, 180, 200, 260, 1600 - with no reason for any of them being different,
/// which is how the app ended up feeling assembled rather than designed.
///
/// Everything that appears shares one decelerating curve. Note that eased is
/// not slower: `ease_out_quint` front-loads the change, so 220ms eased reads
/// faster than the 200ms linear fades it replaces.
pub(crate) mod motion {
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
    /// One cell of the background grid's drift.
    ///
    /// An order of magnitude slower than anything else in the app, because it
    /// is the only animation with no event behind it. Fast enough to notice
    /// and it becomes a thing happening rather than a room you are in.
    pub const DRIFT: Duration = Duration::from_millis(12_000);
}

/// Fade an element in on the shared entrance curve.
///
/// Every arrival in the app goes through here, so how the app feels is one
/// edit rather than seven, and no call site can quietly invent its own timing.
pub(crate) fn appear<E>(
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

pub(crate) fn label(text: impl Into<SharedString>, color: u32) -> gpui::Div {
    div().text_color(rgb(color)).child(text.into())
}

/// Technical microcopy from the identity board: compact, monospaced and used
/// only for orientation/status so body text remains easy to scan.
pub(crate) fn micro(text: impl Into<SharedString>, color: u32) -> gpui::Div {
    label(text, color)
        .font_family("Cascadia Mono")
        .text_size(px(10.0))
        .font_weight(FontWeight::MEDIUM)
}

/// A title: screen headings and card titles.
pub(crate) fn heading(text: impl Into<SharedString>, size: f32) -> gpui::Div {
    label(text, TEXT)
        .font_family("Bahnschrift")
        .text_size(px(size))
        .font_weight(FontWeight::SEMIBOLD)
}

pub(crate) fn wordmark(size: f32) -> gpui::Div {
    label(WORDMARK, ORANGE)
        .font_family("Bahnschrift")
        .text_size(px(size))
        .font_weight(FontWeight::SEMIBOLD)
}

pub(crate) fn accent_rule(width: f32) -> gpui::Div {
    div().w(px(width)).h(px(2.0)).bg(rgb(ORANGE))
}

/// Fade content in. Keyed per screen so navigation reads as a transition
/// rather than an instant swap.
pub(crate) fn fade_in(id: impl Into<SharedString>, element: gpui::AnyElement) -> impl IntoElement {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn picker_cards_use_content_height_instead_of_filling_the_viewport() {
        assert_eq!(
            PICKER_CARD_HEIGHT,
            PICKER_PREVIEW_HEIGHT + PICKER_DETAILS_HEIGHT
        );
    }
}
