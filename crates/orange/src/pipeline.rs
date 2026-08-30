//! The capture and encode pipeline.
//!
//! The default cross-vendor path uses Media Foundation H.265 encoding. The
//! original NVIDIA AV1 path remains available for development and comparison.
//!
//! The property that makes it cheap is that frames never leave VRAM.
//! `d3d11screencapturesrc` outputs `memory:D3D11Memory`, `d3d11convert` scales
//! and converts on the GPU, and the hardware encoder consumes D3D11 textures.
//! Inserting anything that forces a download to system memory (`videoconvert`,
//! `videoscale`, most CPU filters) would destroy the performance profile.

use anyhow::{bail, Context, Result};
use gst::prelude::*;
use gstreamer as gst;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Codec {
    Av1,
    H265,
    H264,
}

impl Codec {
    /// The selected hardware encoders accept the D3D11 capture path without a
    /// CPU conversion step.
    pub(crate) fn encoder(&self) -> &'static str {
        match self {
            Codec::Av1 => "nvd3d11av1enc",
            Codec::H265 => "mfh265enc",
            Codec::H264 => "mfh264enc",
        }
    }

    pub(crate) fn parser(&self) -> &'static str {
        match self {
            Codec::Av1 => "av1parse",
            Codec::H265 => "h265parse",
            Codec::H264 => "h264parse",
        }
    }

    pub fn parse(s: &str) -> Result<Self> {
        match s.to_ascii_lowercase().as_str() {
            "av1" => Ok(Codec::Av1),
            "h265" | "hevc" => Ok(Codec::H265),
            "h264" => Ok(Codec::H264),
            other => bail!("unknown codec '{other}' (expected av1, h265 or h264)"),
        }
    }

    pub(crate) fn rtp_encoding(self) -> &'static str {
        match self {
            Codec::Av1 => "AV1",
            Codec::H265 => "H265",
            Codec::H264 => "H264",
        }
    }

    pub(crate) fn payloader(self) -> &'static str {
        match self {
            Codec::Av1 => "rtpav1pay",
            Codec::H265 => "rtph265pay",
            Codec::H264 => "rtph264pay",
        }
    }

    pub(crate) fn depayloader(self) -> &'static str {
        match self {
            Codec::Av1 => "rtpav1depay",
            Codec::H265 => "rtph265depay",
            Codec::H264 => "rtph264depay",
        }
    }

    pub(crate) fn decoder(self) -> &'static str {
        match self {
            Codec::Av1 => "d3d11av1dec",
            Codec::H265 => "d3d11h265dec",
            Codec::H264 => "d3d11h264dec",
        }
    }

    pub(crate) fn from_rtp_encoding(encoding: &str) -> Option<Self> {
        match encoding {
            "AV1" => Some(Self::Av1),
            "H265" => Some(Self::H265),
            "H264" => Some(Self::H264),
            _ => None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct CaptureSettings {
    pub hwnd: isize,
    pub codec: Codec,
    /// Kilobits per second.
    pub bitrate: u32,
    pub fps: u32,
    /// Downscale on the GPU. `None` keeps the window's native size.
    pub scale: Option<(u32, u32)>,
    /// Capture audio from this process only. `None` disables audio.
    pub audio_pid: Option<u32>,
}

impl Default for CaptureSettings {
    fn default() -> Self {
        Self {
            hwnd: 0,
            codec: Codec::H265,
            bitrate: 30_000,
            fps: 60,
            scale: None,
            audio_pid: None,
        }
    }
}

/// Audio capture, scoped to one process.
///
/// `loopback-target-pid` with `include-process-tree` records only the game and
/// its children, so voice chat, music and notification sounds never reach the
/// stream. Capturing the whole output device would pick all of that up.
///
/// A pid of zero means whole-screen sharing, where capturing everything the
/// machine plays is the expected behaviour.
///
/// Opus at 128 kbps stereo is transparent enough for games and is a rounding
/// error next to the video bitrate.
pub fn build_audio_chain(pid: u32) -> String {
    let scope = if pid == 0 {
        String::new()
    } else {
        format!("loopback-mode=include-process-tree loopback-target-pid={pid} ")
    };
    format!(
        "wasapi2src loopback=true {scope}\
         loopback-silence-on-device-mute=true buffer-time=100000 latency-time=20000 \
         ! queue max-size-buffers=10 leaky=downstream \
         ! audioconvert ! audioresample \
         ! audio/x-raw,format=S16LE,rate=48000,channels=2,layout=interleaved \
         ! opusenc bitrate=128000 frame-size=10 \
         ! rtpopuspay"
    )
}

/// Verify the audio elements exist before trying to build the chain.
pub fn check_audio_elements() -> Result<()> {
    let required = [
        "wasapi2src",
        "audioconvert",
        "audioresample",
        "opusenc",
        "rtpopuspay",
    ];
    let missing: Vec<_> = required
        .iter()
        .filter(|name| gst::ElementFactory::find(name).is_none())
        .collect();
    if !missing.is_empty() {
        bail!(
            "missing audio elements: {}",
            missing
                .iter()
                .map(|s| s.to_string())
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
    Ok(())
}

/// Verify the elements we depend on are actually present, and say precisely
/// which one is missing rather than failing with an opaque link error.
pub fn check_elements(codec: Codec) -> Result<()> {
    let required = [
        "d3d11screencapturesrc",
        "d3d11convert",
        codec.encoder(),
        codec.parser(),
    ];
    let missing: Vec<_> = required
        .iter()
        .filter(|name| gst::ElementFactory::find(name).is_none())
        .collect();

    if !missing.is_empty() {
        bail!(
            "missing GStreamer elements: {}. Is the graphics driver and matching hardware encoder available?",
            missing
                .iter()
                .map(|s| s.to_string())
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
    Ok(())
}

/// Build the GPU-resident half of the pipeline, up to and including the parser.
/// The caller appends a sink: a muxer for recording, or WebRTC for streaming.
///
/// A zero window handle means the whole screen. `d3d11screencapturesrc` takes
/// either a `window-handle` or a `monitor-index`, so the two cases differ only
/// in that property.
///
/// The framerate caps sit directly after the source so the capture element
/// knows the rate to run at; `d3d11convert` does not do framerate conversion,
/// so requesting it further downstream would fail to negotiate.
pub fn build_capture_chain(settings: &CaptureSettings) -> String {
    let scale_caps = match settings.scale {
        Some((w, h)) => format!("! video/x-raw(memory:D3D11Memory),width={w},height={h} "),
        None => String::new(),
    };

    let source = if settings.hwnd == 0 {
        "d3d11screencapturesrc capture-api=wgc monitor-index=0 show-cursor=true".to_string()
    } else {
        format!(
            "d3d11screencapturesrc window-handle={} capture-api=wgc \
             window-capture-mode=client show-cursor=false",
            settings.hwnd
        )
    };
    format!(
        "{source} \
         ! video/x-raw(memory:D3D11Memory),framerate={fps}/1 \
         ! queue max-size-buffers=3 leaky=downstream \
         ! d3d11convert \
         {scale_caps}\
         ! {encoder} name=stream-encoder bitrate={bitrate} \
         ! {parser}",
        fps = settings.fps,
        encoder = settings.codec.encoder(),
        bitrate = settings.bitrate,
        parser = settings.codec.parser(),
    )
}

fn gop_size(fps: u32) -> i32 {
    fps.saturating_mul(2).min(i32::MAX as u32) as i32
}

fn set_if_supported(element: &gst::Element, property: &str, value: impl Into<gst::glib::Value>) {
    if element.find_property(property).is_some() {
        element.set_property(property, value);
    }
}

fn set_from_str_if_supported(element: &gst::Element, property: &str, value: &str) {
    if element.find_property(property).is_some() {
        element.set_property_from_str(property, value);
    }
}

pub fn configure_encoder(element: &gst::Element, codec: Codec, fps: u32) {
    set_encoder_gop(element, gop_size(fps) as u32);
    match codec {
        Codec::H264 | Codec::H265 => {
            set_if_supported(element, "low-latency", true);
            set_from_str_if_supported(element, "rc-mode", "cbr");
            set_if_supported(element, "quality-vs-speed", 50u32);
        }
        Codec::Av1 => {
            set_from_str_if_supported(element, "preset", "p5");
            set_from_str_if_supported(element, "tune", "low-latency");
            set_from_str_if_supported(element, "rc-mode", "cbr");
            set_if_supported(element, "spatial-aq", true);
        }
    }
}

pub fn set_encoder_gop(element: &gst::Element, frames: u32) {
    set_if_supported(element, "gop-size", frames.min(i32::MAX as u32) as i32);
}

/// Record to a file. Primarily a diagnostic: it exercises the exact capture and
/// encode path that streaming will use, without any network involved.
///
/// The sink is built programmatically rather than parsed from a string.
/// GStreamer's parse syntax treats backslashes as escapes, so interpolating a
/// Windows path into a pipeline description silently mangles it.
pub fn build_record_pipeline(settings: &CaptureSettings, output: &str) -> Result<gst::Pipeline> {
    check_elements(settings.codec)?;

    let pipeline = gst::Pipeline::new();

    let capture = gst::parse::bin_from_description(&build_capture_chain(settings), true)
        .context("failed to build capture chain")?;
    let encoder = capture
        .by_name("stream-encoder")
        .context("capture chain has no named encoder")?;
    configure_encoder(&encoder, settings.codec, settings.fps);
    let muxer = gst::ElementFactory::make("matroskamux")
        .build()
        .context("matroskamux missing")?;
    let sink = gst::ElementFactory::make("filesink")
        .property("location", output)
        .build()
        .context("filesink missing")?;

    pipeline.add_many([capture.upcast_ref(), &muxer, &sink])?;
    gst::Element::link_many([capture.upcast_ref(), &muxer, &sink])
        .context("failed to link capture chain to file sink")?;

    Ok(pipeline)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn streaming_encoder_uses_a_bounded_recovery_gop() {
        let settings = CaptureSettings {
            fps: 60,
            ..CaptureSettings::default()
        };

        assert_eq!(gop_size(settings.fps), 120);
    }

    #[test]
    fn recording_gop_tracks_high_refresh_input() {
        let settings = CaptureSettings {
            fps: 240,
            ..CaptureSettings::default()
        };

        assert_eq!(gop_size(settings.fps), 480);
    }

    #[test]
    fn streaming_encoder_uses_consistent_low_latency_quality_settings() {
        gst::init().unwrap();
        let encoder = gst::ElementFactory::make("nvd3d11av1enc").build().unwrap();
        configure_encoder(&encoder, Codec::Av1, 60);

        assert_eq!(encoder.property::<i32>("gop-size"), 120);
        assert!(encoder.property::<bool>("spatial-aq"));
    }

    #[test]
    fn default_streaming_codec_uses_cross_vendor_h265() {
        let settings = CaptureSettings::default();
        let chain = build_capture_chain(&settings);

        assert_eq!(settings.codec, Codec::H265);
        assert!(chain.contains("mfh265enc"));
        assert!(chain.contains("h265parse"));
        assert!(!chain.contains("low-latency="));
        assert!(!chain.contains("quality-vs-speed="));
    }

    #[test]
    fn audio_payload_is_forced_to_the_advertised_stereo_format() {
        let chain = build_audio_chain(42);

        assert!(chain.contains("audio/x-raw,format=S16LE,rate=48000,channels=2,layout=interleaved"));
        assert!(!chain.contains("inband-fec=true"));
        assert!(chain.contains("buffer-time=100000 latency-time=20000"));
    }

    #[test]
    fn audio_preflight_covers_every_chain_element() {
        let chain = build_audio_chain(42);
        for element in [
            "wasapi2src",
            "audioconvert",
            "audioresample",
            "opusenc",
            "rtpopuspay",
        ] {
            assert!(chain.contains(element), "audio chain omitted {element}");
        }
    }
}
