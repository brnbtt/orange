//! Controls drawn onto the video.
//!
//! These are composited by `d3d11videosink` on the GPU via
//! `GstVideoOverlayComposition`, rather than being a second window floating
//! above the video. That matters: a separate window has to chase the video
//! window during moves and resizes, and always lags by a frame or two. A
//! composited overlay is part of the frame and cannot drift.
//!
//! Layout is a set of small clusters anchored to the corners of the picture,
//! not a bar. A live stream has no scrubber, so a full-width bar would be a
//! mostly-empty slab; and the corners are where a viewer already looks for
//! these things - status top-left, close top-right, audio bottom-left, view
//! controls bottom-right.
//!
//! Everything is authored in screen pixels and scaled to video pixels at the
//! last moment. The overlay is composited into the frame, which the sink then
//! scales to the window, so authoring in video pixels would make the controls
//! shrink on a 4K stream and swell on a 720p one.
//!
//! Each cluster is composited as its own rectangle. Nothing rasterises a
//! full-frame pixmap, so the cost does not grow with the stream resolution.

use gst::prelude::{ObjectExt, ToValue};
use gstreamer as gst;
use gstreamer_video as gst_video;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tiny_skia::{Color, FillRule, Paint, PathBuilder, Pixmap, Stroke, Transform};

use crate::text::{self, Weight};

/// How long the controls stay up after the last mouse movement.
const HIDE_AFTER: Duration = Duration::from_millis(1_000);
const FADE: Duration = Duration::from_millis(200);

// Design tokens, in logical pixels: what they measure on screen at 100%
// display scaling, whatever the stream resolution.
const MARGIN: f32 = 18.0;
const BUTTON: f32 = 40.0;
const ICON: f32 = 18.0;
const CHIP: f32 = 34.0;
const PAD: f32 = 13.0;
const TRACK: f32 = 110.0;
const LABEL: f32 = 13.0;
const CONTROL_RADIUS: f32 = 12.0;

// The app's palette, matching the tray.
const INK: (f32, f32, f32) = (0.043, 0.043, 0.043);
const CREAM: (f32, f32, f32) = (0.902, 0.878, 0.820);
const ORANGE: (f32, f32, f32) = (1.0, 0.353, 0.122);
const DANGER: (f32, f32, f32) = (0.878, 0.392, 0.373);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Control {
    Mute,
    AudioGap,
    VolumeTrack,
    Fullscreen,
    Close,
    Stats,
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
    /// Video frame size. The overlay is composited into this space.
    pub video: (u32, u32),
    /// Window client size, fed from the message loop. Together with `video`
    /// this gives the scale the sink will apply, which is what keeps the
    /// controls a constant size on screen.
    pub client: (u32, u32),
    /// The window's DPI over 96. Client sizes arrive in physical pixels, so
    /// without this the controls shrink as display scaling goes up - exactly
    /// backwards from what the setting is asking for.
    pub dpi: f32,
    pub volume: f64,
    pub muted: bool,
    volume_dragging: bool,
    pub fullscreen: bool,
    pub viewers: Option<usize>,
    pub host: Option<String>,
    pub fps: Option<f64>,
    pub bitrate_kbps: Option<u32>,
    /// Persistent compact status used by the bottom-right self-monitor.
    pub monitor_mode: bool,
    shown_at: Instant,
    hot: Option<Control>,
    hits: Vec<Hit>,
    /// Cached rasterisation, invalidated when anything visible changes.
    cache: Option<(u64, gst_video::VideoOverlayComposition)>,
    pub close_requested: bool,
    pub fullscreen_requested: bool,
    /// Hold the controls open instead of hiding them. Only the design harness
    /// sets this; chasing a fading overlay with the mouse makes layout work
    /// impossible.
    pub pinned: bool,
}

impl Default for OverlayState {
    fn default() -> Self {
        Self {
            video: (0, 0),
            client: (0, 0),
            dpi: 1.0,
            volume: 0.3,
            muted: false,
            volume_dragging: false,
            fullscreen: false,
            viewers: None,
            host: None,
            fps: None,
            bitrate_kbps: None,
            monitor_mode: false,
            // Start hidden; the first mouse move reveals the controls.
            shown_at: Instant::now() - HIDE_AFTER * 2,
            hot: None,
            hits: Vec::new(),
            cache: None,
            close_requested: false,
            fullscreen_requested: false,
            pinned: false,
        }
    }
}

