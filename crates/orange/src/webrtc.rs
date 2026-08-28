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

use anyhow::{Context, Result};
use gst::prelude::*;
use gstreamer as gst;
use gstreamer_sdp as gst_sdp;
use gstreamer_video::prelude::VideoOverlayExtManual;
use gstreamer_webrtc as gst_webrtc;
use std::sync::{Arc, Mutex};

use crate::pipeline::{build_capture_chain, check_elements, CaptureSettings};

/// RTP caps for our encoded video. AV1 has no static payload type, so we pick
/// one from the dynamic range and both ends agree on it.
pub fn rtp_caps() -> gst::Caps {
    gst::Caps::builder("application/x-rtp")
        .field("media", "video")
        .field("encoding-name", "AV1")
        .field("payload", 96i32)
        .field("clock-rate", 90_000i32)
        .build()
}

/// RTP caps for Opus audio, on a separate payload type from the video.
pub fn audio_rtp_caps() -> gst::Caps {
    gst::Caps::builder("application/x-rtp")
        .field("media", "audio")
        .field("encoding-name", "OPUS")
        .field("payload", 97i32)
        .field("clock-rate", 48_000i32)
        .field("encoding-params", "2")
        .build()
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
                sender2
                    .emit_by_name::<()>("set-remote-description", &[&answer, &None::<gst::Promise>]);
                println!("[webrtc] negotiation complete");
            });
            receiver.emit_by_name::<()>("create-answer", &[&None::<gst::Structure>, &answer_promise]);
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
    /// Render into a window we own, by HWND. `d3d11videosink` implements
    /// `GstVideoOverlay`, so it draws into our borderless frame instead of
    /// creating a bare window of its own.
    Window(isize),
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
    let pay = gst::ElementFactory::make("rtpav1pay")
        .build()
        .context("rtpav1pay missing")?;
    let caps_filter = gst::ElementFactory::make("capsfilter")
        .property("caps", rtp_caps())
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

        if let Err(err) = build_receive_branch(&pipeline, pad, output) {
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

/// Attach depayload -> parse -> hardware decode -> output to the receiver.
pub fn build_receive_branch(pipeline: &gst::Pipeline, pad: &gst::Pad, output: Output) -> Result<()> {
    let depay = gst::ElementFactory::make("rtpav1depay").build()?;
    let parse = gst::ElementFactory::make("av1parse").build()?;
    let dec = gst::ElementFactory::make("d3d11av1dec")
        .build()
        .context("d3d11av1dec missing - no hardware AV1 decode on this GPU?")?;

    let tail: Vec<gst::Element> = match output {
        Output::Window(hwnd) => {
            let sink = gst::ElementFactory::make("d3d11videosink")
                .property("sync", false)
                .property("force-aspect-ratio", true)
                .build()?;
            // Must be set before the sink reaches PAUSED, otherwise it creates
            // its own window and ours stays empty. `GstVideoOverlay` is an
            // interface, so the element has to be cast to it.
            let overlay = sink
                .dynamic_cast_ref::<gstreamer_video::VideoOverlay>()
                .context("d3d11videosink does not implement GstVideoOverlay")?;
            // SAFETY: `hwnd` comes from our own window, created by
            // `window::spawn`, and remains valid while the viewer runs.
            unsafe { overlay.set_window_handle(hwnd as usize) };
            vec![sink]
        }
        Output::File(path) => {
            // Re-encode only because writing raw frames to disk is impractical.
            // This branch exists for verification, not for the real product.
            let enc = gst::ElementFactory::make("nvd3d11av1enc")
                .property("bitrate", 25_000u32)
                .build()?;
            let parse2 = gst::ElementFactory::make("av1parse").build()?;
            let mux = gst::ElementFactory::make("matroskamux").build()?;
            let sink = gst::ElementFactory::make("filesink")
                .property("location", path)
                .build()?;
            vec![enc, parse2, mux, sink]
        }
    };

    let mut all: Vec<gst::Element> = vec![depay.clone(), parse.clone(), dec.clone()];
    all.extend(tail);

    for e in &all {
        pipeline.add(e)?;
    }
    gst::Element::link_many(all.iter().collect::<Vec<_>>().as_slice())?;
    for e in &all {
        e.sync_state_with_parent()?;
    }

    pad.link(&depay.static_pad("sink").unwrap())?;
    println!("[webrtc] receiving video");
    Ok(())
}

/// Attach the audio branch: depayload -> decode -> volume -> speakers.
///
/// The `volume` element is named so the viewer UI can find it later and drive
/// it from an overlay control.
pub fn build_audio_branch(pipeline: &gst::Pipeline, pad: &gst::Pad) -> Result<()> {
    let depay = gst::ElementFactory::make("rtpopusdepay").build()?;
    let dec = gst::ElementFactory::make("opusdec").build()?;
    let convert = gst::ElementFactory::make("audioconvert").build()?;
    let resample = gst::ElementFactory::make("audioresample").build()?;
    let volume = gst::ElementFactory::make("volume")
        .name("viewer-volume")
        .property("volume", 1.0f64)
        .build()?;
    let sink = gst::ElementFactory::make("wasapi2sink")
        .property("low-latency", true)
        .build()
        .or_else(|_| gst::ElementFactory::make("autoaudiosink").build())?;

    let all = [depay.clone(), dec, convert, resample, volume, sink];
    for e in &all {
        pipeline.add(e)?;
    }
    gst::Element::link_many(all.iter().collect::<Vec<_>>().as_slice())?;
    for e in &all {
        e.sync_state_with_parent()?;
    }

    pad.link(&depay.static_pad("sink").unwrap())?;
    println!("[webrtc] receiving audio");
    Ok(())
}
