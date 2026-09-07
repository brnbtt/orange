//! The ambient layer: the things behind the content rather than in it.
//!
//! Everything here is decoration and none of it is interactive, so it all
//! renders underneath and never takes a hit test. The grid stays still; only
//! the soft lighting changes, so the backdrop never competes with the list.

use super::theme::*;
use gpui::{div, prelude::*, px, rgb, Animation, AnimationExt, SharedString};

const CELL: f32 = 64.0;

/// A hairline. Width or height of one, depending which way it runs.
const HAIR: f32 = 1.0;

/// A wide, stationary grid fading into ink, with slow light from the corner.
/// Only opacity animates: no moving hairlines or blurred shadows on each frame.
/// `moving` remains false on inactive windows to avoid full-window repaints.
pub(crate) fn grid(moving: bool) -> impl IntoElement {
    let mut field = div().absolute().inset_0();

    let mut x = CELL / 2.0;
    while x < WINDOW_WIDTH {
        field = field.child(
            div()
                .absolute()
                .left(px(x))
                .top(px(0.0))
                .w(px(HAIR))
                .h_full()
                .bg(rgb(GRID)),
        );
        x += CELL;
    }
    let mut y = CELL / 2.0;
    while y < WINDOW_HEIGHT {
        field = field.child(
            div()
                .absolute()
                .top(px(y))
                .left(px(0.0))
                .w_full()
                .h(px(HAIR))
                .bg(rgb(GRID)),
        );
        y += CELL;
    }

    let ink: gpui::Hsla = rgb(BG).into();
    let warm: gpui::Hsla = rgb(ORANGE).into();
    let light = div().absolute().inset_0().bg(gpui::linear_gradient(
        135.0,
        gpui::linear_color_stop(warm.opacity(0.065), 0.0),
        gpui::linear_color_stop(warm.opacity(0.0), 0.8),
    ));
    div()
        .absolute()
        .inset_0()
        .overflow_hidden()
        .child(field.opacity(0.45))
        // The old full-contrast grid continued through every gap and the
        // account footer. Fade it away so content owns the centre of the app.
        .child(div().absolute().inset_0().bg(gpui::linear_gradient(
            180.0,
            gpui::linear_color_stop(ink.opacity(0.0), 0.0),
            gpui::linear_color_stop(ink, 0.85),
        )))
        .child(if moving {
            light
                .with_animation(
                    SharedString::from("ambient-light"),
                    Animation::new(motion::AMBIENT)
                        .repeat()
                        .with_easing(gpui::pulsating_between(0.45, 0.85)),
                    |element, delta| element.opacity(delta),
                )
                .into_any_element()
        } else {
            light.opacity(0.65).into_any_element()
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