pub type SharedOverlay = Arc<Mutex<OverlayState>>;

impl OverlayState {
    pub fn visible(&self) -> bool {
        self.pinned || self.volume_dragging || self.shown_at.elapsed() < HIDE_AFTER
    }

    /// Fade factor, so the controls dissolve rather than vanishing.
    fn opacity(&self) -> f32 {
        if self.pinned || self.volume_dragging {
            return 1.0;
        }
        let elapsed = self.shown_at.elapsed();
        if elapsed >= HIDE_AFTER {
            return 0.0;
        }
        let fade_start = HIDE_AFTER.saturating_sub(FADE);
        if elapsed < fade_start {
            1.0
        } else {
            let t = (elapsed - fade_start).as_secs_f32() / FADE.as_secs_f32();
            (1.0 - t).clamp(0.0, 1.0)
        }
    }

    /// Video pixels per unit of design.
    ///
    /// Two conversions. The sink letterboxes to preserve aspect, so the
    /// picture is scaled by `min(cw/vw, ch/vh)` on its way to the window;
    /// dividing by that cancels it out. And client sizes are physical pixels,
    /// so display scaling has to be applied on top or the controls come out
    /// smaller the more zoomed-in the desktop is.
    fn scale(&self) -> f32 {
        let (vw, vh) = self.video;
        let (cw, ch) = self.client;
        let dpi = if self.dpi > 0.0 { self.dpi } else { 1.0 };
        if vw == 0 || vh == 0 {
            return dpi;
        }
        if cw == 0 || ch == 0 {
            // The window has not reported a size yet. Assume a roughly 1080p
            // display rather than 1:1, which would make 4K controls unusably
            // small for the frame or two before WM_SIZE arrives.
            return dpi * (vh as f32 / 1080.0).max(1.0);
        }
        let fit = (cw as f32 / vw as f32).min(ch as f32 / vh as f32);
        if fit > 0.0 {
            dpi / fit
        } else {
            dpi
        }
    }

    pub fn wake(&mut self) {
        self.shown_at = Instant::now();
    }

    /// Whether the cursor is over something clickable, which decides if a
    /// click should press it or drag the window.
    ///
    /// The status chip is excluded: it reacts to hover but does nothing when
    /// clicked, and it sits in the top-left corner where people naturally
    /// grab a borderless window to move it.
    pub fn hovered(&self) -> bool {
        !matches!(self.hot, None | Some(Control::Stats))
    }

