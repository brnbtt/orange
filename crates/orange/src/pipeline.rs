//! The capture and encode pipeline.
//!
//! The tray prefers H.265 and selects the first hardware encoder that can link
//! directly to the D3D11 conversion path. Explicit CLI codec choices stay strict.
//!
//! The property that makes it cheap is that frames never leave VRAM.
//! `d3d11screencapturesrc` outputs `memory:D3D11Memory`, `d3d11convert` scales
//! and converts on the GPU, and the hardware encoder consumes D3D11 textures.
//! Inserting anything that forces a download to system memory (`videoconvert`,
//! `videoscale`, most CPU filters) would destroy the performance profile.

use anyhow::{bail, Context, Result};
use gst::prelude::*;
use gstreamer as gst;

const AUTO_ENCODERS: [(Codec, &str); 4] = [
    (Codec::H265, "mfh265enc"),
    (Codec::H265, "nvd3d11h265enc"),
    (Codec::H264, "mfh264enc"),
    (Codec::H264, "nvd3d11h264enc"),
];
const MIN_FORCE_KEY_UNIT_INTERVAL_NS: u64 = 1_000_000_000;
const REFERENCE_VIDEO_PIXELS: f64 = 1920.0 * 1080.0;
const REFERENCE_VIDEO_KBPS: f64 = 18_000.0;
const VIDEO_BITRATE_STEP_KBPS: f64 = 500.0;
const MAX_RECOMMENDED_VIDEO_KBPS: u32 = 80_000;

/// A measured good-quality ceiling for Orange's low-latency H.265 path.
pub(crate) fn recommended_video_bitrate(width: u32, height: u32, fps: u32) -> (u32, bool) {
    let pixels = f64::from(width) * f64::from(height);
    let target = REFERENCE_VIDEO_KBPS
        * (pixels / REFERENCE_VIDEO_PIXELS).powf(0.8)
        * (f64::from(fps) / 60.0);
    let rounded = (target / VIDEO_BITRATE_STEP_KBPS).round() * VIDEO_BITRATE_STEP_KBPS;
    let constrained = rounded > f64::from(MAX_RECOMMENDED_VIDEO_KBPS);
    (
        (rounded as u32).clamp(500, MAX_RECOMMENDED_VIDEO_KBPS),
        constrained,
    )
}

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
            other => bail!("unknown codec '{other}' (expected auto, av1, h265 or h264)"),
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

