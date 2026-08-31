use anyhow::Context;
use gst::prelude::{ObjectExt, ToValue};
use gstreamer as gst;
use gstreamer_video as gst_video;
use std::panic::{catch_unwind, AssertUnwindSafe};

use super::raster::{render, transparent_composition};
use super::SharedOverlay;

fn draw_composition(
    state: &SharedOverlay,
    fallback: &gst_video::VideoOverlayComposition,
) -> gst_video::VideoOverlayComposition {
    catch_unwind(AssertUnwindSafe(|| {
        let mut state = state.lock().ok()?;
        render(&mut state)
    }))
    .ok()
    .flatten()
    .unwrap_or_else(|| fallback.clone())
}

/// Bind an `overlaycomposition` element to shared overlay state.
///
/// Both the real viewer and the design harness call this, so what you see
/// while iterating on the layout is what a viewer actually gets.
pub fn attach(
    composition: &gst::Element,
    playback: &crate::window::PlaybackWindow,
) -> anyhow::Result<()> {
    let fallback = transparent_composition().context("failed to build overlay fallback")?;

    // Learn the video size and frame rate; the former is the coordinate space
    // the overlay and all hit testing work in, and both are shown to the
    // viewer.
    let state = playback.overlay().clone();
    let playback_for_caps = playback.clone();
    composition.connect("caps-changed", false, move |values| {
        if let Ok(caps) = values[1].get::<gst::Caps>() {
            if let Some(s) = caps.structure(0) {
                let w = s.get::<i32>("width").unwrap_or(0);
                let h = s.get::<i32>("height").unwrap_or(0);
                let fps = s
                    .get::<gst::Fraction>("framerate")
                    .ok()
                    .filter(|f| f.denom() != 0)
                    .map(|f| f.numer() as f64 / f.denom() as f64);
                let mut source_changed = None;
                if let Ok(mut state) = state.lock() {
                    let previous = state.video;
                    state.video = (w.max(0) as u32, h.max(0) as u32);
                    if fps.is_some() {
                        state.fps = fps;
                    }
                    // Show the controls once, on the first frame. A viewer who
                    // never happens to move the mouse would otherwise have no
                    // way to learn they exist.
                    if state.video != (0, 0) && state.video != previous {
                        if previous == (0, 0) {
                            state.wake();
                        }
                        source_changed = Some(state.video);
                    }
                }
                if let Some((width, height)) = source_changed {
                    playback_for_caps.set_source_size(width, height);
                }
            }
        }
        None
    });

    let state = playback.overlay().clone();
    composition.connect("draw", false, move |_values| {
        let composition = draw_composition(&state, &fallback);
        Some(composition.to_value())
    });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::overlay::OverlayState;
    use std::sync::{Arc, Mutex};

    #[test]
    fn poisoned_overlay_returns_the_valid_prebuilt_fallback() {
        gst::init().unwrap();
        let fallback = transparent_composition().unwrap();
        let state = Arc::new(Mutex::new(OverlayState::new(
            crate::window::PlaybackProfile::LiveMonitor,
        )));
        let poison = state.clone();
        std::thread::spawn(move || {
            let _guard = poison.lock().unwrap();
            panic!("poison overlay state");
        })
        .join()
        .unwrap_err();

        let composition = draw_composition(&state, &fallback);

        assert_eq!(composition.seqnum(), fallback.seqnum());
        assert_eq!(composition.n_rectangles(), 1);
        assert!(composition.rectangle(0).is_ok());
    }
}