    /// Feed a mouse position in video coordinates. Returns true if a redraw is
    /// warranted.
    pub fn on_mouse_move(&mut self, x: f32, y: f32) -> bool {
        self.wake();
        let previous = self.hot;
        let next = self
            .hits
            .iter()
            .find(|h| h.contains(x, y))
            .map(|h| h.control);
        // The closed audio control carries a latent corridor where its slider
        // will appear. It only activates while leaving an audio control, so
        // moving through quickly is safe without an invisible hover target
        // opening the slider from elsewhere in the window.
        self.hot = match (previous, next) {
            (
                Some(Control::Mute | Control::AudioGap | Control::VolumeTrack),
                Some(Control::AudioGap),
            ) => Some(Control::AudioGap),
            (_, Some(Control::AudioGap)) => None,
            (_, next) => next,
        };
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
            Control::Fullscreen => self.fullscreen_requested = true,
            Control::Stats | Control::AudioGap => {}
            Control::VolumeTrack => {
                let t = ((x - hit.x) / hit.w).clamp(0.0, 1.0);
                self.volume = t as f64;
                self.muted = false;
                self.volume_dragging = true;
            }
        }
        self.cache = None;
    }

    /// Whether the audio cluster should show its slider. Hovering either the
    /// speaker or the slider keeps it open, so the cursor can travel between
    /// them without it collapsing underfoot.
    fn audio_open(&self) -> bool {
        self.volume_dragging
            || matches!(
                self.hot,
                Some(Control::Mute | Control::AudioGap | Control::VolumeTrack)
            )
    }

    pub fn volume_dragging(&self) -> bool {
        self.volume_dragging
    }

    /// Continue a slider drag even after the pointer leaves its visual bounds.
    pub fn drag_volume(&mut self, x: f32) -> bool {
        if !self.volume_dragging {
            return false;
        }
        let Some(track) = self
            .hits
            .iter()
            .find(|hit| hit.control == Control::VolumeTrack)
            .copied()
        else {
            return false;
        };
        let next = ((x - track.x) / track.w).clamp(0.0, 1.0) as f64;
        let changed = (next - self.volume).abs() > f64::EPSILON || self.muted;
        self.volume = next;
        self.muted = false;
        self.wake();
        if changed {
            self.cache = None;
        }
        changed
    }

    pub fn end_volume_drag(&mut self) -> bool {
        std::mem::take(&mut self.volume_dragging)
    }

    /// A short description of what is being received: the product's whole
    /// claim, and until now invisible to the person watching.
    fn quality_label(&self) -> String {
        let (w, h) = self.video;
        if w == 0 || h == 0 {
            return String::from("connecting");
        }
        let name = match h {
            2160 => String::from("4K"),
            1440 => String::from("1440p"),
            1080 => String::from("1080p"),
            720 => String::from("720p"),
            _ => format!("{w}x{h}"),
        };
        match self.fps {
            Some(fps) if fps > 0.0 => format!("{name}{}", fps.round() as u32),
            _ => name,
        }
    }

    fn detail_label(&self) -> Option<String> {
        let mut parts = Vec::new();
        if let Some(kbps) = self.bitrate_kbps {
            parts.push(format!("{:.0} Mbps", kbps as f32 / 1000.0));
        }
        if let Some(host) = &self.host {
            parts.push(host.clone());
        }
        if let Some(n) = self.viewers {
            parts.push(format!("{n} watching"));
        }
        if parts.is_empty() {
            None
        } else {
            Some(parts.join("  \u{00b7}  "))
        }
    }

    /// Identifies a cache entry. Any change here forces a redraw.
    fn signature(&self) -> u64 {
        let mut hasher = DefaultHasher::new();
        self.video.hash(&mut hasher);
        self.client.hash(&mut hasher);
        self.dpi.to_bits().hash(&mut hasher);
        self.visible().hash(&mut hasher);
        ((self.opacity() * 24.0) as u32).hash(&mut hasher);
        ((self.volume * 100.0) as u32).hash(&mut hasher);
        self.muted.hash(&mut hasher);
        self.volume_dragging.hash(&mut hasher);
        self.fullscreen.hash(&mut hasher);
        self.monitor_mode.hash(&mut hasher);
        self.hot.map(|c| c as u8).hash(&mut hasher);
        self.quality_label().hash(&mut hasher);
        self.detail_label().hash(&mut hasher);
        hasher.finish()
    }
}

// --- painting ---------------------------------------------------------------

fn rgba(c: (f32, f32, f32), a: f32) -> Color {
    Color::from_rgba(c.0, c.1, c.2, a.clamp(0.0, 1.0)).unwrap_or(Color::TRANSPARENT)
}

