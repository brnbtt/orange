use anyhow::Context;
use gst::prelude::{ObjectExt, ToValue};
use gstreamer as gst;
use gstreamer_video as gst_video;
use std::panic::{catch_unwind, AssertUnwindSafe};

use super::raster::{render, transparent_composition};
use super::SharedOverlay;

fn display_size(video: (u32, u32), par: gst::Fraction) -> Option<(u32, u32)> {
    let (w, h) = video;
    if w == 0 || h == 0 || par.numer() <= 0 || par.denom() <= 0 {
        return None;
    }
    let ratio = gst_video::calculate_display_ratio(w, h, par, gst::Fraction::new(1, 1))?;
    let (num, den) = (
        u32::try_from(ratio.numer()).ok()?,
        u32::try_from(ratio.denom()).ok()?,
    );
    if num == 0 || den == 0 {
        return None;
    }
    // Match d3d11videosink's display-size choice exactly: its compositor uses
    // render_info, not the decoded dimensions. Merely correcting the window's
    // aspect leaves right-hand buttons shifted left on anamorphic streams.
    let size = if h % den != 0 && w % num == 0 {
        (w as u64, w as u64 * den as u64 / num as u64)
    } else {
        (h as u64 * num as u64 / den as u64, h as u64)
    };
    if size.0 == 0 || size.1 == 0 || size.0 > i32::MAX as u64 || size.1 > i32::MAX as u64 {
        return None;
    }
    Some((size.0 as u32, size.1 as u32))
}

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
    playback: &crate::window::PlaybackWindowHandle,
) -> anyhow::Result<()> {
    let fallback = transparent_composition().context("failed to build overlay fallback")?;

    // Keep encoded dimensions for telemetry, but use the sink's PAR-corrected
    // display dimensions for layout, window aspect and all hit testing.
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
                    let previous = state.display_size();
                    state.video = (w.max(0) as u32, h.max(0) as u32);
                    let par = s
                        .get::<gst::Fraction>("pixel-aspect-ratio")
                        .unwrap_or_else(|_| gst::Fraction::new(1, 1));
                    state.display = display_size(state.video, par);
                    if fps.is_some() {
                        state.fps = fps;
                    }
                    // Show the controls once, when caps first provide a source
                    // size. A viewer who never moves the mouse would otherwise
                    // have no way to learn they exist.
                    if state.display_size() != (0, 0) && state.display_size() != previous {
                        if previous == (0, 0) {
                            state.wake();
                        }
                        source_changed = Some(state.display_size());
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
    fn display_geometry_matches_d3d11_for_wide_tall_and_fractional_pixels() {
        // Matching only the aspect ratio is insufficient: the compositor also
        // uses the sink's integer choice of which source dimension to retain.
        for (video, par, expected) in [
            ((1920, 1080), (1, 1), Some((1920, 1080))),
            ((1920, 1080), (4, 3), Some((2560, 1080))),
            ((1920, 1080), (3, 4), Some((1440, 1080))),
            ((720, 577), (577, 540), Some((720, 540))),
            ((721, 577), (16, 15), Some((769, 577))),
            ((0, 1080), (1, 1), None),
            ((1920, 0), (1, 1), None),
            ((1920, 1080), (0, 1), None),
            ((1920, 1080), (-1, 1), None),
        ] {
            assert_eq!(
                display_size(video, par.into()),
                expected,
                "{video:?}, {par:?}"
            );
        }
    }

    #[test]
    fn pixel_aspect_changes_reposition_controls_without_changing_quality_labels() {
        // GPU scaling can preserve an ultrawide source as 1920x1080 PAR 4/3.
        // D3D11 composites in 2560x1080 display space, not encoded pixel space.
        gst::init().unwrap();
        let owner =
            crate::window::PlaybackWindow::spawn_preview("overlay caps test", 1280, 720).unwrap();
        let playback = owner.handle();
        let element = gst::ElementFactory::make("overlaycomposition")
            .build()
            .unwrap();
        attach(&element, &playback).unwrap();
        for (par, display) in [
            ((1, 1), (1920, 1080)),
            ((4, 3), (2560, 1080)),
            ((1, 1), (1920, 1080)),
        ] {
            let caps = gst_video::VideoInfo::builder(gst_video::VideoFormat::Bgra, 1920, 1080)
                .par(par)
                .fps((60, 1))
                .build()
                .unwrap()
                .to_caps()
                .unwrap();
            element.emit_by_name::<()>("caps-changed", &[&caps, &0u32, &0u32]);
            let mut state = playback.overlay().lock().unwrap();
            state.client = (1280, 720);
            state.dpi = 1.0;
            state.pinned = true;
            let composition = render(&mut state).unwrap();
            let (x, y, w, h) = composition.rectangle(1).unwrap().render_rectangle();
            // Independent screen projection: the close panel must be 18px
            // from the picture's right/top edges and remain 40px square.
            let fit = (1280.0 / display.0 as f32).min(720.0 / display.1 as f32);
            assert!(((display.0 as f32 - (x as f32 + w as f32)) * fit - 18.0).abs() <= 1.0);
            assert!((y as f32 * fit - 18.0).abs() <= 1.0);
            assert!((w as f32 * fit - 40.0).abs() <= 1.0);
            assert!((h as f32 * fit - 40.0).abs() <= 1.0);
            assert_eq!(state.quality_label(None), "1080p60");
        }
    }

    #[test]
    #[ignore = "requires a visible Windows desktop and D3D11 hardware"]
    fn displayed_close_button_matches_native_hit_testing_for_non_square_pixels() {
        use gst::prelude::*;
        use gst_video::prelude::*;
        use std::time::{Duration, Instant};
        use windows::Win32::Foundation::{HWND, LPARAM, POINT, RECT, WPARAM};
        use windows::Win32::Graphics::Gdi::{ClientToScreen, GetDC, GetPixel, ReleaseDC};
        use windows::Win32::UI::WindowsAndMessaging::{
            GetClientRect, SendMessageTimeoutW, SetWindowPos, HWND_TOPMOST, SMTO_ABORTIFHUNG,
            SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, WM_NCHITTEST,
        };

        // A white picture makes an offset dark panel distinguishable from its
        // intended hit target. Read actual window pixels, not overlay metadata.
        gst::init().unwrap();
        crate::window::set_dpi_aware();
        let owner =
            crate::window::PlaybackWindow::spawn_preview("overlay pixel acceptance", 960, 540)
                .unwrap();
        let playback = owner.handle();
        let pipeline = gst::parse::launch(
            "videotestsrc is-live=true pattern=white ! video/x-raw,format=BGRA,width=1920,height=1080,pixel-aspect-ratio=4/3 \
             ! d3d11upload ! overlaycomposition name=controls ! d3d11videosink name=screen sync=false",
        ).unwrap().downcast::<gst::Pipeline>().unwrap();
        struct Stop(gst::Pipeline);
        impl Drop for Stop {
            fn drop(&mut self) {
                let _ = self.0.set_state(gst::State::Null);
            }
        }
        let _stop = Stop(pipeline.clone());
        attach(&pipeline.by_name("controls").unwrap(), &playback).unwrap();
        playback.overlay().lock().unwrap().pinned = true;
        let sink = pipeline.by_name("screen").unwrap();
        let hwnd = HWND(playback.hwnd().unwrap() as *mut _);
        // SAFETY: the owner outlives the pipeline and its Null-state guard.
        unsafe {
            sink.dynamic_cast_ref::<gst_video::VideoOverlay>()
                .unwrap()
                .set_window_handle(hwnd.0 as usize)
        };
        playback.reveal();
        // SAFETY: keep only this disposable window above other desktop apps
        // so screen capture observes the video rather than an occluding window.
        unsafe {
            SetWindowPos(
                hwnd,
                Some(HWND_TOPMOST),
                0,
                0,
                0,
                0,
                SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
            )
        }
        .unwrap();
        pipeline.set_state(gst::State::Playing).unwrap();

        for (dpi, size) in [
            (1.0_f32, None),
            (1.25, Some((1000, 700))),
            (1.5, Some((1200, 400))),
            (2.0, Some((960, 540))),
        ] {
            if let Some((w, h)) = size {
                // SAFETY: resize the owned test window to exercise both
                // letterboxing and pillarboxing with physical client sizes.
                unsafe {
                    SetWindowPos(
                        hwnd,
                        None,
                        0,
                        0,
                        w,
                        h,
                        SWP_NOMOVE
                            | SWP_NOACTIVATE
                            | windows::Win32::UI::WindowsAndMessaging::SWP_NOZORDER,
                    )
                }
                .unwrap();
            }
            playback.overlay().lock().unwrap().dpi = dpi;
            let deadline = Instant::now() + Duration::from_secs(3);
            loop {
                let mut client = RECT::default();
                // SAFETY: HWND is live and all output pointers are local values.
                unsafe { GetClientRect(hwnd, &mut client) }.unwrap();
                let cw = client.right as f32;
                let ch = client.bottom as f32;
                let fit = (cw / 2560.0).min(ch / 1080.0);
                let right = (cw + 2560.0 * fit) / 2.0;
                let top = (ch - 1080.0 * fit) / 2.0;
                let mut point = POINT {
                    x: (right - 38.0 * dpi) as i32,
                    y: (top + 38.0 * dpi) as i32,
                };
                let sample_x = (right - 52.0 * dpi) as i32;
                let sample_y = point.y;
                let mut sample = POINT {
                    x: sample_x,
                    y: sample_y,
                };
                // SAFETY: simulate the native hit-test message with a client
                // point converted to the screen coordinates Win32 requires.
                let observed = unsafe {
                    ClientToScreen(hwnd, &mut point).ok().unwrap();
                    SendMessageTimeoutW(
                        hwnd,
                        WM_NCHITTEST,
                        WPARAM(0),
                        LPARAM(
                            (((point.y as u32 & 0xffff) << 16) | (point.x as u32 & 0xffff))
                                as isize,
                        ),
                        SMTO_ABORTIFHUNG,
                        1000,
                        None,
                    );
                    ClientToScreen(hwnd, &mut sample).ok().unwrap();
                    let dc = GetDC(None);
                    let pixel = GetPixel(dc, sample.x, sample.y).0;
                    ReleaseDC(None, dc);
                    (pixel & 255, (pixel >> 8) & 255, (pixel >> 16) & 255)
                };
                let hot = playback.overlay().lock().unwrap().hot;
                let (r, g, b) = observed;
                if hot == Some(crate::overlay::Control::Close) && r > g + 20 && r < 150 && b < 80 {
                    eprintln!("DPI {dpi}: visible hover panel RGB={observed:?} at ({sample_x}, {sample_y})");
                    break;
                }
                assert!(Instant::now() < deadline,
                    "DPI {dpi}: expected a red hover panel at ({sample_x}, {sample_y}), got {observed:?}, hot={hot:?}");
                std::thread::sleep(Duration::from_millis(20));
            }
        }
    }

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
