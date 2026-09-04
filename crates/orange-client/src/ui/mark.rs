//! The mark: the scanline eclipse crescent, and the states it can be in.
//!
//! ## Why this is a still image and not an animated one
//!
//! This module used to bake the mark's motion into a 44-frame image sheet:
//! rays swept out of the scanlines, and a halo built from a blur of the mark's
//! own alpha breathed underneath. It looked good and it cost **14.5% of a CPU
//! core, permanently**, any time the window was open.
//!
//! Measured on this machine, release build, window open on Home:
//!
//! | | CPU, one core |
//! |---|---|
//! | nothing animating | 0.62% |
//! | grid drifting, mark still | 0.78% |
//! | grid still, mark as an image sheet | 15.14% |
//!
//! A div-based animation is essentially free because GPUI re-renders the tree
//! anyway; an animated `RenderImage` is not, because every frame is a distinct
//! sprite-atlas tile and cycling them re-uploads roughly 3 MB/s to the GPU for
//! as long as the window is visible.
//!
//! So the motion moved out of the pixels and into the scene graph: one still
//! frame, uploaded once, with a GPU-rendered glow breathing behind it. Same
//! intent, two orders of magnitude cheaper.

use super::theme::{motion, MARK_BRAND, MARK_HERO, MARK_TITLEBAR, ORANGE};
use gpui::{div, prelude::*, px, rgb, Animation, AnimationExt, SharedString};
use std::time::Duration;

/// What the mark is doing.
///
/// The boards give the logo five states. Three are here, because three are
/// things this app can actually be. Hover and active are interaction states,
/// and the mark is not a control on any screen - the titlebar block it sits in
/// is a drag region, and the hero instance is decoration. Animating either on
/// hover would promise a click that never lands.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum LogoState {
    /// The app is open and nothing in particular is happening. Still breathing:
    /// at hero size the mark is the largest thing on the screen, and a frozen
    /// logo above a drifting grid reads as an image that failed to load.
    Idle,
    /// Waiting on something outside the app, like a browser finishing a login.
    Loading,
    /// A stream is going out right now.
    Live,
}

/// The glow behind the mark: how bright, and how slowly it breathes.
struct Aura {
    /// Peak opacity of the halo.
    strength: f32,
    /// One full breath.
    period: Duration,
    /// Diameter of the outermost ring, as a multiple of the mark's box.
    spread: f32,
}

impl LogoState {
    fn aura(self) -> Aura {
        match self {
            // Slow and dim. Present enough that the mark is clearly lit rather
            // than painted, faint enough to ignore while reading the cards.
            LogoState::Idle => Aura {
                strength: 0.30,
                period: motion::BREATH * 3,
                spread: 1.35,
            },
            // Quick and shallow: the visual equivalent of a spinner, without
            // anything actually spinning.
            LogoState::Loading => Aura {
                strength: 0.42,
                period: motion::BREATH,
                spread: 1.15,
            },
            // Bright, and on the same slow rhythm as the live dot in the
            // status line, so the two read as one heartbeat.
            LogoState::Live => Aura {
                strength: 0.72,
                period: motion::BREATH,
                spread: 1.6,
            },
        }
    }
}

/// Below this the mark is drawn bare. A halo around an eighteen-pixel logo in
/// the titlebar is a smudge, and it would put a permanent repaint behind every
/// screen in the app rather than just the one with a hero on it.
const HERO: f32 = 64.0;

// The titlebar mark must land below the threshold, or the whole app animates.
const _: () = assert!(HERO > 44.0);

// Every mark that carries a state has to be big enough to show one.
//
// Below `HERO` the aura is dropped, so a state-carrying call site under the
// threshold renders correctly and animates nothing - no warning, no failure,
// just a "waiting for Discord" mark sitting perfectly still. That shipped once
// at 60px against a 64px threshold. Checked at compile time rather than in a
// test, because there is no version of this that should ever build.
const _: () = assert!(MARK_BRAND >= HERO);
const _: () = assert!(MARK_HERO >= HERO);
const _: () = assert!(MARK_TITLEBAR < HERO);

/// The mark itself: one frame, decoded once, uploaded to the sprite atlas
/// once, and reused by every instance at every size.
fn mark() -> Option<std::sync::Arc<gpui::RenderImage>> {
    static MARK: std::sync::OnceLock<Option<std::sync::Arc<gpui::RenderImage>>> =
        std::sync::OnceLock::new();
    MARK.get_or_init(|| {
        let decoded = image::load_from_memory(include_bytes!("../../../../assets/logo.png"))
            .ok()?
            .into_rgba8();
        let (width, height) = decoded.dimensions();
        let mut raw = decoded.into_raw();
        // GPUI wants BGRA; the PNG decodes as RGBA.
        for pixel in raw.as_chunks_mut::<4>().0 {
            pixel.swap(0, 2);
        }
        let buffer = image::RgbaImage::from_raw(width, height, raw)?;
        Some(std::sync::Arc::new(gpui::RenderImage::new(vec![
            image::Frame::new(buffer),
        ])))
    })
    .clone()
}

