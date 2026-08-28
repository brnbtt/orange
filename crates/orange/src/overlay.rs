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
const HIDE_AFTER: Duration = Duration::from_secs(3);
const FADE: Duration = Duration::from_millis(500);

// Design tokens, in logical pixels: what they measure on screen at 100%
// display scaling, whatever the stream resolution.
const MARGIN: f32 = 22.0;
const BUTTON: f32 = 42.0;
const ICON: f32 = 19.0;
const CHIP: f32 = 34.0;
const PAD: f32 = 13.0;
const TRACK: f32 = 110.0;
const LABEL: f32 = 13.0;

// The app's palette, matching the tray.
const INK: (f32, f32, f32) = (0.043, 0.031, 0.043);
const CREAM: (f32, f32, f32) = (0.902, 0.878, 0.820);
const ORANGE: (f32, f32, f32) = (1.0, 0.353, 0.122);
const DANGER: (f32, f32, f32) = (0.878, 0.392, 0.373);
const GREEN: (f32, f32, f32) = (0.306, 0.788, 0.478);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Control {
    Mute,
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
    pub fullscreen: bool,
    pub viewers: Option<usize>,
    pub host: Option<String>,
    pub fps: Option<f64>,
    pub bitrate_kbps: Option<u32>,
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
            volume: 1.0,
            muted: false,
            fullscreen: false,
            viewers: None,
            host: None,
            fps: None,
            bitrate_kbps: None,
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
        self.pinned || self.shown_at.elapsed() < HIDE_AFTER
    }

    /// Fade factor, so the controls dissolve rather than vanishing.
    fn opacity(&self) -> f32 {
        if self.pinned {
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
        if fit > 0.0 { dpi / fit } else { dpi }
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
            Control::Fullscreen => self.fullscreen_requested = true,
            Control::Stats => {}
            Control::VolumeTrack => {
                let t = ((x - hit.x) / hit.w).clamp(0.0, 1.0);
                self.volume = t as f64;
                self.muted = false;
            }
        }
        self.cache = None;
    }

    /// Whether the audio cluster should show its slider. Hovering either the
    /// speaker or the slider keeps it open, so the cursor can travel between
    /// them without it collapsing underfoot.
    fn audio_open(&self) -> bool {
        matches!(self.hot, Some(Control::Mute) | Some(Control::VolumeTrack))
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
        ((self.opacity() * 24.0) as u32).hash(&mut hasher);
        ((self.volume * 100.0) as u32).hash(&mut hasher);
        self.muted.hash(&mut hasher);
        self.fullscreen.hash(&mut hasher);
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
        for (i, r) in [s * 0.16, s * 0.28].iter().enumerate() {
            let (cx, cy) = (x + s * 0.58, y + s * 0.50);
            let mut pb = PathBuilder::new();
            pb.move_to(cx + r, cy - r * 0.72);
            pb.quad_to(cx + r * 1.5, cy, cx + r, cy + r * 0.72);
            if let Some(path) = pb.finish() {
                let faded = Color::from_rgba(
                    color.red(),
                    color.green(),
                    color.blue(),
                    color.alpha() * (1.0 - i as f32 * 0.35),
                )
                .unwrap_or(color);
                stroke(pixmap, &path, faded, s * 0.09);
            }
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
}

/// Build one cluster: allocate a pixmap of the right size, let `paint` fill
/// it, and record where it goes.
fn cluster(x: f32, y: f32, w: f32, h: f32, paint: impl FnOnce(&mut Pixmap)) -> Option<Panel> {
    let mut pixmap = Pixmap::new(w.ceil().max(1.0) as u32, h.ceil().max(1.0) as u32)?;
    paint(&mut pixmap);
    Some(Panel { pixmap, x, y })
}

/// Rasterise the controls and wrap them as an overlay composition.
///
/// Returns `None` when they are hidden, which tells the sink there is nothing
/// to composite and costs nothing.
pub fn render(state: &mut OverlayState) -> Option<gst_video::VideoOverlayComposition> {
    let (vw, vh) = state.video;
    if vw == 0 || vh == 0 || !state.visible() {
        state.hits.clear();
        return None;
    }

    let signature = state.signature();
    if let Some((cached, composition)) = &state.cache {
        if *cached == signature {
            return Some(composition.clone());
        }
    }

    let alpha = state.opacity();
    let s = state.scale();
    let (fw, fh) = (vw as f32, vh as f32);

    // Screen-pixel design sizes, in video pixels.
    let margin = MARGIN * s;
    let button = BUTTON * s;
    let icon = ICON * s;
    let chip = CHIP * s;
    let pad = PAD * s;
    let label_size = LABEL * s;

    let ink = rgba(CREAM, alpha);
    let mut hits: Vec<Hit> = Vec::new();
    let mut panels: Vec<Panel> = Vec::new();

    let hot_alpha = |control: Control, base: f32| {
        if state.hot == Some(control) { 1.0 } else { base }
    };

    // --- status, top-left ---------------------------------------------------
    // What you are receiving. Collapsed it is a dot and a quality label;
    // hovering adds bitrate, host and viewer count.
    {
        let quality = state.quality_label();
        let detail = if state.hot == Some(Control::Stats) {
            state.detail_label()
        } else {
            None
        };
        let dot = 7.0 * s;
        let gap = 9.0 * s;
        let has_text = text::available();

        let mut text_w = text::width(&quality, label_size, Weight::Semibold);
        if let Some(detail) = &detail {
            text_w += text::width(&format!("  \u{00b7}  {detail}"), label_size, Weight::Regular);
        }

        let h = chip;
        let w = if has_text {
            pad + dot + gap + text_w + pad
        } else {
            pad + dot + pad
        };
        // Centred on the close button's axis rather than sharing its top
        // edge: the chip is shorter, and aligning tops leaves it looking
        // like it slipped.
        let (x, y) = (margin, margin + (button - h) / 2.0);

        if let Some(p) = cluster(x, y, w, h, |pixmap| {
            panel(pixmap, 0.0, 0.0, w, h, h / 2.0, alpha);
            circle(pixmap, pad + dot / 2.0, h / 2.0, dot / 2.0, rgba(GREEN, alpha));
            if !has_text {
                return;
            }
            let baseline = h / 2.0 + text::cap_height(label_size) / 2.0;
            let mut caret = pad + dot + gap;
            text::draw(
                pixmap,
                caret,
                baseline,
                &quality,
                label_size,
                Weight::Semibold,
                ink,
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
                    rgba(CREAM, 0.62 * alpha),
                );
            }
        }) {
            panels.push(p);
            hits.push(Hit {
                control: Control::Stats,
                x,
                y,
                w,
                h,
            });
        }
    }

    // --- close, top-right ---------------------------------------------------
    // Where a window's close button would be, since this frame has no title
    // bar of its own. Tinted red on hover: it ends the session.
    {
        let (w, h) = (button, button);
        let (x, y) = (fw - margin - w, margin);
        let hovered = state.hot == Some(Control::Close);
        if let Some(p) = cluster(x, y, w, h, |pixmap| {
            panel(pixmap, 0.0, 0.0, w, h, h / 2.0, alpha);
            if hovered {
                fill_round(pixmap, 0.0, 0.0, w, h, h / 2.0, rgba(DANGER, 0.22 * alpha));
            }
            let color = if hovered { rgba(DANGER, alpha) } else { ink };
            cross(pixmap, (w - icon) / 2.0, (h - icon) / 2.0, icon, color);
        }) {
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
    {
        let open = state.audio_open();
        let track = TRACK * s;
        let gap = 12.0 * s;
        let h = button;
        // The slider grows out to the right of the speaker, which stays
        // exactly where it was. Recentring the icon in a wider pill would
        // make it jump sideways under the cursor that just opened it.
        let w = if open {
            button + gap + track + pad
        } else {
            button
        };
        let (x, y) = (margin, fh - margin - h);

        let level = if state.muted { 0.0 } else { state.volume as f32 };
        let muted = state.muted;

        if let Some(p) = cluster(x, y, w, h, |pixmap| {
            panel(pixmap, 0.0, 0.0, w, h, h / 2.0, alpha);
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
            let th = 4.0 * s;
            let ty = h / 2.0 - th / 2.0;
            fill_round(pixmap, tx, ty, track, th, th / 2.0, rgba(CREAM, 0.22 * alpha));
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
                6.0 * s,
                rgba(CREAM, alpha),
            );
        }) {
            panels.push(p);
            hits.push(Hit {
                control: Control::Mute,
                x,
                y,
                w: button,
                h,
            });
            if open {
                hits.push(Hit {
                    control: Control::VolumeTrack,
                    x: x + button + gap,
                    y,
                    w: track,
                    h,
                });
            }
        }
    }

    // --- view, bottom-right -------------------------------------------------
    // Fullscreen, where every video player puts it.
    {
        let (w, h) = (button, button);
        let (x, y) = (fw - margin - w, fh - margin - h);
        let exiting = state.fullscreen;
        let hovered = state.hot == Some(Control::Fullscreen);
        if let Some(p) = cluster(x, y, w, h, |pixmap| {
            panel(pixmap, 0.0, 0.0, w, h, h / 2.0, alpha);
            if hovered {
                fill_round(pixmap, 0.0, 0.0, w, h, h / 2.0, rgba(CREAM, 0.10 * alpha));
            }
            expand(
                pixmap,
                (w - icon) / 2.0,
                (h - icon) / 2.0,
                icon,
                exiting,
                rgba(CREAM, hot_alpha(Control::Fullscreen, 0.85) * alpha),
            );
        }) {
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
        let (w, h) = (panel.pixmap.width(), panel.pixmap.height());
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
                w,
                h,
            )
            .ok()?;
        }

        rectangles.push(gst_video::VideoOverlayRectangle::new_raw(
            &buffer,
            panel.x.round() as i32,
            panel.y.round() as i32,
            w,
            h,
            gst_video::VideoOverlayFormatFlags::PREMULTIPLIED_ALPHA,
        ));
    }

    gst_video::VideoOverlayComposition::new(rectangles.iter()).ok()
}

/// Bind an `overlaycomposition` element to shared overlay state.
///
/// Both the real viewer and the design harness call this, so what you see
/// while iterating on the layout is what a viewer actually gets.
pub fn attach(composition: &gst::Element, overlay: &SharedOverlay) {
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
                if let Ok(mut state) = state.lock() {
                    let first = state.video == (0, 0);
                    state.video = (w.max(0) as u32, h.max(0) as u32);
                    if fps.is_some() {
                        state.fps = fps;
                    }
                    // Show the controls once, on the first frame. A viewer who
                    // never happens to move the mouse would otherwise have no
                    // way to learn they exist.
                    if first && state.video != (0, 0) {
                        state.wake();
                    }
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
