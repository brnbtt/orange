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
use tiny_skia::{Color, FillRule, Paint, PathBuilder, Pixmap, PixmapPaint, Stroke, Transform};

use crate::text::{self, Weight};

/// How long the controls stay up after the last mouse movement.
const HIDE_AFTER: Duration = Duration::from_millis(1_000);
const FADE: Duration = Duration::from_millis(200);

// Design tokens, in logical pixels: what they measure on screen at 100%
// display scaling, whatever the stream resolution.
const MARGIN: f32 = 18.0;
const BUTTON: f32 = 40.0;
const ICON: f32 = 19.0;
const CHIP: f32 = 34.0;
const PAD: f32 = 13.0;
const TRACK: f32 = 110.0;
const LABEL: f32 = 13.0;
const CONTROL_RADIUS: f32 = 10.0;

// The app's palette, matching the tray.
const SURFACE: (f32, f32, f32) = (0.086, 0.086, 0.086);
const BORDER: (f32, f32, f32) = (0.165, 0.165, 0.165);
const CREAM: (f32, f32, f32) = (0.902, 0.878, 0.820);
const ORANGE: (f32, f32, f32) = (1.0, 0.353, 0.122);
const DANGER: (f32, f32, f32) = (0.878, 0.392, 0.373);

const ICON_SPEAKER_HIGH: &str = "M155.51,24.81a8,8,0,0,0-8.42.88L77.25,80H32A16,16,0,0,0,16,96v64a16,16,0,0,0,16,16H77.25l69.84,54.31A8,8,0,0,0,160,224V32A8,8,0,0,0,155.51,24.81ZM32,96H72v64H32ZM144,207.64,88,164.09V91.91l56-43.55Zm54-106.08a40,40,0,0,1,0,52.88,8,8,0,0,1-12-10.58,24,24,0,0,0,0-31.72,8,8,0,0,1,12-10.58ZM248,128a79.9,79.9,0,0,1-20.37,53.34,8,8,0,0,1-11.92-10.67,64,64,0,0,0,0-85.33,8,8,0,1,1,11.92-10.67A79.83,79.83,0,0,1,248,128Z";
const ICON_SPEAKER_SLASH: &str = "M53.92,34.62A8,8,0,1,0,42.08,45.38L73.55,80H32A16,16,0,0,0,16,96v64a16,16,0,0,0,16,16H77.25l69.84,54.31A8,8,0,0,0,160,224V175.09l42.08,46.29a8,8,0,1,0,11.84-10.76ZM32,96H72v64H32ZM144,207.64,88,164.09V95.89l56,61.6Zm42-63.77a24,24,0,0,0,0-31.72,8,8,0,1,1,12-10.57,40,40,0,0,1,0,52.88,8,8,0,0,1-12-10.59Zm-80.16-76a8,8,0,0,1,1.4-11.23l39.85-31A8,8,0,0,1,160,32v74.83a8,8,0,0,1-16,0V48.36l-26.94,21A8,8,0,0,1,105.84,67.91ZM248,128a79.9,79.9,0,0,1-20.37,53.34,8,8,0,0,1-11.92-10.67,64,64,0,0,0,0-85.33,8,8,0,1,1,11.92-10.67A79.83,79.83,0,0,1,248,128Z";
const ICON_X: &str = "M205.66,194.34a8,8,0,0,1-11.32,11.32L128,139.31,61.66,205.66a8,8,0,0,1-11.32-11.32L116.69,128,50.34,61.66A8,8,0,0,1,61.66,50.34L128,116.69l66.34-66.35a8,8,0,0,1,11.32,11.32L139.31,128Z";
const ICON_ARROWS_OUT: &str = "M216,48V96a8,8,0,0,1-16,0V67.31l-42.34,42.35a8,8,0,0,1-11.32-11.32L188.69,56H160a8,8,0,0,1,0-16h48A8,8,0,0,1,216,48ZM98.34,146.34,56,188.69V160a8,8,0,0,0-16,0v48a8,8,0,0,0,8,8H96a8,8,0,0,0,0-16H67.31l42.35-42.34a8,8,0,0,0-11.32-11.32ZM208,152a8,8,0,0,0-8,8v28.69l-42.34-42.35a8,8,0,0,0-11.32,11.32L188.69,200H160a8,8,0,0,0,0,16h48a8,8,0,0,0,8-8V160A8,8,0,0,0,208,152ZM67.31,56H96a8,8,0,0,0,0-16H48a8,8,0,0,0-8,8V96a8,8,0,0,0,16,0V67.31l42.34,42.35a8,8,0,0,0,11.32-11.32Z";
const ICON_ARROWS_IN: &str = "M144,104V64a8,8,0,0,1,16,0V84.69l42.34-42.35a8,8,0,0,1,11.32,11.32L171.31,96H192a8,8,0,0,1,0,16H152A8,8,0,0,1,144,104Zm-40,40H64a8,8,0,0,0,0,16H84.69L42.34,202.34a8,8,0,0,0,11.32,11.32L96,171.31V192a8,8,0,0,0,16,0V152A8,8,0,0,0,104,144Zm67.31,16H192a8,8,0,0,0,0-16H152a8,8,0,0,0-8,8v40a8,8,0,0,0,16,0V171.31l42.34,42.35a8,8,0,0,0,11.32-11.32ZM104,56a8,8,0,0,0-8,8V84.69L53.66,42.34A8,8,0,0,0,42.34,53.66L84.69,96H64a8,8,0,0,0,0,16h40a8,8,0,0,0,8-8V64A8,8,0,0,0,104,56Z";

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
    profile: crate::window::PlaybackProfile,
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

