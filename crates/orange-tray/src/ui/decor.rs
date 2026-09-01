//! The ambient layer: the things behind the content rather than in it.
//!
//! Everything here is decoration and none of it is interactive, so it all
//! renders underneath and never takes a hit test. Two rules keep it from
//! becoming noise: it is drawn from the palette's dimmest values, and there is
//! exactly one thing moving at a time.

use super::theme::*;
use gpui::{div, prelude::*, px, rgb, Animation, AnimationExt, SharedString};

/// Spacing of the grid, and the distance it travels before repeating.
const CELL: f32 = 32.0;
/// How far the field is drawn past the window on every side.
///
/// The grid drifts, so it has to be oversized or the leading edge would walk
/// into view as a bare strip. One cell of overscan is exactly enough for a
/// drift of one cell.
const OVERSCAN: f32 = CELL;
/// Generous enough for the widest screen (the picker, at 576) plus overscan.
const FIELD_W: f32 = 640.0;
const FIELD_H: f32 = 760.0;

/// A hairline. Width or height of one, depending which way it runs.
const HAIR: f32 = 1.0;

// The drift is only seamless if the field is drawn at least as far oversize as
// it travels. Get this wrong and a bare strip walks in from the edge once per
// loop, which is the kind of thing nobody notices until they cannot stop
// noticing it. Checked here rather than in a test because both sides are
// constants: a runtime assertion could only ever fail after shipping.
const _: () = assert!(OVERSCAN >= CELL);
const _: () = assert!(FIELD_W >= 576.0 + OVERSCAN, "narrower than the picker");
const _: () = assert!(FIELD_H >= 660.0, "shorter than the tallest screen");

/// The grid: a field of hairlines that drifts by exactly one cell and repeats.
///
/// Drift rather than a pulse. A pulsing grid draws the eye on every beat,
/// which is the opposite of what a background is for; a drift of one cell over
/// twelve seconds is never caught moving but is never quite still either.
///
/// Because the travel is exactly `CELL` and the field is drawn a cell oversize,
/// the loop point is seamless - the line that walks off the bottom is standing
/// where its neighbour began.
///
/// `moving` is false whenever the window is not the active one. Any running
/// animation costs a full repaint at 60fps, which on this window measures
/// around 12% of a CPU core - a price worth paying while somebody is looking
/// at it and pure waste the moment they alt-tab away.
pub(crate) fn grid(moving: bool) -> impl IntoElement {
    let mut field = div().absolute().w(px(FIELD_W)).h(px(FIELD_H));

    let mut x = 0.0;
    while x <= FIELD_W {
        field = field.child(
            div()
                .absolute()
                .left(px(x))
                .top(px(0.0))
                .w(px(HAIR))
                .h(px(FIELD_H))
                .bg(rgb(GRID)),
        );
        x += CELL;
    }
    let mut y = 0.0;
    while y <= FIELD_H {
        field = field.child(
            div()
                .absolute()
                .top(px(y))
                .left(px(0.0))
                .w(px(FIELD_W))
                .h(px(HAIR))
                .bg(rgb(GRID)),
        );
        y += CELL;
    }

    // The clip is the window; the field inside it is what moves. Animating the
    // clip instead would move the hole rather than the contents.
    let field = field.top(px(-OVERSCAN));
    div()
        .absolute()
        .inset_0()
        .overflow_hidden()
        .child(if moving {
            field
                .with_animation(
                    SharedString::from("grid-drift"),
                    Animation::new(motion::DRIFT).repeat(),
                    |element, delta| element.top(px(-OVERSCAN + delta * CELL)),
                )
                .into_any_element()
        } else {
            field.into_any_element()
        })
}

/// One corner bracket, as two bars meeting at a right angle.
///
/// `dx`/`dy` are -1 or 1 and say which corner this is, so a single arm length
/// and weight describe all four rather than four hand-placed pairs that drift
/// apart the first time the length changes.
fn bracket(dx: f32, dy: f32, arm: f32, weight: f32, color: u32) -> gpui::Div {
    let horizontal = div().absolute().w(px(arm)).h(px(weight)).bg(rgb(color));
    let vertical = div().absolute().w(px(weight)).h(px(arm)).bg(rgb(color));
    let place = |element: gpui::Div| {
        let element = if dx < 0.0 {
            element.left(px(0.0))
        } else {
            element.right(px(0.0))
        };
        if dy < 0.0 {
            element.top(px(0.0))
        } else {
            element.bottom(px(0.0))
        }
    };
    div()
        .absolute()
        .inset_0()
        .child(place(horizontal))
        .child(place(vertical))
}

/// Viewfinder brackets around the content area.
///
/// The frame the boards draw. Open corners rather than a border: a closed
/// rectangle around the whole screen reads as a panel edge and fights the
/// cards inside it, where four corners read as framing.
pub(crate) fn corner_brackets(arm: f32, color: u32) -> impl IntoElement {
    div()
        .absolute()
        .inset_0()
        .child(bracket(-1.0, -1.0, arm, 1.5, color))
        .child(bracket(1.0, -1.0, arm, 1.5, color))
        .child(bracket(-1.0, 1.0, arm, 1.5, color))
        .child(bracket(1.0, 1.0, arm, 1.5, color))
}

/// A small registration cross, for the mid-edge marks on the boards.
pub(crate) fn crosshair(size: f32, color: u32) -> gpui::Div {
    div()
        .relative()
        .w(px(size))
        .h(px(size))
        .flex_shrink_0()
        .child(
            div()
                .absolute()
                .top(px(size / 2.0 - HAIR / 2.0))
                .left(px(0.0))
                .w(px(size))
                .h(px(HAIR))
                .bg(rgb(color)),
        )
        .child(
            div()
                .absolute()
                .left(px(size / 2.0 - HAIR / 2.0))
                .top(px(0.0))
                .w(px(HAIR))
                .h(px(size))
                .bg(rgb(color)),
        )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The grid has to outlast the tallest screen in both directions or it
    /// stops short of the edge on the picker.
    #[test]
    fn the_field_covers_every_screen() {
        let widest: f32 = 576.0;
        let tallest: f32 = 660.0;
        assert!(FIELD_W - OVERSCAN >= widest);
        assert!(FIELD_H >= tallest);
    }
}