/// The halo, as a stack of translucent discs.
///
/// Not a box shadow. GPUI has one, it blurs on the GPU, and it is the single
/// most expensive thing this app can draw: one shadow at a 40px blur radius
/// measured at **13.5% of a core**, per frame, whether or not it was moving.
/// Six overlapping quads cost nothing measurable and, at these opacities,
/// stack into a falloff that is indistinguishable from a blur at arm's length.
///
/// Largest and dimmest first, so the accumulated alpha rises toward the middle.
fn aura(state: LogoState, px_size: f32, epoch: u64, moving: bool) -> gpui::AnyElement {
    const STEPS: usize = 6;
    let Aura {
        strength,
        period,
        spread,
    } = state.aura();

    let mut stack = div()
        .absolute()
        .inset_0()
        .flex()
        .items_center()
        .justify_center();
    for step in 0..STEPS {
        // 1.0 at the core, `spread` at the outermost ring.
        let t = step as f32 / (STEPS - 1) as f32;
        let diameter = px_size * (0.52 + (spread - 0.52) * (1.0 - t));
        stack = stack.child(
            div()
                .absolute()
                .w(px(diameter))
                .h(px(diameter))
                .rounded_full()
                .bg(rgb(ORANGE))
                // Each ring is faint; six of them overlapping are not.
                .opacity(0.05 + 0.03 * (1.0 - t)),
        );
    }

    if moving {
        stack
            .with_animation(
                SharedString::from(format!("aura-{}-{epoch}", state as u8)),
                // The same curve the live dot uses, so everything in the app
                // that breathes breathes together.
                Animation::new(period)
                    .repeat()
                    .with_easing(gpui::pulsating_between(0.35, 1.0)),
                move |element, delta| element.opacity(delta * strength),
            )
            .into_any_element()
    } else {
        // Parked at the middle of the breath rather than at either end, so
        // regaining focus resumes rather than jumps.
        stack.opacity(strength * 0.7).into_any_element()
    }
}

pub(crate) fn logo(px_size: f32, state: LogoState, epoch: u64, moving: bool) -> impl IntoElement {
    let image = match mark() {
        Some(image) => gpui::img(image)
            .id(SharedString::from("logo"))
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
    };

    div()
        .relative()
        .w(px(px_size))
        .h(px(px_size))
        .flex_shrink_0()
        // The halo first, so the mark paints over it.
        .children((px_size >= HERO).then(|| aura(state, px_size, epoch, moving)))
        .child(image)
}

#[cfg(test)]
mod tests {
    use super::*;

    const EVERY: [LogoState; 3] = [LogoState::Idle, LogoState::Loading, LogoState::Live];

    /// Live has to out-glow idle, or going live changes nothing anyone sees.
    #[test]
    fn the_glow_separates_live_from_idle() {
        let idle = LogoState::Idle.aura();
        let live = LogoState::Live.aura();
        assert!(
            live.strength > idle.strength,
            "live is no brighter than idle"
        );
        assert!(
            live.spread > idle.spread,
            "live spreads no further than idle"
        );
        assert!(
            live.period < idle.period,
            "live breathes no faster than idle"
        );
    }

    /// Every state must actually move. A strength of zero or a period of zero
    /// is a state that renders as a still image with extra steps.
    #[test]
    fn every_state_breathes() {
        for state in EVERY {
            let aura = state.aura();
            assert!(aura.strength > 0.0, "a state with no glow");
            assert!(aura.period > Duration::ZERO, "a glow with no breath");
            assert!(
                aura.spread > 1.0,
                "a glow that does not reach past the mark"
            );
        }
    }

    /// The mark is compiled in, so decoding it cannot fail at runtime - and it
    /// must stay a single frame. An animated sheet here is what cost 14.5% of
    /// a core, and nothing about the call sites would reveal the regression.
    #[test]
    fn the_mark_is_one_frame() {
        let image = mark().expect("the mark is compiled in, so it always decodes");
        assert_eq!(
            image.frame_count(),
            1,
            "the mark became an animated sheet again; see this module's header \
             for what that costs"
        );
    }
}