fn rounded_rect(x: f32, y: f32, w: f32, h: f32, r: f32) -> Option<tiny_skia::Path> {
    let r = r.min(w / 2.0).min(h / 2.0);
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

fn fill_round(pixmap: &mut Pixmap, x: f32, y: f32, w: f32, h: f32, r: f32, color: Color) {
    if let Some(path) = rounded_rect(x, y, w, h, r) {
        fill(pixmap, &path, color);
    }
}

fn stroke(pixmap: &mut Pixmap, path: &tiny_skia::Path, color: Color, width: f32) {
    let mut paint = Paint::default();
    paint.set_color(color);
    paint.anti_alias = true;
    let stroke = Stroke {
        width,
        line_cap: tiny_skia::LineCap::Round,
        line_join: tiny_skia::LineJoin::Round,
        ..Default::default()
    };
    pixmap.stroke_path(path, &paint, &stroke, Transform::identity(), None);
}

fn circle(pixmap: &mut Pixmap, cx: f32, cy: f32, r: f32, color: Color) {
    let mut pb = PathBuilder::new();
    pb.push_circle(cx, cy, r);
    if let Some(path) = pb.finish() {
        fill(pixmap, &path, color);
    }
}

/// The panel every cluster sits on.
///
/// Dark enough that white sits on it cleanly over white video - the scrim has
/// to survive the worst case, not the average one - but small enough that
/// being nearly opaque hides almost nothing.
fn panel(pixmap: &mut Pixmap, x: f32, y: f32, w: f32, h: f32, r: f32, alpha: f32) {
    fill_round(pixmap, x, y, w, h, r, rgba(INK, 0.84 * alpha));
}

// --- icons ------------------------------------------------------------------

/// A speaker, drawn by hand to avoid dragging in an icon set.
fn speaker(pixmap: &mut Pixmap, x: f32, y: f32, s: f32, muted: bool, color: Color) {
    let mut pb = PathBuilder::new();
    pb.move_to(x + s * 0.06, y + s * 0.34);
    pb.line_to(x + s * 0.26, y + s * 0.34);
    pb.line_to(x + s * 0.50, y + s * 0.12);
    pb.line_to(x + s * 0.50, y + s * 0.88);
    pb.line_to(x + s * 0.26, y + s * 0.66);
    pb.line_to(x + s * 0.06, y + s * 0.66);
    pb.close();
    if let Some(path) = pb.finish() {
        fill(pixmap, &path, color);
    }

    if muted {
        for (dx, dy) in [(1.0, 1.0), (1.0, -1.0)] {
            let (cx, cy, r) = (x + s * 0.76, y + s * 0.50, s * 0.17);
            let mut pb = PathBuilder::new();
            pb.move_to(cx - r * dx, cy - r * dy);
            pb.line_to(cx + r * dx, cy + r * dy);
            if let Some(path) = pb.finish() {
                stroke(pixmap, &path, color, s * 0.10);
            }
        }
    } else {
        // One deliberate wave stays legible after the video sink scales the
        // overlay. Two nested hairline waves looked soft and busy at 150% DPI.
        let mut pb = PathBuilder::new();
        pb.move_to(x + s * 0.64, y + s * 0.29);
        pb.cubic_to(
            x + s * 0.88,
            y + s * 0.38,
            x + s * 0.88,
            y + s * 0.62,
            x + s * 0.64,
            y + s * 0.71,
        );
        if let Some(path) = pb.finish() {
            stroke(pixmap, &path, color, s * 0.10);
        }
    }
}

fn cross(pixmap: &mut Pixmap, x: f32, y: f32, s: f32, color: Color) {
    for (a, b) in [((0.22, 0.22), (0.78, 0.78)), ((0.78, 0.22), (0.22, 0.78))] {
        let mut pb = PathBuilder::new();
        pb.move_to(x + s * a.0, y + s * a.1);
        pb.line_to(x + s * b.0, y + s * b.1);
        if let Some(path) = pb.finish() {
            stroke(pixmap, &path, color, s * 0.11);
        }
    }
}

/// Compact broadcast mark for a live stream: a source dot with one signal
/// wave on each side. It remains recognizable at the collapsed 18px size.
fn live_mark(pixmap: &mut Pixmap, x: f32, y: f32, s: f32, color: Color) {
    let cx = x + s / 2.0;
    let cy = y + s / 2.0;
    circle(pixmap, cx, cy, s * 0.12, color);
    for side in [-1.0f32, 1.0] {
        let mut pb = PathBuilder::new();
        pb.move_to(cx + side * s * 0.24, cy - s * 0.22);
        pb.quad_to(
            cx + side * s * 0.42,
            cy,
            cx + side * s * 0.24,
            cy + s * 0.22,
        );
        if let Some(path) = pb.finish() {
            stroke(pixmap, &path, color, s * 0.10);
        }
    }
}

/// Corner brackets: pointing outwards to enter fullscreen, inwards to leave.
///
/// The arms are kept well short of meeting. Brackets that almost touch read
/// as a plain square at the sizes these are drawn at.
fn expand(pixmap: &mut Pixmap, x: f32, y: f32, s: f32, exiting: bool, color: Color) {
    let arm = s * 0.26;
    let inset = s * 0.08;
    let width = s * 0.11;

    for (sx, sy) in [(1.0f32, 1.0f32), (-1.0, 1.0), (1.0, -1.0), (-1.0, -1.0)] {
        // The corner of the icon box this bracket belongs to. Leaving
        // fullscreen pulls the elbows inward so they point back at the centre.
        let pull = if exiting { s * 0.22 } else { 0.0 };
        let cx = if sx > 0.0 {
            x + inset + pull
        } else {
            x + s - inset - pull
        };
        let cy = if sy > 0.0 {
            y + inset + pull
        } else {
            y + s - inset - pull
        };
        // Entering: elbow at the corner, arms running along the edges.
        // Leaving: elbow inboard, arms running back out towards the corner.
        let direction = if exiting { -1.0 } else { 1.0 };

        let mut pb = PathBuilder::new();
        pb.move_to(cx + sx * arm * direction, cy);
        pb.line_to(cx, cy);
        pb.line_to(cx, cy + sy * arm * direction);
        if let Some(path) = pb.finish() {
            stroke(pixmap, &path, color, width);
        }
    }
}

// --- layout -----------------------------------------------------------------

/// A rasterised cluster and where it belongs, in video coordinates.
struct Panel {
    pixmap: Pixmap,
    x: f32,
    y: f32,
    render_w: f32,
    render_h: f32,
}

/// Build one cluster: allocate a pixmap of the right size, let `paint` fill
/// it, and record where it goes.
fn cluster(
    x: f32,
    y: f32,
    render_w: f32,
    render_h: f32,
    raster_w: f32,
    raster_h: f32,
    paint: impl FnOnce(&mut Pixmap),
) -> Option<Panel> {
    let mut pixmap = Pixmap::new(
        raster_w.ceil().max(1.0) as u32,
        raster_h.ceil().max(1.0) as u32,
    )?;
    paint(&mut pixmap);
    Some(Panel {
        pixmap,
        x,
        y,
        render_w,
        render_h,
    })
}

/// Rasterise the controls and wrap them as an overlay composition.
///
/// Hidden controls become one transparent pixel. The element's `draw` signal
/// requires a composition object even when there is nothing visible.
pub fn render(state: &mut OverlayState) -> Option<gst_video::VideoOverlayComposition> {
    let (vw, vh) = state.video;
    if vw == 0 || vh == 0 {
        state.hits.clear();
        return transparent_composition();
    }

    let signature = state.signature();
    if let Some((cached, composition)) = &state.cache {
        if *cached == signature {
            return Some(composition.clone());
        }
    }

    // The element's `draw` signal requires a composition return value. `None`
    // aborts inside GLib once the controls time out, so hidden means one fully
    // transparent pixel rather than no object at all.
    if !state.visible() && !state.monitor_mode {
        state.hits.clear();
        let composition = transparent_composition()?;
        state.cache = Some((signature, composition.clone()));
        return Some(composition);
    }

    let alpha = state.opacity();
    let status_alpha = if state.monitor_mode { 1.0 } else { alpha };
    // Placement is in video coordinates; painting is at the output's physical
    // DPI. If both use video scale, tiny-skia's antialiasing is filtered again
    // when the sink fits the stream to the window, which softens every icon.
    let render_scale = state.scale();
    let raster_scale = state.dpi.max(1.0);
    let (fw, fh) = (vw as f32, vh as f32);

    let ink = rgba(CREAM, alpha);
    let status_ink = rgba(CREAM, status_alpha);
    let mut hits: Vec<Hit> = Vec::new();
    let mut panels: Vec<Panel> = Vec::new();

    let hot_alpha = |control: Control, base: f32| {
        if state.hot == Some(control) {
            1.0
        } else {
            base
        }
    };

    // --- status, top-left ---------------------------------------------------
    // A broadcast mark at rest; hovering expands it to what is being received.
    {
        let expanded = state.hot == Some(Control::Stats);
        let received = state.quality_label();
        let quality = if state.monitor_mode {
            String::from("LIVE")
        } else {
            received.clone()
        };
        let detail = if expanded {
            let detail = state.detail_label();
            if state.monitor_mode {
                Some(match detail {
                    Some(detail) => format!("{received}  \u{00b7}  {detail}"),
                    None => received,
                })
            } else {
                detail
            }
        } else {
            None
        };
        let has_text = text::available();
        let label_size = LABEL * raster_scale;
        let mut text_w = text::width(&quality, label_size, Weight::Semibold) / raster_scale;
        if let Some(detail) = &detail {
            text_w += text::width(
                &format!("  \u{00b7}  {detail}"),
                label_size,
                Weight::Regular,
            ) / raster_scale;
        }

        // Logical dimensions first; each is independently converted for the
        // destination rectangle and for the source pixmap.
        let h = CHIP;
        let show_label = (expanded || state.monitor_mode) && has_text;
        let w = if show_label {
            CHIP + 8.0 + text_w + PAD
        } else {
            CHIP
        };
        // Centred on the close button's axis rather than sharing its top
        // edge: the chip is shorter, and aligning tops leaves it looking
        // like it slipped.
        let x = MARGIN * render_scale;
        let y = (MARGIN + (BUTTON - h) / 2.0) * render_scale;

        if let Some(p) = cluster(
            x,
            y,
            w * render_scale,
            h * render_scale,
            w * raster_scale,
            h * raster_scale,
            |pixmap| {
                let (w, h) = (w * raster_scale, h * raster_scale);
                let icon = ICON * raster_scale;
                panel(
                    pixmap,
                    0.0,
                    0.0,
                    w,
                    h,
                    CONTROL_RADIUS * raster_scale,
                    status_alpha,
                );
                if state.monitor_mode {
                    if let Some(path) = rounded_rect(
                        0.5 * raster_scale,
                        0.5 * raster_scale,
                        w - raster_scale,
                        h - raster_scale,
                        CONTROL_RADIUS * raster_scale,
                    ) {
                        stroke(pixmap, &path, rgba(ORANGE, 0.42), raster_scale);
                    }
                }
                live_mark(
                    pixmap,
                    (CHIP * raster_scale - icon) / 2.0,
                    (h - icon) / 2.0,
                    icon,
                    rgba(ORANGE, status_alpha),
                );
                if !show_label {
                    return;
                }
                let baseline = h / 2.0 + text::cap_height(label_size) / 2.0;
                let mut caret = (CHIP + 8.0) * raster_scale;
                text::draw(
                    pixmap,
                    caret,
                    baseline,
                    &quality,
                    label_size,
                    Weight::Semibold,
                    status_ink,
                );
                if let Some(detail) = &detail {
                    caret += text::width(&quality, label_size, Weight::Semibold);
                    let joined = format!("  \u{00b7}  {detail}");
                    text::draw(
                        pixmap,
                        caret,
                        baseline,
                        &joined,
                        label_size,
                        Weight::Regular,
                        rgba(CREAM, 0.62 * status_alpha),
                    );
                }
            },
        ) {
            panels.push(p);
            hits.push(Hit {
                control: Control::Stats,
                x,
                y,
                w: w * render_scale,
                h: h * render_scale,
            });
        }
    }

    // --- close, top-right ---------------------------------------------------
    // Where a window's close button would be, since this frame has no title
    // bar of its own. Tinted red on hover: it ends the session.
    if alpha > 0.0 {
        let (w, h) = (BUTTON * render_scale, BUTTON * render_scale);
        let (x, y) = (fw - MARGIN * render_scale - w, MARGIN * render_scale);
        let hovered = state.hot == Some(Control::Close);
        if let Some(p) = cluster(
            x,
            y,
            w,
            h,
            BUTTON * raster_scale,
            BUTTON * raster_scale,
            |pixmap| {
                let w = BUTTON * raster_scale;
                let h = w;
                let icon = ICON * raster_scale;
                let control_radius = CONTROL_RADIUS * raster_scale;
                panel(pixmap, 0.0, 0.0, w, h, control_radius, alpha);
                if hovered {
                    fill_round(
                        pixmap,
                        0.0,
                        0.0,
                        w,
                        h,
                        control_radius,
                        rgba(DANGER, 0.22 * alpha),
                    );
                }
                let color = if hovered { rgba(DANGER, alpha) } else { ink };
                cross(pixmap, (w - icon) / 2.0, (h - icon) / 2.0, icon, color);
            },
        ) {
            panels.push(p);
            hits.push(Hit {
                control: Control::Close,
                x,
                y,
                w,
                h,
            });
        }
    }

    // --- audio, bottom-left -------------------------------------------------
    // A speaker on its own until pointed at, then the slider grows out of it
    // to the right. The volume control is the one thing a viewer actually
    // reaches for, so it gets the largest target of the four.
    if alpha > 0.0 && !state.monitor_mode {
        let open = state.audio_open();
        let gap = 12.0;
        let h = BUTTON;
        // The slider grows out to the right of the speaker, which stays
        // exactly where it was. Recentring the icon in a wider pill would
        // make it jump sideways under the cursor that just opened it.
        let w = if open {
            BUTTON + gap + TRACK + PAD
        } else {
            BUTTON
        };
        let x = MARGIN * render_scale;
        let y = fh - (MARGIN + h) * render_scale;

        let level = if state.muted {
            0.0
        } else {
            state.volume as f32
        };
        let muted = state.muted;

        if let Some(p) = cluster(
            x,
            y,
            w * render_scale,
            h * render_scale,
            w * raster_scale,
            h * raster_scale,
            |pixmap| {
                let w = w * raster_scale;
                let h = h * raster_scale;
                let button = BUTTON * raster_scale;
                let gap = gap * raster_scale;
                let track = TRACK * raster_scale;
                let icon = ICON * raster_scale;
                let control_radius = CONTROL_RADIUS * raster_scale;
                panel(
                    pixmap,
                    0.0,
                    0.0,
                    w,
                    h,
                    if open { h * 0.36 } else { control_radius },
                    alpha,
                );
                speaker(
                    pixmap,
                    (button - icon) / 2.0,
                    (h - icon) / 2.0,
                    icon,
                    muted,
                    rgba(CREAM, hot_alpha(Control::Mute, 0.85) * alpha),
                );
                if !open {
                    return;
                }
                let tx = button + gap;
                let th = 4.0 * raster_scale;
                let ty = h / 2.0 - th / 2.0;
                fill_round(
                    pixmap,
                    tx,
                    ty,
                    track,
                    th,
                    th / 2.0,
                    rgba(CREAM, 0.22 * alpha),
                );
                if level > 0.0 {
                    fill_round(
                        pixmap,
                        tx,
                        ty,
                        track * level,
                        th,
                        th / 2.0,
                        rgba(ORANGE, alpha),
                    );
                }
                circle(
                    pixmap,
                    tx + track * level,
                    h / 2.0,
                    6.0 * raster_scale,
                    rgba(CREAM, alpha),
                );
            },
        ) {
            panels.push(p);
            hits.push(Hit {
                control: Control::Mute,
                x,
                y,
                w: BUTTON * render_scale,
                h: h * render_scale,
            });
            if open {
                hits.push(Hit {
                    control: Control::VolumeTrack,
                    x: x + (BUTTON + gap) * render_scale,
                    y,
                    w: TRACK * render_scale,
                    h: h * render_scale,
                });
            }
            // Covers both the visible gap and the future track. It comes after
            // real controls so clicks use their exact geometry.
            hits.push(Hit {
                control: Control::AudioGap,
                x: x + BUTTON * render_scale,
                y,
                w: (gap + TRACK + PAD) * render_scale,
                h: h * render_scale,
            });
        }
    }

    // --- view, bottom-right -------------------------------------------------
    // Fullscreen, where every video player puts it.
    if alpha > 0.0 && !state.monitor_mode {
        let (w, h) = (BUTTON * render_scale, BUTTON * render_scale);
        let x = fw - (MARGIN + BUTTON) * render_scale;
        let y = fh - (MARGIN + BUTTON) * render_scale;
        let exiting = state.fullscreen;
        let hovered = state.hot == Some(Control::Fullscreen);
        if let Some(p) = cluster(
            x,
            y,
            w,
            h,
            BUTTON * raster_scale,
            BUTTON * raster_scale,
            |pixmap| {
                let w = BUTTON * raster_scale;
                let h = w;
                let icon = ICON * raster_scale;
                let control_radius = CONTROL_RADIUS * raster_scale;
                panel(pixmap, 0.0, 0.0, w, h, control_radius, alpha);
                if hovered {
                    fill_round(
                        pixmap,
                        0.0,
                        0.0,
                        w,
                        h,
                        control_radius,
                        rgba(CREAM, 0.10 * alpha),
                    );
                }
                expand(
                    pixmap,
                    (w - icon) / 2.0,
                    (h - icon) / 2.0,
                    icon,
                    exiting,
                    rgba(CREAM, hot_alpha(Control::Fullscreen, 0.85) * alpha),
                );
            },
        ) {
            panels.push(p);
            hits.push(Hit {
                control: Control::Fullscreen,
                x,
                y,
                w,
                h,
            });
        }
    }

    state.hits = hits;

    let composition = to_composition(panels)?;
    state.cache = Some((signature, composition.clone()));
    Some(composition)
}

fn transparent_composition() -> Option<gst_video::VideoOverlayComposition> {
    let pixmap = Pixmap::new(1, 1)?;
    to_composition(vec![Panel {
        pixmap,
        x: 0.0,
        y: 0.0,
        render_w: 1.0,
        render_h: 1.0,
    }])
}

/// Wrap the rasterised clusters as something the sink can composite.
///
/// Two conversions matter. tiny-skia produces premultiplied alpha, which
/// GStreamer wants flagged explicitly or anti-aliased edges come out wrong.
/// And it lays pixels out as RGBA, whereas an overlay composition on a
/// little-endian machine must be BGRA - `GST_VIDEO_OVERLAY_COMPOSITION_FORMAT_RGB`
/// is an alias for it. Getting that wrong makes `new_raw` return NULL, which
/// aborts the process from inside a C callback with no usable message.
fn to_composition(panels: Vec<Panel>) -> Option<gst_video::VideoOverlayComposition> {
    let mut rectangles = Vec::with_capacity(panels.len());

    for panel in panels {
        let (source_w, source_h) = (panel.pixmap.width(), panel.pixmap.height());
        // Round both destination edges rather than the origin and width
        // independently, or fractional scales can shift the far edge a pixel.
        let left = panel.x.round() as i32;
        let top = panel.y.round() as i32;
        let right = (panel.x + panel.render_w).round() as i32;
        let bottom = (panel.y + panel.render_h).round() as i32;
        let render_w = (right - left).max(1) as u32;
        let render_h = (bottom - top).max(1) as u32;
        let mut data = panel.pixmap.take();
        for pixel in data.chunks_exact_mut(4) {
            pixel.swap(0, 2);
        }

        let mut buffer = gst::Buffer::from_mut_slice(data);
        {
            let buffer = buffer.get_mut()?;
            gst_video::VideoMeta::add(
                buffer,
                gst_video::VideoFrameFlags::empty(),
                gst_video::VideoFormat::Bgra,
                source_w,
                source_h,
            )
            .ok()?;
        }

        rectangles.push(gst_video::VideoOverlayRectangle::new_raw(
            &buffer,
            left,
            top,
            render_w,
            render_h,
            gst_video::VideoOverlayFormatFlags::PREMULTIPLIED_ALPHA,
        ));
    }

    gst_video::VideoOverlayComposition::new(rectangles.iter()).ok()
}

/// Bind an `overlaycomposition` element to shared overlay state.
///
/// Both the real viewer and the design harness call this, so what you see
/// while iterating on the layout is what a viewer actually gets.
pub fn attach(composition: &gst::Element, overlay: &SharedOverlay, hwnd: isize) {
    // Learn the video size and frame rate; the former is the coordinate space
    // the overlay and all hit testing work in, and both are shown to the
    // viewer.
    let state = overlay.clone();
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
                let mut resize = None;
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
                        resize = Some((hwnd, state.video.0, state.video.1));
                    }
                }
                if let Some((hwnd, width, height)) = resize {
                    crate::window::set_video_aspect(hwnd, width, height);
                }
            }
        }
        None
    });

    let state = overlay.clone();
    composition.connect("draw", false, move |_values| {
        let mut state = state.lock().ok()?;
        render(&mut state).map(|c| c.to_value())
    });
}
