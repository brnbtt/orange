//! WebRTC transport.
//!
//! The important architectural decision lives here: we use `webrtcbin` rather
//! than `webrtcsink`.
//!
//! `webrtcsink` is friendlier - it handles negotiation and codec selection -
//! but it owns the encoder and expects raw video. That would re-encode frames
//! we have already encoded on the GPU, discarding the whole reason this
//! project is cheap. `webrtcbin` accepts RTP-payloaded, already-encoded media,
//! so our NVENC output goes straight onto the wire.
//!
//! The price is that we do signalling ourselves. This module proves the media
//! path with both peers in one process, exchanging SDP by direct call.

mod transport;
pub(crate) use transport::{
    audio_rtp_caps, configure_receive_transport, video_rtp_caps as rtp_caps,
};

use anyhow::{Context, Result};
use gst::prelude::*;
use gstreamer as gst;
use gstreamer_sdp as gst_sdp;
use gstreamer_video::prelude::VideoOverlayExtManual;
use gstreamer_webrtc as gst_webrtc;
use std::sync::{Arc, Mutex};

use crate::media_diagnostics::{
    measure_operation, track_pad, MediaProgress, MediaStage, Operation,
};
use crate::pipeline::{
    build_capture_chain, check_elements, configure_encoder, CaptureSettings, Codec,
};

pub fn build_video_payloader(codec: Codec) -> Result<gst::Element> {
    let factory = codec.payloader();
    match codec {
        Codec::Av1 => gst::ElementFactory::make(factory).build(),
        Codec::H264 => gst::ElementFactory::make(factory)
            .property("config-interval", -1i32)
            .property_from_str("aggregate-mode", "zero-latency")
            .build(),
        Codec::H265 => gst::ElementFactory::make(factory)
            .property("config-interval", -1i32)
            .build(),
    }
    .with_context(|| format!("{factory} missing"))
}

struct Peers {
    sender: gst::Element,
    receiver: gst::Element,
}

/// Wire the two `webrtcbin` elements together: offer/answer plus ICE.
///
/// Normally these messages would cross a network via a signalling server. Here
/// they are function calls, which isolates the media path from any networking
/// concerns while we verify it.
fn connect_signalling(peers: Arc<Mutex<Peers>>) {
    let (sender, receiver) = {
        let p = peers.lock().unwrap();
        (p.sender.clone(), p.receiver.clone())
    };

    // Trickle ICE, in both directions.
    let rx = receiver.clone();
    sender.connect("on-ice-candidate", false, move |values| {
        let mlineindex = values[1].get::<u32>().unwrap();
        let candidate = values[2].get::<String>().unwrap();
        rx.emit_by_name::<()>("add-ice-candidate", &[&mlineindex, &candidate]);
        None
    });

    let tx = sender.clone();
    receiver.connect("on-ice-candidate", false, move |values| {
        let mlineindex = values[1].get::<u32>().unwrap();
        let candidate = values[2].get::<String>().unwrap();
        tx.emit_by_name::<()>("add-ice-candidate", &[&mlineindex, &candidate]);
        None
    });

    // The sender drives negotiation as soon as its sink pad is linked.
    let peers_for_neg = peers.clone();
    sender.connect("on-negotiation-needed", false, move |_| {
        let peers = peers_for_neg.clone();
        let (sender, receiver) = {
            let p = peers.lock().unwrap();
            (p.sender.clone(), p.receiver.clone())
        };
        // The closure below takes ownership, so keep a handle for the emit.
        let sender_for_offer = sender.clone();

        let promise = gst::Promise::with_change_func(move |reply| {
            let Ok(Some(reply)) = reply else {
                eprintln!("[webrtc] offer failed");
                return;
            };
            let offer = reply
                .value("offer")
                .unwrap()
                .get::<gst_webrtc::WebRTCSessionDescription>()
                .unwrap();

            sender.emit_by_name::<()>("set-local-description", &[&offer, &None::<gst::Promise>]);
            receiver.emit_by_name::<()>("set-remote-description", &[&offer, &None::<gst::Promise>]);

            // Answer back the other way.
            let sender2 = sender.clone();
            let receiver2 = receiver.clone();
            let answer_promise = gst::Promise::with_change_func(move |reply| {
                let Ok(Some(reply)) = reply else {
                    eprintln!("[webrtc] answer failed");
                    return;
                };
                let answer = reply
                    .value("answer")
                    .unwrap()
                    .get::<gst_webrtc::WebRTCSessionDescription>()
                    .unwrap();
                receiver2
                    .emit_by_name::<()>("set-local-description", &[&answer, &None::<gst::Promise>]);
                sender2.emit_by_name::<()>(
                    "set-remote-description",
                    &[&answer, &None::<gst::Promise>],
                );
                println!("[webrtc] negotiation complete");
            });
            receiver
                .emit_by_name::<()>("create-answer", &[&None::<gst::Structure>, &answer_promise]);
        });

        sender_for_offer.emit_by_name::<()>("create-offer", &[&None::<gst::Structure>, &promise]);
        None
    });

    // Silence the unused warning on the SDP import while keeping it available
    // for the real signalling module that replaces this.
    let _ = gst_sdp::SDPMessage::new();
}

