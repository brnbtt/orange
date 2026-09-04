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

use gstreamer_video as gst_video;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

mod gst;
mod raster;

pub(crate) use gst::attach;

/// How long the controls stay up after the last mouse movement.
const HIDE_AFTER: Duration = Duration::from_millis(1_000);
const FADE: Duration = Duration::from_millis(200);
/// One breath of the live dot. The client's status dot uses the same period, so
/// a host with both windows open sees one rhythm rather than two.
const BREATH: Duration = Duration::from_millis(1_600);

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
    connection: Arc<crate::connection::ConnectionTracker>,
    profile: crate::window::PlaybackProfile,
    shown_at: Instant,
    /// Fixed for the life of the overlay, so the live dot's breath is
    /// continuous rather than restarting whenever the controls wake.
    born: Instant,
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
    #[cfg(test)]
    pub fn new(profile: crate::window::PlaybackProfile) -> Self {
        Self::with_connection(
            profile,
            Arc::new(crate::connection::ConnectionTracker::default()),
        )
    }

    pub(crate) fn with_connection(
        profile: crate::window::PlaybackProfile,
        connection: Arc<crate::connection::ConnectionTracker>,
    ) -> Self {
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
            connection,
            profile,
            // Start hidden; pointer activity or the first known source size
            // briefly reveals the controls.
            shown_at: Instant::now() - HIDE_AFTER * 2,
            born: Instant::now(),
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

    /// One breath of the live dot, as an alpha multiplier.
    ///
    /// Never reaches zero. A dot that blinks fully off reads as a fault light;
    /// this is a slow swell that says the picture is arriving now rather than
    /// being a label that happens to be red.
    ///
    /// Driven from a fixed birth instant rather than `shown_at`, which resets
    /// on every mouse move and would restart the breath mid-swell.
    pub(super) fn live_pulse(&self) -> f32 {
        let phase = self.born.elapsed().as_secs_f32() / BREATH.as_secs_f32();
        let wave = 0.5 - 0.5 * (phase * std::f32::consts::TAU).cos();
        0.45 + 0.55 * wave
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
        if let Some(stage) = self
            .connection
            .snapshot()
            .filter(|stage| !stage.is_connected())
        {
            return stage.copy().title.to_string();
        }
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
        if let Some(stage) = self
            .connection
            .snapshot()
            .filter(|stage| !stage.is_connected())
        {
            return Some(stage.copy().detail.to_string());
        }
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
        // Only while the dot is actually on screen. Hashing the breath
        // unconditionally would re-rasterise the whole overlay a dozen times a
        // second behind a hidden control set, which is what the cache is for.
        if self.visible() || self.profile.persistent_live_status() {
            ((self.live_pulse() * 20.0) as u32).hash(&mut hasher);
        }
        hasher.finish()
    }
}

pub type SharedOverlay = Arc<Mutex<OverlayState>>;
