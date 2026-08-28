//! Overlay controls drawn onto the video.
//!
//! These are composited by `d3d11videosink` on the GPU via
//! `GstVideoOverlayComposition`, rather than being a second window floating
//! above the video. That matters: a separate window has to chase the video
//! window during moves and resizes, and always lags by a frame or two. A
//! composited overlay is part of the frame and cannot drift.
//!
//! The bar is rasterised with `tiny-skia` only when something changes - hover,
//! volume, visibility - not per frame, so the cost is negligible.

use gstreamer as gst;
use gstreamer_video as gst_video;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tiny_skia::{Color, FillRule, Paint, PathBuilder, Pixmap, Rect, Transform};

/// How long the controls stay up after the last mouse movement.
const HIDE_AFTER: Duration = Duration::from_secs(3);

const BAR_HEIGHT: f32 = 64.0;
const BAR_MARGIN: f32 = 16.0;
const ICON_SIZE: f32 = 24.0;
const ORANGE_RGB: (f32, f32, f32) = (1.0, 0.478, 0.0);

/// A clickable region of the bar, in video coordinates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Control {
    Mute,
    VolumeTrack,
    Close,
}

#[derive(Debug, Clone, Copy)]
struct Hit {
    control: Control,
    x: f32,
    y: f32,
    w: f32,
    h: f32,
}

impl Hit {
    fn contains(&self, px: f32, py: f32) -> bool {
        px >= self.x && px <= self.x + self.w && py >= self.y && py <= self.y + self.h
    }
}

pub struct OverlayState {
    /// Video frame size, which is the coordinate space the overlay draws in.
    pub video: (u32, u32),
    pub volume: f64,
    pub muted: bool,
    pub viewers: Option<usize>,
    shown_at: Instant,
    hot: Option<Control>,
    hits: Vec<Hit>,
    /// Cached rasterisation, invalidated when any of the above changes.
    cache: Option<(u64, gst_video::VideoOverlayComposition)>,
    pub close_requested: bool,
}

impl Default for OverlayState {
    fn default() -> Self {
        Self {
            video: (0, 0),
            volume: 1.0,
            muted: false,
            viewers: None,
            // Start hidden; the first mouse move reveals the bar.
            shown_at: Instant::now() - HIDE_AFTER * 2,
            hot: None,
            hits: Vec::new(),
            cache: None,
            close_requested: false,
        }
    }
}

pub type SharedOverlay = Arc<Mutex<OverlayState>>;

impl OverlayState {
    fn visible(&self) -> bool {
        self.shown_at.elapsed() < HIDE_AFTER
    }

    /// Fade factor, so the bar dissolves rather than vanishing.
    fn opacity(&self) -> f32 {
        let elapsed = self.shown_at.elapsed();
        if elapsed >= HIDE_AFTER {
            return 0.0;
        }
        let fade_start = HIDE_AFTER.saturating_sub(Duration::from_millis(600));
        if elapsed < fade_start {
            1.0
        } else {
            let t = (elapsed - fade_start).as_secs_f32() / 0.6;
            (1.0 - t).clamp(0.0, 1.0)
        }
    }

    pub fn wake(&mut self) {
        self.shown_at = Instant::now();
    }

    /// Identifies a cache entry. Any change here forces a redraw.
    fn signature(&self) -> u64 {
        let opacity_step = (self.opacity() * 20.0) as u64;
        let volume_step = (self.volume * 100.0) as u64;
        let hot = match self.hot {
            None => 0,
            Some(Control::Mute) => 1,
            Some(Control::VolumeTrack) => 2,
            Some(Control::Close) => 3,
        };
        (self.video.0 as u64) << 40
            | (self.video.1 as u64) << 24
            | opacity_step << 16
            | volume_step << 8
            | hot << 2
            | (self.muted as u64)
    }

    /// Whether the cursor is currently over a control, which decides if a
    /// click should press it or drag the window.
    pub fn hovered(&self) -> bool {
        self.hot.is_some()
    }

    /// Feed a mouse position in video coordinates. Returns true if a redraw is
    /// warranted.
    pub fn on_mouse_move(&mut self, x: f32, y: f32) -> bool {
        self.wake();
        let previous = self.hot;
        self.hot = self
            .hits
            .iter()
            .find(|h| h.contains(x, y))
            .map(|h| h.control);
        previous != self.hot
    }

    /// Handle a click in video coordinates.
    pub fn on_click(&mut self, x: f32, y: f32) {
        self.wake();
        let Some(hit) = self.hits.iter().find(|h| h.contains(x, y)).copied() else {
            return;
        };
        match hit.control {
            Control::Mute => self.muted = !self.muted,
            Control::Close => self.close_requested = true,
            Control::VolumeTrack => {
                let t = ((x - hit.x) / hit.w).clamp(0.0, 1.0);
                self.volume = t as f64;
                self.muted = false;
            }
        }
        self.cache = None;
    }
}