/// Where the received video should end up.
pub enum Output {
    /// Render into a window we own, by HWND, with controls composited on top.
    /// `d3d11videosink` implements `GstVideoOverlay`, so it draws into our
    /// borderless frame instead of creating a bare window of its own.
    Window(crate::window::PlaybackWindow),
    /// Write to a file, so the result can be verified without a display.
    File(String),
}

/// Capture a window, send it over WebRTC, receive it back, and output it.
///
/// Both peers live in this process. If this works, the encode -> payload ->
/// transport -> depayload -> decode path is sound and only signalling stands
/// between us and streaming to another machine.
pub fn run_loopback(settings: &CaptureSettings, output: Output, seconds: u64) -> Result<()> {
    check_elements(settings.codec)?;

    let pipeline = gst::Pipeline::new();

    // --- sending half -------------------------------------------------------
    let capture = gst::parse::bin_from_description(&build_capture_chain(settings), true)
        .context("failed to build capture chain")?;
    let encoder = capture
        .by_name("stream-encoder")
        .context("capture chain has no named encoder")?;
    configure_encoder(&encoder, settings.codec, settings.fps);
    let pay = build_video_payloader(settings.codec)?;
    let caps_filter = gst::ElementFactory::make("capsfilter")
        .property("caps", rtp_caps(settings.codec, settings.fps))
        .build()?;
    let send_bin = gst::ElementFactory::make("webrtcbin")
        .name("sender")
        .property_from_str("bundle-policy", "max-bundle")
        .build()
        .context("webrtcbin missing")?;

    // --- receiving half -----------------------------------------------------
    let recv_bin = gst::ElementFactory::make("webrtcbin")
        .name("receiver")
        .property_from_str("bundle-policy", "max-bundle")
        .build()?;
    configure_receive_transport(&recv_bin, matches!(&output, Output::Window(_)))?;

    pipeline.add_many([
        capture.upcast_ref(),
        &pay,
        &caps_filter,
        &send_bin,
        &recv_bin,
    ])?;
    gst::Element::link_many([capture.upcast_ref(), &pay, &caps_filter])?;

    // webrtcbin takes media on request pads.
    let src_pad = caps_filter.static_pad("src").unwrap();
    let sink_pad = send_bin
        .request_pad_simple("sink_%u")
        .context("webrtcbin refused a sink pad")?;
    src_pad.link(&sink_pad)?;

    // The receiver's pad appears only once media starts flowing.
    let pipeline_weak = pipeline.downgrade();
    let output = Arc::new(Mutex::new(Some(output)));
    recv_bin.connect_pad_added(move |_, pad| {
        let Some(pipeline) = pipeline_weak.upgrade() else {
            return;
        };
        let Some(output) = output.lock().unwrap().take() else {
            return; // only handle the first stream
        };

        if let Err(err) = build_receive_branch(&pipeline, pad, output, None, "loopback") {
            eprintln!("[webrtc] could not build receive branch: {err}");
        }
    });

    connect_signalling(Arc::new(Mutex::new(Peers {
        sender: send_bin,
        receiver: recv_bin,
    })));

    crate::run_pipeline(&pipeline, seconds)
}

