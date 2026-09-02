use anyhow::{Context, Result};
use gst::prelude::*;
use gstreamer as gst;
use gstreamer_video::prelude::VideoOverlayExtManual;
use std::sync::Arc;

use super::{workers::AudioControlWorker, ReceiveOutput};
use crate::media_diagnostics::{
    measure_operation, track_pad, MediaProgress, MediaStage, Operation,
};
use crate::pipeline::Codec;

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
                .property("sync", false)
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
                .property("sync", false)
                .property("buffer-time", 40_000i64)
                .property("latency-time", 10_000i64)
                .build()
                .context("audio sink is unavailable")
        }),
    }
}

fn file_output_factories(
    codec: Codec,
    encoder: Option<&'static str>,
) -> (&'static str, &'static str) {
    (encoder.unwrap_or_else(|| codec.encoder()), codec.parser())
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
    output: ReceiveOutput,
    progress: Option<Arc<MediaProgress>>,
    diagnostic_role: &str,
) -> Result<()> {
    let reveal_playback = match &output {
        ReceiveOutput::Window(playback) => Some(playback.clone()),
        ReceiveOutput::File { .. } => None,
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
        matches!(&output, ReceiveOutput::File { .. }),
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
        ReceiveOutput::Window(playback) => {
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
            crate::overlay::attach(&composition.element, &playback)?;
            let queue = build_live_video_queue(diagnostic_role)?;

            let sink = build_video_sink(diagnostic_role)?;
            let overlay_iface = sink
                .element
                .dynamic_cast_ref::<gstreamer_video::VideoOverlay>()
                .context("d3d11videosink does not implement GstVideoOverlay")?;
            let hwnd = playback.hwnd().context("playback window is unavailable")?;
            // SAFETY: Window ReceiveOutput values are created only by
            // run_loopback/run_watch, where the unique owner is declared
            // before and outlives the receiver pipeline and its callbacks.
            unsafe { overlay_iface.set_window_handle(hwnd as usize) };

            vec![queue, composition, sink]
        }
        ReceiveOutput::File { path, encoder } => {
            // Re-encode only because writing raw frames to disk is impractical.
            // This branch exists for verification, not for the real product.
            let (encoder, parser) = file_output_factories(codec, encoder);
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
pub(crate) fn build_audio_branch(
    pipeline: &gst::Pipeline,
    pad: &gst::Pad,
    overlay: Option<crate::overlay::SharedOverlay>,
    progress: Option<Arc<MediaProgress>>,
    diagnostic_role: &str,
) -> Result<Option<AudioControlWorker>> {
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

    if let Some(progress) = progress {
        track_pad(
            &depay
                .element
                .static_pad("src")
                .context("audio depayloader has no src pad")?,
            MediaStage::AudioDepay,
            progress.clone(),
        );
        track_pad(
            &dec.element
                .static_pad("src")
                .context("audio decoder has no src pad")?,
            MediaStage::AudioDecoded,
            progress.clone(),
        );
        track_pad(
            &sink
                .element
                .static_pad("sink")
                .context("audio sink has no sink pad")?,
            MediaStage::AudioSinkInput,
            progress,
        );
    }

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

    let worker = overlay.as_ref().and_then(|overlay| {
        match AudioControlWorker::spawn(
            all[4].element.clone(),
            overlay,
            initial_volume,
            diagnostic_role,
        ) {
            Ok(worker) => Some(worker),
            Err(error) => {
                eprintln!("[{diagnostic_role}] audio controls disabled: {error}");
                None
            }
        }
    });
    println!("[webrtc] receiving audio");
    Ok(worker)
}

#[cfg(test)]
mod tests {
    use super::super::{build_video_payloader, rtp_caps};
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
            file_output_factories(Codec::H264, None),
            ("mfh264enc", "h264parse")
        );
    }

    #[test]
    fn h265_file_output_uses_cross_vendor_media_foundation() {
        assert_eq!(
            file_output_factories(Codec::H265, None),
            ("mfh265enc", "h265parse")
        );
    }

    #[test]
    fn loopback_file_output_reuses_the_selected_capture_encoder() {
        assert_eq!(
            file_output_factories(Codec::H265, Some("nvd3d11h265enc")),
            ("nvd3d11h265enc", "h265parse")
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
        assert!(!audio.element.property::<bool>("sync"));
        assert_eq!(audio.element.property::<i64>("buffer-time"), 40_000);
        assert_eq!(audio.element.property::<i64>("latency-time"), 10_000);
    }
}