fn rounded_rect(x: f32, y: f32, w: f32, h: f32, r: f32) -> Option<tiny_skia::Path> {
    let mut pb = PathBuilder::new();
    pb.move_to(x + r, y);
    pb.line_to(x + w - r, y);
    pb.quad_to(x + w, y, x + w, y + r);
    pb.line_to(x + w, y + h - r);
    pb.quad_to(x + w, y + h, x + w - r, y + h);
    pb.line_to(x + r, y + h);
    pb.quad_to(x, y + h, x, y + h - r);
    pb.line_to(x, y + r);
    pb.quad_to(x, y, x + r, y);
    pb.close();
    pb.finish()
}

fn fill(pixmap: &mut Pixmap, path: &tiny_skia::Path, color: Color) {
    let mut paint = Paint::default();
    paint.set_color(color);
    paint.anti_alias = true;
    pixmap.fill_path(path, &paint, FillRule::Winding, Transform::identity(), None);
}

fn fill_rect(pixmap: &mut Pixmap, x: f32, y: f32, w: f32, h: f32, color: Color) {
    if let Some(rect) = Rect::from_xywh(x, y, w, h) {
        let mut paint = Paint::default();
        paint.set_color(color);
        paint.anti_alias = true;
        pixmap.fill_rect(rect, &paint, Transform::identity(), None);
    }
}

/// A speaker glyph, drawn by hand to avoid dragging in a font or icon set.
fn draw_speaker(pixmap: &mut Pixmap, x: f32, y: f32, muted: bool, alpha: f32) {
    let color = Color::from_rgba(1.0, 1.0, 1.0, alpha).unwrap_or(Color::WHITE);
    let s = ICON_SIZE;

    // Cone.
    let mut pb = PathBuilder::new();
    pb.move_to(x + s * 0.10, y + s * 0.36);
    pb.line_to(x + s * 0.30, y + s * 0.36);
    pb.line_to(x + s * 0.52, y + s * 0.16);
    pb.line_to(x + s * 0.52, y + s * 0.84);
    pb.line_to(x + s * 0.30, y + s * 0.64);
    pb.line_to(x + s * 0.10, y + s * 0.64);
    pb.close();
    if let Some(path) = pb.finish() {
        fill(pixmap, &path, color);
    }

    if muted {
        // A cross, rather than waves.
        for (dx, dy) in [(1.0, 1.0), (1.0, -1.0)] {
            let mut pb = PathBuilder::new();
            let cx = x + s * 0.74;
            let cy = y + s * 0.50;
            let r = s * 0.16;
            pb.move_to(cx - r * dx, cy - r * dy);
            pb.line_to(cx + r * dx, cy + r * dy);
            if let Some(path) = pb.finish() {
                let mut paint = Paint::default();
                paint.set_color(color);
                paint.anti_alias = true;
                let stroke = tiny_skia::Stroke {
                    width: s * 0.09,
                    ..Default::default()
                };
                pixmap.stroke_path(&path, &paint, &stroke, Transform::identity(), None);
            }
        }
    } else {
        // Two arcs suggesting sound.
        for (i, r) in [s * 0.16, s * 0.26].iter().enumerate() {
            let mut pb = PathBuilder::new();
            let cx = x + s * 0.60;
            let cy = y + s * 0.50;
            pb.move_to(cx + r, cy - r * 0.7);
            pb.quad_to(cx + r * 1.5, cy, cx + r, cy + r * 0.7);
            if let Some(path) = pb.finish() {
                let mut paint = Paint::default();
                paint.set_color(
                    Color::from_rgba(1.0, 1.0, 1.0, alpha * (1.0 - i as f32 * 0.3))
                        .unwrap_or(Color::WHITE),
                );
                paint.anti_alias = true;
                let stroke = tiny_skia::Stroke {
                    width: s * 0.08,
                    ..Default::default()
                };
                pixmap.stroke_path(&path, &paint, &stroke, Transform::identity(), None);
            }
        }
    }
}

fn draw_close(pixmap: &mut Pixmap, x: f32, y: f32, alpha: f32) {
    let color = Color::from_rgba(1.0, 1.0, 1.0, alpha).unwrap_or(Color::WHITE);
    let s = ICON_SIZE;
    let mut paint = Paint::default();
    paint.set_color(color);
    paint.anti_alias = true;
    let stroke = tiny_skia::Stroke {
        width: s * 0.10,
        ..Default::default()
    };
    for (a, b) in [((0.25, 0.25), (0.75, 0.75)), ((0.75, 0.25), (0.25, 0.75))] {
        let mut pb = PathBuilder::new();
        pb.move_to(x + s * a.0, y + s * a.1);
        pb.line_to(x + s * b.0, y + s * b.1);
        if let Some(path) = pb.finish() {
            pixmap.stroke_path(&path, &paint, &stroke, Transform::identity(), None);
        }
    }
}