/// Which codec a newly-arrived pad carries, so the right branch is built.
pub fn encoding_name(pad: &gst::Pad) -> Option<String> {
    let caps = pad.current_caps().or_else(|| pad.allowed_caps())?;
    let structure = caps.structure(0)?;
    structure.get::<String>("encoding-name").ok()
}

/// Add and link a dynamic receive branch as one transaction. GStreamer does
/// not roll back partially added elements or pad links when a later operation
/// fails, so the caller must do it explicitly before keeping the session alive.
struct ReceiveElement {
    element: gst::Element,
    logical_name: &'static str,
    factory: &'static str,
}

fn build_receive_element(
    diagnostic_role: &str,
    logical_name: &'static str,
    factory: &'static str,
    build: impl FnOnce() -> Result<gst::Element>,
) -> Result<ReceiveElement> {
    let element = measure_operation(
        diagnostic_role,
        Operation::element("element-create", logical_name, factory),
        build,
    )?;
    Ok(ReceiveElement {
        element,
        logical_name,
        factory,
    })
}

fn attach_receive_elements(
    pipeline: &gst::Pipeline,
    pad: &gst::Pad,
    elements: &[ReceiveElement],
    diagnostic_role: &str,
    link_operation: &'static str,
    pad_link_operation: &'static str,
    remove_probe_operation: &'static str,
) -> Result<()> {
    let mut added = 0;
    let block_probe = pad.add_probe(gst::PadProbeType::BLOCK_DOWNSTREAM, |_, _| {
        gst::PadProbeReturn::Ok
    });
    let result = (|| -> Result<()> {
        for element in elements {
            measure_operation(
                diagnostic_role,
                Operation::element("pipeline-add", element.logical_name, element.factory),
                || pipeline.add(&element.element),
            )?;
            added += 1;
        }
        measure_operation(diagnostic_role, Operation::named(link_operation), || {
            gst::Element::link_many(elements.iter().map(|element| &element.element))
        })?;
        measure_operation(
            diagnostic_role,
            Operation::named(pad_link_operation),
            || pad.link(&elements[0].element.static_pad("sink").unwrap()),
        )?;
        for element in elements {
            measure_operation(
                diagnostic_role,
                Operation::element(
                    "sync-state-with-parent",
                    element.logical_name,
                    element.factory,
                ),
                || element.element.sync_state_with_parent(),
            )?;
        }
        Ok(())
    })();
    if let Err(error) = result {
        if let Some(sink_pad) = elements
            .first()
            .and_then(|element| element.element.static_pad("sink"))
        {
            let _ = pad.unlink(&sink_pad);
        }
        for element in elements.iter().take(added).rev() {
            let _ = element.element.set_state(gst::State::Null);
            let _ = pipeline.remove(&element.element);
        }
        if let Some(block_probe) = block_probe {
            measure_operation(
                diagnostic_role,
                Operation::named(remove_probe_operation),
                || {
                    pad.remove_probe(block_probe);
                },
            );
        }
        return Err(error);
    }
    if let Some(block_probe) = block_probe {
        measure_operation(
            diagnostic_role,
            Operation::named(remove_probe_operation),
            || {
                pad.remove_probe(block_probe);
            },
        );
    }
    Ok(())
}

fn frame_rate_from_rtp_caps(caps: &gst::CapsRef) -> Option<u32> {
    caps.structure(0)?
        .get::<String>("a-framerate")
        .ok()?
        .parse::<u32>()
        .ok()
        .filter(|rate| (1..=480).contains(rate))
}

