//! The capture and encode pipeline.
//!
//! This is the pipeline validated by measurement on an RTX 4080 SUPER: 4K60
//! AV1 at ~29 Mbps cost roughly 9% of a single CPU core and no measurable
//! in-game FPS.
//!
//! The property that makes it cheap is that frames never leave VRAM.
//! `d3d11screencapturesrc` outputs `memory:D3D11Memory`, `d3d11convert` scales
//! and converts on the GPU, and `nvd3d11*enc` consumes D3D11 textures directly.
//! Inserting anything that forces a download to system memory (`videoconvert`,
//! `videoscale`, most CPU filters) would destroy the performance profile.

use anyhow::{bail, Context, Result};
use gstreamer as gst;
use gst::prelude::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Codec {
    Av1,
    H265,
    H264,
}

impl Codec {
    /// D3D11-mode encoders keep the frame on the GPU. The CUDA-mode variants
    /// (`nvav1enc`, `nvh264enc`) would work but involve extra copies.
    fn encoder(&self) -> &'static str {
        match self {
            Codec::Av1 => "nvd3d11av1enc",
            Codec::H265 => "nvd3d11h265enc",
            Codec::H264 => "nvd3d11h264enc",
        }
    }

    fn parser(&self) -> &'static str {
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
            codec: Codec::Av1,
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
         loopback-silence-on-device-mute=true low-latency=true \
         ! queue max-size-buffers=10 leaky=downstream \
         ! audioconvert ! audioresample \
         ! opusenc bitrate=128000 frame-size=10 \
         ! rtpopuspay"
    )
}

/// Verify the audio elements exist before trying to build the chain.
pub fn check_audio_elements() -> Result<()> {
    let required = ["wasapi2src", "audioconvert", "opusenc", "rtpopuspay"];
    let missing: Vec<_> = required
        .iter()
        .filter(|name| gst::ElementFactory::find(name).is_none())
        .collect();
    if !missing.is_empty() {
        bail!(
            "missing audio elements: {}",
            missing.iter().map(|s| s.to_string()).collect::<Vec<_>>().join(", ")
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
            "missing GStreamer elements: {}. Is the NVIDIA plugin (nvcodec) available?",
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
    let gop_size = settings.fps.saturating_mul(2).min(i32::MAX as u32);
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
         ! {encoder} bitrate={bitrate} gop-size={gop_size} \
           preset=p5 tune=low-latency rc-mode=cbr spatial-aq=true \
         ! {parser}",
        fps = settings.fps,
        encoder = settings.codec.encoder(),
        bitrate = settings.bitrate,
        gop_size = gop_size,
        parser = settings.codec.parser(),
    )
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

        assert!(build_capture_chain(&settings).contains("gop-size=120"));
    }

    #[test]
    fn streaming_encoder_uses_consistent_low_latency_quality_settings() {
        let chain = build_capture_chain(&CaptureSettings::default());

        assert!(chain.contains("preset=p5"));
        assert!(chain.contains("tune=low-latency"));
        assert!(chain.contains("rc-mode=cbr"));
        assert!(chain.contains("spatial-aq=true"));
    }
}