/// Rasterise the control bar and wrap it as an overlay composition.
///
/// Returns `None` when the bar is hidden, which tells the sink there is
/// nothing to composite and costs nothing.
pub fn render(state: &mut OverlayState) -> Option<gst_video::VideoOverlayComposition> {
    let (vw, vh) = state.video;
    if vw == 0 || vh == 0 || !state.visible() {
        state.hits.clear();
        return None;
    }

    let signature = state.signature();
    if let Some((cached_sig, composition)) = &state.cache {
        if *cached_sig == signature {
            return Some(composition.clone());
        }
    }

    let alpha = state.opacity();
    let width = vw as f32;
    let bar_w = (width - BAR_MARGIN * 2.0).max(200.0);
    let bar_x = BAR_MARGIN;
    let bar_y = vh as f32 - BAR_HEIGHT - BAR_MARGIN;

    let mut pixmap = Pixmap::new(vw, vh)?;

    // Backing panel.
    if let Some(path) = rounded_rect(bar_x, bar_y, bar_w, BAR_HEIGHT, 14.0) {
        fill(
            &mut pixmap,
            &path,
            Color::from_rgba(0.06, 0.06, 0.07, 0.82 * alpha).unwrap_or(Color::BLACK),
        );
    }

    let cy = bar_y + BAR_HEIGHT / 2.0;
    let mut hits = Vec::new();

    // Mute toggle.
    let mute_x = bar_x + 20.0;
    let icon_y = cy - ICON_SIZE / 2.0;
    draw_speaker(&mut pixmap, mute_x, icon_y, state.muted, alpha);
    hits.push(Hit {
        control: Control::Mute,
        x: mute_x - 8.0,
        y: icon_y - 8.0,
        w: ICON_SIZE + 16.0,
        h: ICON_SIZE + 16.0,
    });

    // Volume track.
    let track_x = mute_x + ICON_SIZE + 20.0;
    let track_w = 160.0;
    let track_h = 6.0;
    let track_y = cy - track_h / 2.0;
    fill_rect(
        &mut pixmap,
        track_x,
        track_y,
        track_w,
        track_h,
        Color::from_rgba(1.0, 1.0, 1.0, 0.22 * alpha).unwrap_or(Color::WHITE),
    );
    let level = if state.muted { 0.0 } else { state.volume as f32 };
    fill_rect(
        &mut pixmap,
        track_x,
        track_y,
        track_w * level,
        track_h,
        Color::from_rgba(ORANGE_RGB.0, ORANGE_RGB.1, ORANGE_RGB.2, alpha).unwrap_or(Color::WHITE),
    );
    // Knob.
    if let Some(path) = rounded_rect(
        track_x + track_w * level - 6.0,
        cy - 9.0,
        12.0,
        18.0,
        6.0,
    ) {
        fill(
            &mut pixmap,
            &path,
            Color::from_rgba(1.0, 1.0, 1.0, alpha).unwrap_or(Color::WHITE),
        );
    }
    hits.push(Hit {
        control: Control::VolumeTrack,
        x: track_x,
        y: cy - 14.0,
        w: track_w,
        h: 28.0,
    });

    // Close, on the right.
    let close_x = bar_x + bar_w - ICON_SIZE - 20.0;
    draw_close(&mut pixmap, close_x, icon_y, alpha);
    hits.push(Hit {
        control: Control::Close,
        x: close_x - 8.0,
        y: icon_y - 8.0,
        w: ICON_SIZE + 16.0,
        h: ICON_SIZE + 16.0,
    });

    // Highlight whatever the cursor is over.
    if let Some(hot) = state.hot {
        if let Some(hit) = hits.iter().find(|h| h.control == hot) {
            if let Some(path) = rounded_rect(hit.x, hit.y, hit.w, hit.h, 8.0) {
                fill(
                    &mut pixmap,
                    &path,
                    Color::from_rgba(1.0, 1.0, 1.0, 0.10 * alpha).unwrap_or(Color::WHITE),
                );
            }
        }
    }

    state.hits = hits;

    let composition = to_composition(pixmap, vw, vh)?;
    state.cache = Some((signature, composition.clone()));
    Some(composition)
}

/// Wrap the rasterised pixels as something the sink can composite.
///
/// tiny-skia produces premultiplied RGBA; GStreamer wants that flagged
/// explicitly or the edges of anti-aliased shapes come out wrong.
fn to_composition(
    pixmap: Pixmap,
    width: u32,
    height: u32,
) -> Option<gst_video::VideoOverlayComposition> {
    let data = pixmap.take();
    let mut buffer = gst::Buffer::from_mut_slice(data);
    {
        let buffer = buffer.get_mut()?;
        gst_video::VideoMeta::add(
            buffer,
            gst_video::VideoFrameFlags::empty(),
            gst_video::VideoFormat::Rgba,
            width,
            height,
        )
        .ok()?;
    }

    let rectangle = gst_video::VideoOverlayRectangle::new_raw(
        &buffer,
        0,
        0,
        width,
        height,
        gst_video::VideoOverlayFormatFlags::PREMULTIPLIED_ALPHA,
    );
    gst_video::VideoOverlayComposition::new(Some(&rectangle)).ok()
}