fn av1_decoder_factory(selection: Option<&str>, file_output: bool) -> &'static str {
    match (selection, file_output) {
        (_, true) => "d3d11av1dec",
        (Some("software"), false) => "dav1ddec",
        _ => "d3d11av1dec",
    }
}

fn build_av1_decoder(
    selection: Option<&str>,
    file_output: bool,
    diagnostic_role: &str,
) -> Result<ReceiveElement> {
    let factory = av1_decoder_factory(selection, file_output);
    build_receive_element(diagnostic_role, "video-decoder", factory, || {
        gst::ElementFactory::make(factory)
            .property("automatic-request-sync-points", true)
            .property("discard-corrupted-frames", true)
            .build()
            .with_context(|| format!("{factory} is unavailable"))
    })
}

fn build_video_decoder(
    codec: Codec,
    selection: Option<&str>,
    file_output: bool,
    diagnostic_role: &str,
) -> Result<ReceiveElement> {
    if codec == Codec::Av1 {
        return build_av1_decoder(selection, file_output, diagnostic_role);
    }
    let factory = codec.decoder();
    build_receive_element(diagnostic_role, "video-decoder", factory, || {
        gst::ElementFactory::make(factory)
            .property("automatic-request-sync-points", true)
            .property("discard-corrupted-frames", true)
            .build()
            .with_context(|| format!("{factory} is unavailable"))
    })
}

fn build_video_depayloader(codec: Codec, diagnostic_role: &str) -> Result<ReceiveElement> {
    let factory = codec.depayloader();
    build_receive_element(diagnostic_role, "video-depayloader", factory, || {
        gst::ElementFactory::make(factory)
            .property("request-keyframe", true)
            .property("wait-for-keyframe", true)
            .build()
            .with_context(|| format!("{factory} is unavailable"))
    })
}

#[cfg(test)]
fn build_av1_depayloader(diagnostic_role: &str) -> Result<ReceiveElement> {
    build_video_depayloader(Codec::Av1, diagnostic_role)
}

fn build_live_video_queue(diagnostic_role: &str) -> Result<ReceiveElement> {
    build_receive_element(diagnostic_role, "video-presentation-queue", "queue", || {
        gst::ElementFactory::make("queue")
            .property("max-size-buffers", 1u32)
            .property("max-size-bytes", 0u32)
            .property("max-size-time", 0u64)
            .property_from_str("leaky", "downstream")
            .build()
            .context("video presentation queue is unavailable")
    })
}

fn build_video_sink(diagnostic_role: &str) -> Result<ReceiveElement> {
    build_receive_element(diagnostic_role, "video-sink", "d3d11videosink", || {
        gst::ElementFactory::make("d3d11videosink")
            .property("async", false)
            .property("sync", false)
            .property("force-aspect-ratio", true)
            .build()
            .context("d3d11videosink is unavailable")
    })
}

fn build_audio_sink(diagnostic_role: &str) -> Result<ReceiveElement> {
    let primary = measure_operation(
        diagnostic_role,
        Operation::element("element-create", "audio-sink", "wasapi2sink"),
        || {
            gst::ElementFactory::make("wasapi2sink")
                .property("async", false)
                .property("buffer-time", 40_000i64)
                .property("latency-time", 10_000i64)
                .build()
        },
    );
    match primary {
        Ok(element) => Ok(ReceiveElement {
            element,
            logical_name: "audio-sink",
            factory: "wasapi2sink",
        }),
        Err(_) => build_receive_element(diagnostic_role, "audio-sink", "wasapisink", || {
            gst::ElementFactory::make("wasapisink")
                .property("async", false)
                .property("buffer-time", 40_000i64)
                .property("latency-time", 10_000i64)
                .build()
                .context("audio sink is unavailable")
        }),
    }
}

fn file_output_factories(codec: Codec) -> (&'static str, &'static str) {
    (codec.encoder(), codec.parser())
}