fn select_encoder_with(
    requested: Option<Codec>,
    mut is_compatible: impl FnMut(&str) -> bool,
) -> Result<(Codec, &'static str)> {
    if let Some(codec) = requested {
        return Ok((codec, codec.encoder()));
    }

    AUTO_ENCODERS
        .into_iter()
        .find(|(_, factory)| is_compatible(factory))
        .ok_or_else(|| {
            anyhow::anyhow!(
                "no compatible zero-copy D3D11 encoder found; attempted {}. Update the graphics driver or install a GStreamer hardware encoder plugin",
                AUTO_ENCODERS
                    .iter()
                    .map(|(_, factory)| *factory)
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        })
}

fn supports_d3d11_input(factory: &str) -> bool {
    let description = format!(
        "d3d11convert ! video/x-raw(memory:D3D11Memory),format=NV12,width=1280,height=720 ! {factory}"
    );
    let Ok(probe) = gst::parse::bin_from_description(&description, true) else {
        return false;
    };
    drop(probe);
    true
}

pub(crate) fn select_encoder(requested: Option<Codec>) -> Result<(Codec, &'static str)> {
    select_encoder_with(requested, supports_d3d11_input)
}

#[derive(Debug, Clone)]
pub struct CaptureSettings {
    pub hwnd: isize,
    pub codec: Codec,
    pub encoder: &'static str,
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
            encoder: "mfh265enc",
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
/// machine plays is the expected behaviour - with one exception. The tray plays
/// short cues when a viewer arrives or leaves, and those are meant for the
/// person hosting, not for the people watching them. `exclude_pid` carries the
/// tray's own process id so that one tree can be left out while everything else
/// is still captured.
///
/// Opus at 128 kbps stereo is transparent enough for games and is a rounding
/// error next to the video bitrate.
pub fn build_audio_chain(pid: u32, exclude_pid: Option<u32>) -> String {
    let scope = match (pid, exclude_pid) {
        (0, Some(ui)) => format!("loopback-mode=exclude-process-tree loopback-target-pid={ui} "),
        (0, None) => String::new(),
        (pid, _) => format!("loopback-mode=include-process-tree loopback-target-pid={pid} "),
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
pub fn check_elements(settings: &CaptureSettings) -> Result<()> {
    let required = [
        "d3d11screencapturesrc",
        "d3d11convert",
        settings.encoder,
        settings.codec.parser(),
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
        encoder = settings.encoder,
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

pub fn configure_encoder(element: &gst::Element, encoder: &str, codec: Codec, fps: u32) {
    set_encoder_gop(element, gop_size(fps) as u32);
    set_if_supported(
        element,
        "min-force-key-unit-interval",
        MIN_FORCE_KEY_UNIT_INTERVAL_NS,
    );
    if encoder.starts_with("nvd3d11") {
        set_from_str_if_supported(element, "preset", "p5");
        set_from_str_if_supported(element, "tune", "low-latency");
        set_from_str_if_supported(element, "rc-mode", "cbr");
        set_if_supported(element, "spatial-aq", true);
    } else if matches!(codec, Codec::H264 | Codec::H265) {
        set_if_supported(element, "low-latency", true);
        set_from_str_if_supported(element, "rc-mode", "cbr");
        set_if_supported(element, "quality-vs-speed", 50u32);
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
    check_elements(settings)?;

    let pipeline = gst::Pipeline::new();

    let capture = gst::parse::bin_from_description(&build_capture_chain(settings), true)
        .context("failed to build capture chain")?;
    let encoder = capture
        .by_name("stream-encoder")
        .context("capture chain has no named encoder")?;
    configure_encoder(&encoder, settings.encoder, settings.codec, settings.fps);
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

    fn compatible_factories<'a>(factories: &'a [&'a str]) -> impl FnMut(&str) -> bool + 'a {
        move |factory| factories.contains(&factory)
    }

    #[test]
    fn auto_prefers_compatible_media_foundation_h265() {
        let selected =
            select_encoder_with(None, compatible_factories(&["mfh265enc", "nvd3d11h265enc"]))
                .unwrap();

        assert_eq!(selected, (Codec::H265, "mfh265enc"));
    }

    #[test]
    fn auto_keeps_h265_when_nvidia_is_the_compatible_encoder() {
        let selected =
            select_encoder_with(None, compatible_factories(&["nvd3d11h265enc", "mfh264enc"]))
                .unwrap();

        assert_eq!(selected, (Codec::H265, "nvd3d11h265enc"));
    }

    #[test]
    fn auto_falls_back_to_media_foundation_h264() {
        let selected =
            select_encoder_with(None, compatible_factories(&["mfh264enc", "nvd3d11h264enc"]))
                .unwrap();

        assert_eq!(selected, (Codec::H264, "mfh264enc"));
    }

    #[test]
    fn auto_uses_nvidia_h264_as_the_last_zero_copy_option() {
        let selected =
            select_encoder_with(None, compatible_factories(&["nvd3d11h264enc"])).unwrap();

        assert_eq!(selected, (Codec::H264, "nvd3d11h264enc"));
    }

    #[test]
    fn auto_error_names_every_attempted_zero_copy_encoder() {
        let error = select_encoder_with(None, |_| false).unwrap_err();
        let message = error.to_string();

        assert!(message.contains("zero-copy"));
        for encoder in ["mfh265enc", "nvd3d11h265enc", "mfh264enc", "nvd3d11h264enc"] {
            assert!(message.contains(encoder), "error omitted {encoder}");
        }
    }

    #[test]
    fn explicit_codecs_keep_their_existing_encoder_without_fallback() {
        for (codec, encoder) in [
            (Codec::H265, "mfh265enc"),
            (Codec::H264, "mfh264enc"),
            (Codec::Av1, "nvd3d11av1enc"),
        ] {
            let selected = select_encoder_with(Some(codec), |_| {
                panic!("explicit codec must not run auto compatibility selection")
            })
            .unwrap();
            assert_eq!(selected, (codec, encoder));
        }
    }

    #[test]
    fn unknown_codec_error_lists_every_cli_choice() {
        let message = Codec::parse("vp9").unwrap_err().to_string();

        assert!(message.contains("expected auto, av1, h265 or h264"));
    }

    #[test]
    #[ignore = "requires the local Windows GStreamer hardware stack"]
    fn local_auto_probe_selects_first_compatible_candidate() {
        gst::init().unwrap();
        let expected = AUTO_ENCODERS
            .iter()
            .copied()
            .find(|(_, factory)| supports_d3d11_input(factory))
            .expect("no local zero-copy encoder is compatible");

        assert_eq!(select_encoder(None).unwrap(), expected);
    }

    #[test]
    fn streaming_encoder_uses_a_bounded_recovery_gop() {
        let settings = CaptureSettings {
            fps: 60,
            ..CaptureSettings::default()
        };

        assert_eq!(gop_size(settings.fps), 120);
    }

    #[test]
    fn recommended_bitrate_tracks_output_pixels_and_frame_rate() {
        for (width, height, fps, expected_kbps) in [
            (1280, 720, 60, 9_500),
            (1920, 1080, 30, 9_000),
            (1920, 1080, 60, 18_000),
            (1920, 1080, 120, 36_000),
            (2560, 1440, 60, 28_500),
            (3840, 2160, 60, 54_500),
            (1920, 804, 60, 14_000),
        ] {
            assert_eq!(
                recommended_video_bitrate(width, height, fps),
                (expected_kbps, false),
                "{width}x{height} at {fps} fps"
            );
        }
    }

    #[test]
    fn recommended_bitrate_reports_the_extreme_quality_cap() {
        assert_eq!(recommended_video_bitrate(3840, 2160, 120), (80_000, true));
        assert_eq!(recommended_video_bitrate(3840, 2160, 88), (80_000, false));
        assert_eq!(recommended_video_bitrate(3840, 2160, 89), (80_000, true));
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
        configure_encoder(&encoder, "nvd3d11av1enc", Codec::Av1, 60);

        assert_eq!(encoder.property::<i32>("gop-size"), 120);
        assert_eq!(
            encoder.property::<u64>("min-force-key-unit-interval"),
            1_000_000_000
        );
        assert!(encoder.property::<bool>("spatial-aq"));
    }

    #[test]
    fn nvidia_h265_and_h264_encoders_use_nvidia_low_latency_tuning() {
        gst::init().unwrap();

        for (factory, codec) in [
            ("nvd3d11h265enc", Codec::H265),
            ("nvd3d11h264enc", Codec::H264),
        ] {
            let encoder = gst::ElementFactory::make(factory).build().unwrap();
            configure_encoder(&encoder, factory, codec, 60);

            assert_eq!(encoder.property::<i32>("gop-size"), 120);
            assert!(encoder.property::<bool>("spatial-aq"), "{factory}");
        }
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
    fn capture_chain_uses_selected_encoder_independently_of_codec_identity() {
        let settings = CaptureSettings {
            codec: Codec::H265,
            encoder: "nvd3d11h265enc",
            ..CaptureSettings::default()
        };

        let chain = build_capture_chain(&settings);

        assert!(chain.contains("nvd3d11h265enc"));
        assert!(chain.contains("h265parse"));
        assert!(!chain.contains("mfh265enc"));
    }

    #[test]
    fn audio_payload_is_forced_to_the_advertised_stereo_format() {
        let chain = build_audio_chain(42, None);

        assert!(chain.contains("audio/x-raw,format=S16LE,rate=48000,channels=2,layout=interleaved"));
        assert!(!chain.contains("inband-fec=true"));
        assert!(chain.contains("buffer-time=100000 latency-time=20000"));
    }

    #[test]
    fn sharing_one_window_captures_only_that_window() {
        // Scoping to the shared app is what keeps voice chat and music out of
        // the stream, and it has to win even when a tray pid is offered: there
        // is nothing to exclude from a capture that is already this narrow.
        let chain = build_audio_chain(42, Some(7));
        assert!(chain.contains("loopback-mode=include-process-tree loopback-target-pid=42"));
        assert!(!chain.contains("exclude"));
    }

    #[test]
    fn sharing_the_whole_screen_captures_everything_except_our_own_cues() {
        // Whole-screen sharing is meant to pick up everything the machine
        // plays. The tray's cues are the exception: a chime that says "someone
        // joined" is for the person hosting, and broadcasting it to everyone
        // already watching is the opposite of feedback.
        let chain = build_audio_chain(0, Some(7));
        assert!(chain.contains("loopback-mode=exclude-process-tree loopback-target-pid=7"));

        // Run without a tray - `orange host` from a shell - and there is no
        // process making cues, so nothing is excluded.
        let unattended = build_audio_chain(0, None);
        assert!(!unattended.contains("loopback-mode"));
        assert!(unattended.contains("wasapi2src loopback=true"));
    }

    #[test]
    fn audio_preflight_covers_every_chain_element() {
        let chain = build_audio_chain(42, None);
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