impl OverlayState {
    pub fn new(profile: crate::window::PlaybackProfile) -> Self {
        Self {
            video: (0, 0),
            client: (0, 0),
            dpi: 1.0,
            volume: profile.initial_volume(),
            muted: profile.starts_muted(),
            volume_dragging: false,
            fullscreen: false,
            viewers: None,
            host: None,
            fps: None,
            bitrate_kbps: None,
            profile,
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
        self.profile.hash(&mut hasher);
        self.hot.map(|c| c as u8).hash(&mut hasher);
        self.quality_label().hash(&mut hasher);
        self.detail_label().hash(&mut hasher);
        hasher.finish()
    }
}

pub type SharedOverlay = Arc<Mutex<OverlayState>>;

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

fn status_text_origin(height: f32, gap: f32, scale: f32) -> f32 {
    (height + gap) * scale
}

fn audio_track_width(logical_width: f32) -> f32 {
    (logical_width - (MARGIN * 2.0 + BUTTON + 12.0 + PAD)).clamp(0.0, TRACK)
}

fn audio_gap_width(open: bool, gap: f32, track: f32, logical_width: f32) -> f32 {
    if open {
        gap + track + PAD
    } else {
        (logical_width - MARGIN * 2.0 - BUTTON * 2.0).clamp(0.0, gap + 24.0)
    }
}

fn status_expanded(hovered: bool) -> bool {
    hovered
}

fn status_max_width(logical_width: f32) -> f32 {
    (logical_width - MARGIN * 2.0 - BUTTON - 8.0).max(0.0)
}

fn status_leading_width(persistent_live: bool, height: f32) -> f32 {
    if persistent_live {
        20.0
    } else {
        height
    }
}

/// The panel every cluster sits on.
///
/// Dark enough that white sits on it cleanly over white video - the scrim has
/// to survive the worst case, not the average one - but small enough that
/// being nearly opaque hides almost nothing.
fn panel(pixmap: &mut Pixmap, x: f32, y: f32, w: f32, h: f32, r: f32, alpha: f32) {
    fill_round(pixmap, x, y, w, h, r, rgba(SURFACE, 0.94 * alpha));
    if let Some(path) = rounded_rect(x + 0.5, y + 0.5, w - 1.0, h - 1.0, r) {
        stroke(pixmap, &path, rgba(BORDER, 0.92 * alpha), 1.0);
    }
}

// --- icons ------------------------------------------------------------------

fn draw_svg_icon(pixmap: &mut Pixmap, x: f32, y: f32, s: f32, path: &str, color: Color) {
    let dimension = s.ceil().max(1.0) as u32;
    let to_byte = |channel: f32| (channel * 255.0).round().clamp(0.0, 255.0) as u8;
    let svg = format!(
        r##"<svg xmlns="http://www.w3.org/2000/svg" width="{dimension}" height="{dimension}" viewBox="0 0 256 256" fill="#{:02x}{:02x}{:02x}" fill-opacity="{}"><path d="{path}"/></svg>"##,
        to_byte(color.red()),
        to_byte(color.green()),
        to_byte(color.blue()),
        color.alpha()
    );
    let Ok(tree) = resvg::usvg::Tree::from_str(&svg, &resvg::usvg::Options::default()) else {
        return;
    };
    let Some(mut icon) = Pixmap::new(dimension, dimension) else {
        return;
    };
    resvg::render(&tree, Transform::identity(), &mut icon.as_mut());
    pixmap.draw_pixmap(
        x.round() as i32,
        y.round() as i32,
        icon.as_ref(),
        &PixmapPaint::default(),
        Transform::identity(),
        None,
    );
}

fn speaker(pixmap: &mut Pixmap, x: f32, y: f32, s: f32, muted: bool, color: Color) {
    draw_svg_icon(
        pixmap,
        x,
        y,
        s,
        if muted {
            ICON_SPEAKER_SLASH
        } else {
            ICON_SPEAKER_HIGH
        },
        color,
    );
}

fn cross(pixmap: &mut Pixmap, x: f32, y: f32, s: f32, color: Color) {
    draw_svg_icon(pixmap, x, y, s, ICON_X, color);
}

fn live_mark(pixmap: &mut Pixmap, x: f32, y: f32, s: f32, color: Color) {
    circle(pixmap, x + s / 2.0, y + s / 2.0, s * 0.24, color);
}

fn expand(pixmap: &mut Pixmap, x: f32, y: f32, s: f32, exiting: bool, color: Color) {
    draw_svg_icon(
        pixmap,
        x,
        y,
        s,
        if exiting {
            ICON_ARROWS_IN
        } else {
            ICON_ARROWS_OUT
        },
        color,
    );
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
    let persistent_live = state.profile.persistent_live_status();
    if !state.visible() && !persistent_live {
        let composition = transparent_composition()?;
        state.cache = Some((signature, composition.clone()));
        return Some(composition);
    }

    let alpha = state.opacity();
    let status_alpha = if persistent_live { 1.0 } else { alpha };
    // Placement is in video coordinates; painting is at the output's physical
    // DPI. If both use video scale, tiny-skia's antialiasing is filtered again
    // when the sink fits the stream to the window, which softens every icon.
    let render_scale = state.scale();
    let raster_scale = state.dpi.max(1.0);
    let (fw, fh) = (vw as f32, vh as f32);
    let logical_width = fw / render_scale;

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
    // A compact stream mark at rest; hovering expands into real receive data.
    {
        let expanded = status_expanded(state.hot == Some(Control::Stats));
        let received = state.quality_label();
        let quality = if persistent_live {
            String::from("LIVE")
        } else {
            received.clone()
        };
        let mut detail = if expanded {
            let detail = state.detail_label();
            if persistent_live {
                Some(match detail {
                    Some(detail) => format!("{received}  \u{00b7}  {detail}"),
                    None => received.clone(),
                })
            } else {
                detail
            }
        } else {
            None
        };
        // Logical dimensions first; each is independently converted for the
        // destination rectangle and for the source pixmap.
        let h = if persistent_live { 28.0 } else { CHIP };
        let leading = status_leading_width(persistent_live, h);
        let has_text = text::available();
        let show_label = (expanded || persistent_live) && has_text;
        let label_gap = if persistent_live { 0.0 } else { 8.0 };
        let end_pad = if persistent_live { 10.0 } else { PAD };
        let max_w = status_max_width(logical_width).max(h);
        let label_size = LABEL * raster_scale;
        let measure = |detail: Option<&String>| {
            let mut width = text::width(&quality, label_size, Weight::Semibold) / raster_scale;
            if let Some(detail) = detail {
                width += text::width(
                    &format!("  \u{00b7}  {detail}"),
                    label_size,
                    Weight::Regular,
                ) / raster_scale;
            }
            width
        };
        let mut text_w = measure(detail.as_ref());
        if expanded && persistent_live && leading + label_gap + text_w + end_pad > max_w {
            detail = Some(received);
            text_w = measure(detail.as_ref());
        }
        if expanded && leading + label_gap + text_w + end_pad > max_w {
            detail = None;
            text_w = measure(None);
        }
        let w = if show_label {
            (leading + label_gap + text_w + end_pad).min(max_w)
        } else {
            h
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
                let (raster_w, raster_h) = (w * raster_scale, h * raster_scale);
                panel(
                    pixmap,
                    0.0,
                    0.0,
                    raster_w,
                    raster_h,
                    if persistent_live { 8.0 } else { CONTROL_RADIUS } * raster_scale,
                    status_alpha,
                );
                if persistent_live {
                    circle(
                        pixmap,
                        10.0 * raster_scale,
                        raster_h / 2.0,
                        3.0 * raster_scale,
                        rgba(ORANGE, status_alpha),
                    );
                } else {
                    let icon = ICON * raster_scale;
                    live_mark(
                        pixmap,
                        (raster_h - icon) / 2.0,
                        (raster_h - icon) / 2.0,
                        icon,
                        rgba(ORANGE, status_alpha),
                    );
                }
                if !show_label {
                    return;
                }
                let baseline = raster_h / 2.0 + text::cap_height(label_size) / 2.0;
                let mut caret = status_text_origin(leading, label_gap, raster_scale);
                text::draw(
                    pixmap,
                    caret,
                    baseline,
                    &quality,
                    label_size,
                    Weight::Semibold,
                    if persistent_live {
                        rgba(ORANGE, status_alpha)
                    } else {
                        status_ink
                    },
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
    if alpha > 0.0 {
        let track = audio_track_width(logical_width);
        let open = state.audio_open() && track >= 36.0;
        let gap = 12.0;
        let h = BUTTON;
        // The slider grows out to the right of the speaker, which stays
        // exactly where it was. Recentring the icon in a wider pill would
        // make it jump sideways under the cursor that just opened it.
        let w = if open {
            BUTTON + gap + track + PAD
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
                let track = track * raster_scale;
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
                    w: track * render_scale,
                    h: h * render_scale,
                });
            }
            let gap_width = audio_gap_width(open, gap, track, logical_width);
            if gap_width > 0.0 {
                hits.push(Hit {
                    control: Control::AudioGap,
                    x: x + BUTTON * render_scale,
                    y,
                    w: gap_width * render_scale,
                    h: h * render_scale,
                });
            }
        }
    }

    // --- view, bottom-right -------------------------------------------------
    // Fullscreen, where every video player puts it.
    let audio_displaces_fullscreen =
        state.audio_open() && logical_width < MARGIN * 2.0 + BUTTON * 2.0 + 12.0 + PAD + TRACK;
    if alpha > 0.0 && !audio_displaces_fullscreen {
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

    if !state.visible() && persistent_live {
        hits.extend(
            state
                .hits
                .iter()
                .copied()
                .filter(|hit| hit.control != Control::Stats),
        );
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
pub fn attach(composition: &gst::Element, playback: &crate::window::PlaybackWindow) {
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
        let mut state = state.lock().ok()?;
        render(&mut state).map(|c| c.to_value())
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_text_origin_scales_logical_height_once() {
        assert_eq!(status_text_origin(30.0, 4.0, 1.5), 51.0);
        assert_eq!(status_text_origin(34.0, 8.0, 2.0), 84.0);
    }

    #[test]
    fn narrow_audio_layout_reserves_room_for_a_useful_slider() {
        assert_eq!(audio_track_width(480.0), 110.0);
        assert_eq!(audio_track_width(152.0), 51.0);
    }

    #[test]
    fn closed_audio_control_has_no_invisible_slider_hit_region() {
        assert_eq!(audio_gap_width(false, 12.0, 110.0, 152.0), 36.0);
        assert_eq!(audio_gap_width(true, 12.0, 51.0, 152.0), 76.0);
    }

    #[test]
    fn faded_controls_keep_their_hit_geometry() {
        gst::init().unwrap();
        let mut state =
            OverlayState::new(crate::window::PlaybackProfile::FriendViewer { cascade: 0 });
        state.video = (1920, 1080);
        state.client = (1280, 720);
        state.pinned = true;
        render(&mut state).unwrap();
        assert!(!state.hits.is_empty());

        state.pinned = false;
        state.shown_at = Instant::now() - HIDE_AFTER * 2;
        render(&mut state).unwrap();

        assert!(!state.hits.is_empty());
    }

    #[test]
    fn live_status_expands_on_hover() {
        assert!(status_expanded(true));
        assert!(!status_expanded(false));
    }

    #[test]
    fn live_status_uses_a_compact_typographic_lead() {
        assert_eq!(status_leading_width(true, 28.0), 20.0);
        assert_eq!(status_leading_width(false, 34.0), 34.0);
    }

    #[test]
    fn live_monitor_keeps_all_control_hits_while_faded() {
        gst::init().unwrap();
        let mut state = OverlayState::new(crate::window::PlaybackProfile::LiveMonitor);
        state.video = (1406, 1541);
        state.client = (246, 270);
        state.pinned = true;
        render(&mut state).unwrap();
        assert!(state.hits.iter().any(|hit| hit.control == Control::Close));
        assert!(state.hits.iter().any(|hit| hit.control == Control::Mute));
        assert!(state
            .hits
            .iter()
            .any(|hit| hit.control == Control::Fullscreen));

        state.pinned = false;
        state.shown_at = Instant::now() - HIDE_AFTER * 2;
        render(&mut state).unwrap();

        assert!(state.hits.iter().any(|hit| hit.control == Control::Close));
        assert!(state.hits.iter().any(|hit| hit.control == Control::Mute));
        assert!(state
            .hits
            .iter()
            .any(|hit| hit.control == Control::Fullscreen));
    }

    #[test]
    fn status_width_stops_before_the_close_control() {
        assert_eq!(status_max_width(480.0), 396.0);
        assert_eq!(status_max_width(152.0), 68.0);
    }

    #[test]
    fn canonical_overlay_icons_render_visible_pixels() {
        for path in [
            ICON_SPEAKER_HIGH,
            ICON_SPEAKER_SLASH,
            ICON_X,
            ICON_ARROWS_OUT,
            ICON_ARROWS_IN,
        ] {
            let mut pixmap = Pixmap::new(24, 24).unwrap();
            draw_svg_icon(&mut pixmap, 2.0, 2.0, 20.0, path, rgba(CREAM, 1.0));
            assert!(pixmap.data().chunks_exact(4).any(|pixel| pixel[3] > 0));
        }
    }
}