fn build_audio_decoder(diagnostic_role: &str) -> Result<ReceiveElement> {
    build_receive_element(diagnostic_role, "audio-decoder", "opusdec", || {
        Ok(gst::ElementFactory::make("opusdec")
            .property("plc", true)
            .build()?)
    })
}

/// Attach depayload -> parse -> hardware decode -> output to the receiver.
pub fn build_receive_branch(
    pipeline: &gst::Pipeline,
    pad: &gst::Pad,
    output: Output,
    progress: Option<Arc<MediaProgress>>,
    diagnostic_role: &str,
) -> Result<()> {
    let reveal_playback = match &output {
        Output::Window(playback) => Some(playback.clone()),
        Output::File(_) => None,
    };
    let encoding = encoding_name(pad).context("video RTP pad has no encoding name")?;
    let codec = Codec::from_rtp_encoding(&encoding).context("unsupported video RTP encoding")?;
    let depay = build_video_depayloader(codec, diagnostic_role)?;
    let parser = codec.parser();
    let parse = build_receive_element(diagnostic_role, "video-parser", parser, || {
        gst::ElementFactory::make(parser)
            .build()
            .with_context(|| format!("{parser} is unavailable"))
    })?;
    let decoder_selection = std::env::var("ORANGE_AV1_DECODER").ok();
    let dec = build_video_decoder(
        codec,
        decoder_selection.as_deref(),
        matches!(&output, Output::File(_)),
        diagnostic_role,
    )?;
    let advertised_rate = pad
        .current_caps()
        .as_ref()
        .and_then(|caps| frame_rate_from_rtp_caps(caps));
    if let Some(progress) = progress {
        track_pad(
            &depay
                .element
                .static_pad("src")
                .context("depayloader has no src pad")?,
            MediaStage::Depay,
            progress.clone(),
        );
        track_pad(
            &parse
                .element
                .static_pad("src")
                .context("parser has no src pad")?,
            MediaStage::Parsed,
            progress.clone(),
        );
        track_pad(
            &dec.element
                .static_pad("src")
                .context("decoder has no src pad")?,
            MediaStage::Decoded,
            progress,
        );
    }

    let tail: Vec<ReceiveElement> = match output {
        Output::Window(playback) => {
            if let Some(rate) = advertised_rate {
                if let Ok(mut state) = playback.overlay().lock() {
                    state.fps = Some(rate as f64);
                }
            }
            // Controls are composited into the frame here, on the GPU, rather
            // than drawn by a second window that would have to chase this one.
            let composition = build_receive_element(
                diagnostic_role,
                "video-overlay",
                "overlaycomposition",
                || {
                    gst::ElementFactory::make("overlaycomposition")
                        .build()
                        .context("overlaycomposition missing")
                },
            )?;
            crate::overlay::attach(&composition.element, &playback);
            let queue = build_live_video_queue(diagnostic_role)?;

            let sink = build_video_sink(diagnostic_role)?;
            let overlay_iface = sink
                .element
                .dynamic_cast_ref::<gstreamer_video::VideoOverlay>()
                .context("d3d11videosink does not implement GstVideoOverlay")?;
            // SAFETY: the handle belongs to this playback component and
            // remains valid while the receiver session is running.
            unsafe { overlay_iface.set_window_handle(playback.hwnd() as usize) };

            vec![queue, composition, sink]
        }
        Output::File(path) => {
            // Re-encode only because writing raw frames to disk is impractical.
            // This branch exists for verification, not for the real product.
            let (encoder, parser) = file_output_factories(codec);
            let enc =
                build_receive_element(diagnostic_role, "video-file-encoder", encoder, || {
                    Ok(gst::ElementFactory::make(encoder)
                        .property("bitrate", 25_000u32)
                        .build()?)
                })?;
            let parse2 =
                build_receive_element(diagnostic_role, "video-file-parser", parser, || {
                    Ok(gst::ElementFactory::make(parser).build()?)
                })?;
            let mux =
                build_receive_element(diagnostic_role, "video-file-muxer", "matroskamux", || {
                    Ok(gst::ElementFactory::make("matroskamux").build()?)
                })?;
            let sink =
                build_receive_element(diagnostic_role, "video-file-sink", "filesink", || {
                    Ok(gst::ElementFactory::make("filesink")
                        .property("location", path)
                        .build()?)
                })?;
            vec![enc, parse2, mux, sink]
        }
    };

    let mut all = vec![depay, parse, dec];
    all.extend(tail);

    attach_receive_elements(
        pipeline,
        pad,
        &all,
        diagnostic_role,
        "link-video-receive-elements",
        "link-incoming-video-rtp-pad",
        "remove-incoming-video-block-probe",
    )?;
    if let Some(playback) = reveal_playback {
        playback.reveal();
    }
    println!("[webrtc] receiving video");
    Ok(())
}

/// Attach the audio branch: depayload -> decode -> volume -> speakers.
///
/// The overlay's volume and mute state is applied here. A short poll is used
/// rather than a callback because the state is owned by the window thread and
/// changes only on user input; at 20 Hz the cost is unmeasurable.
pub fn build_audio_branch(
    pipeline: &gst::Pipeline,
    pad: &gst::Pad,
    overlay: Option<crate::overlay::SharedOverlay>,
    diagnostic_role: &str,
) -> Result<()> {
    let initial_volume = overlay
        .as_ref()
        .and_then(|overlay| overlay.lock().ok())
        .map(|state| if state.muted { 0.0 } else { state.volume })
        .unwrap_or(0.3);
    let depay =
        build_receive_element(diagnostic_role, "audio-depayloader", "rtpopusdepay", || {
            Ok(gst::ElementFactory::make("rtpopusdepay").build()?)
        })?;
    let dec = build_audio_decoder(diagnostic_role)?;
    let convert =
        build_receive_element(diagnostic_role, "audio-converter", "audioconvert", || {
            Ok(gst::ElementFactory::make("audioconvert").build()?)
        })?;
    let resample =
        build_receive_element(diagnostic_role, "audio-resampler", "audioresample", || {
            Ok(gst::ElementFactory::make("audioresample").build()?)
        })?;
    let volume = build_receive_element(diagnostic_role, "audio-volume", "volume", || {
        Ok(gst::ElementFactory::make("volume")
            .name("viewer-volume")
            .property("volume", initial_volume)
            .build()?)
    })?;
    let sink = build_audio_sink(diagnostic_role)?;

    let all = [depay, dec, convert, resample, volume, sink];
    attach_receive_elements(
        pipeline,
        pad,
        &all,
        diagnostic_role,
        "link-audio-receive-elements",
        "link-incoming-audio-rtp-pad",
        "remove-incoming-audio-block-probe",
    )?;

    if let Some(overlay) = overlay {
        let overlay = Arc::downgrade(&overlay);
        let volume = all[4].element.clone();
        std::thread::spawn(move || {
            let mut applied = initial_volume;
            loop {
                std::thread::sleep(std::time::Duration::from_millis(50));
                let Some(overlay) = overlay.upgrade() else {
                    break;
                };
                let Ok(state) = overlay.lock() else { break };
                let wanted = if state.muted { 0.0 } else { state.volume };
                drop(state);
                if (wanted - applied).abs() > f64::EPSILON {
                    volume.set_property("volume", wanted);
                    applied = wanted;
                }
            }
        });
    }
    println!("[webrtc] receiving audio");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rtp_caps_advertise_the_configured_frame_rate() {
        gst::init().unwrap();
        let caps = rtp_caps(Codec::H264, 120);

        assert_eq!(
            caps.structure(0)
                .unwrap()
                .get::<String>("a-framerate")
                .unwrap(),
            "120"
        );
        assert_eq!(frame_rate_from_rtp_caps(&caps), Some(120));
    }

    #[test]
    fn h264_transport_resends_headers_and_recovers_after_loss() {
        gst::init().unwrap();
        let caps = rtp_caps(Codec::H264, 60);
        let pay = build_video_payloader(Codec::H264).unwrap();
        let depay = build_video_depayloader(Codec::H264, "test").unwrap();

        assert_eq!(
            caps.structure(0)
                .unwrap()
                .get::<String>("encoding-name")
                .unwrap(),
            "H264"
        );
        assert_eq!(pay.property::<i32>("config-interval"), -1);
        assert!(depay.element.property::<bool>("request-keyframe"));
        assert!(depay.element.property::<bool>("wait-for-keyframe"));
        assert_eq!(Codec::from_rtp_encoding("H264"), Some(Codec::H264));
    }

    #[test]
    fn h264_file_output_does_not_require_av1_encoding() {
        assert_eq!(
            file_output_factories(Codec::H264),
            ("mfh264enc", "h264parse")
        );
    }

    #[test]
    fn h265_file_output_uses_cross_vendor_media_foundation() {
        assert_eq!(
            file_output_factories(Codec::H265),
            ("mfh265enc", "h265parse")
        );
    }

    #[test]
    fn opus_decoder_conceals_loss_without_fec_lookahead() {
        gst::init().unwrap();
        let decoder = build_audio_decoder("test").unwrap();

        assert!(decoder.element.property::<bool>("plc"));
        assert!(!decoder.element.property::<bool>("use-inband-fec"));
    }

    #[test]
    fn audio_payload_does_not_collide_with_video_rtx() {
        gst::init().unwrap();
        let payload = audio_rtp_caps()
            .structure(0)
            .unwrap()
            .get::<i32>("payload")
            .unwrap();

        assert_eq!(payload, transport::AUDIO_PAYLOAD);
        assert_ne!(payload, transport::VIDEO_RTX_PAYLOAD);
    }

    #[test]
    fn software_decoder_can_be_selected_for_diagnostic_comparison() {
        assert_eq!(av1_decoder_factory(Some("software"), false), "dav1ddec");
        assert_eq!(av1_decoder_factory(Some("software"), true), "d3d11av1dec");
        assert_eq!(av1_decoder_factory(Some("hardware"), false), "d3d11av1dec");
        assert_eq!(
            av1_decoder_factory(Some("unexpected"), false),
            "d3d11av1dec"
        );
        assert_eq!(av1_decoder_factory(None, false), "d3d11av1dec");
    }

    #[test]
    fn av1_decoder_rejects_corrupt_output_and_requests_recovery() {
        gst::init().unwrap();
        let decoder = build_av1_decoder(Some("software"), false, "test").unwrap();

        assert!(decoder
            .element
            .property::<bool>("automatic-request-sync-points"));
        assert!(decoder.element.property::<bool>("discard-corrupted-frames"));
    }

    #[test]
    fn av1_depayloader_requests_and_waits_for_recovery_keyframes() {
        gst::init().unwrap();
        let depay = build_av1_depayloader("test").unwrap();

        assert!(depay.element.property::<bool>("request-keyframe"));
        assert!(depay.element.property::<bool>("wait-for-keyframe"));
    }

    #[test]
    fn presentation_queue_keeps_only_the_live_decoded_frame() {
        gst::init().unwrap();
        let queue = build_live_video_queue("test").unwrap();

        assert_eq!(queue.element.property::<u32>("max-size-buffers"), 1);
        assert_eq!(queue.element.property::<u32>("max-size-bytes"), 0);
        assert_eq!(queue.element.property::<u64>("max-size-time"), 0);
    }

    #[test]
    fn live_sinks_do_not_wait_for_preroll() {
        gst::init().unwrap();
        let video = build_video_sink("test").unwrap();
        let audio = build_audio_sink("test").unwrap();

        assert!(!video.element.property::<bool>("async"));
        assert!(!video.element.property::<bool>("sync"));
        assert!(!audio.element.property::<bool>("async"));
        assert_eq!(audio.element.property::<i64>("buffer-time"), 40_000);
        assert_eq!(audio.element.property::<i64>("latency-time"), 10_000);
    }
}
