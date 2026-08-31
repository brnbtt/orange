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

mod receive;
mod transport;
mod workers;
pub(crate) use receive::build_audio_branch;
pub use receive::{build_receive_branch, encoding_name};
pub(crate) use transport::{
    audio_rtp_caps, configure_receive_transport, video_rtp_caps as rtp_caps,
};
pub(crate) use workers::{
    accept_receive_pad, watch_incoming_bitrate, AcceptedReceivePad, AudioControlWorker,
    ReceiveWorkerRegistry,
};

use anyhow::{Context, Result};
use gst::prelude::*;
use gstreamer as gst;
use gstreamer_sdp as gst_sdp;
use gstreamer_webrtc as gst_webrtc;
use std::sync::{Arc, Mutex};

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

struct LoopbackSender {
    element: gst::Element,
    sink_pad: gst::Pad,
}

impl LoopbackSender {
    fn request(element: &gst::Element) -> Result<Self> {
        let sink_pad = element
            .request_pad_simple("sink_%u")
            .context("webrtcbin refused a sink pad")?;
        Ok(Self {
            element: element.clone(),
            sink_pad,
        })
    }
}

impl Drop for LoopbackSender {
    fn drop(&mut self) {
        if let Some(peer) = self.sink_pad.peer() {
            let _ = peer.unlink(&self.sink_pad);
        }
        self.element.release_request_pad(&self.sink_pad);
    }
}

fn link_loopback_sender(element: &gst::Element, src_pad: &gst::Pad) -> Result<LoopbackSender> {
    let sender = LoopbackSender::request(element)?;
    src_pad.link(&sender.sink_pad)?;
    Ok(sender)
}

/// Wire the two `webrtcbin` elements together: offer/answer plus ICE.
///
/// Normally these messages would cross a network via a signalling server. Here
/// they are function calls, which isolates the media path from any networking
/// concerns while we verify it.
fn connect_signalling(sender: &gst::Element, receiver: &gst::Element) {
    // Trickle ICE, in both directions.
    let rx = receiver.downgrade();
    sender.connect("on-ice-candidate", false, move |values| {
        let rx = rx.upgrade()?;
        let mlineindex = values[1].get::<u32>().unwrap();
        let candidate = values[2].get::<String>().unwrap();
        rx.emit_by_name::<()>("add-ice-candidate", &[&mlineindex, &candidate]);
        None
    });

    let tx = sender.downgrade();
    receiver.connect("on-ice-candidate", false, move |values| {
        let tx = tx.upgrade()?;
        let mlineindex = values[1].get::<u32>().unwrap();
        let candidate = values[2].get::<String>().unwrap();
        tx.emit_by_name::<()>("add-ice-candidate", &[&mlineindex, &candidate]);
        None
    });

    // The sender drives negotiation as soon as its sink pad is linked.
    let sender_weak = sender.downgrade();
    let receiver_weak = receiver.downgrade();
    sender.connect("on-negotiation-needed", false, move |_| {
        let (Some(sender), Some(receiver)) = (sender_weak.upgrade(), receiver_weak.upgrade())
        else {
            return None;
        };
        let offer_sender = sender.downgrade();
        let offer_receiver = receiver.downgrade();

        let promise = gst::Promise::with_change_func(move |reply| {
            let (Some(sender), Some(receiver)) = (offer_sender.upgrade(), offer_receiver.upgrade())
            else {
                return;
            };
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
            let answer_sender = sender.downgrade();
            let answer_receiver = receiver.downgrade();
            let answer_promise = gst::Promise::with_change_func(move |reply| {
                let (Some(sender), Some(receiver)) =
                    (answer_sender.upgrade(), answer_receiver.upgrade())
                else {
                    return;
                };
                let Ok(Some(reply)) = reply else {
                    eprintln!("[webrtc] answer failed");
                    return;
                };
                let answer = reply
                    .value("answer")
                    .unwrap()
                    .get::<gst_webrtc::WebRTCSessionDescription>()
                    .unwrap();
                receiver
                    .emit_by_name::<()>("set-local-description", &[&answer, &None::<gst::Promise>]);
                sender.emit_by_name::<()>(
                    "set-remote-description",
                    &[&answer, &None::<gst::Promise>],
                );
                println!("[webrtc] negotiation complete");
            });
            receiver
                .emit_by_name::<()>("create-answer", &[&None::<gst::Structure>, &answer_promise]);
        });

        sender.emit_by_name::<()>("create-offer", &[&None::<gst::Structure>, &promise]);
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

pub(crate) enum ReceiveOutput {
    Window(crate::window::PlaybackWindowHandle),
    File(String),
}

/// Capture a window, send it over WebRTC, receive it back, and output it.
///
/// Both peers live in this process. If this works, the encode -> payload ->
/// transport -> depayload -> decode path is sound and only signalling stands
/// between us and streaming to another machine.
pub fn run_loopback(settings: &CaptureSettings, output: Output, seconds: u64) -> Result<()> {
    // Keep the unique owner outside every callback and declare it before the
    // pipeline so the sink reaches Null before owner-driven HWND destruction.
    let (playback_owner, output) = match output {
        Output::Window(owner) => {
            let handle = owner.handle();
            (Some(owner), ReceiveOutput::Window(handle))
        }
        Output::File(path) => (None, ReceiveOutput::File(path)),
    };
    let playback = playback_owner.as_ref().map(|owner| owner.handle());
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
    configure_receive_transport(&recv_bin, matches!(&output, ReceiveOutput::Window(_)))?;

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
    let _loopback_sender = link_loopback_sender(&send_bin, &src_pad)?;

    // The receiver's pad appears only once media starts flowing.
    let pipeline_weak = pipeline.downgrade();
    let output = Arc::new(Mutex::new(Some(output)));
    let overlay = playback.as_ref().map(|playback| playback.overlay().clone());
    let workers = ReceiveWorkerRegistry::new();
    let workers_for_pad = workers.clone();
    let overlay_for_pad = overlay.clone();
    let pad_added = recv_bin.connect_pad_added(move |_, pad| {
        let Some(pipeline) = pipeline_weak.upgrade() else {
            return;
        };
        match accept_receive_pad(&workers_for_pad, pad) {
            Some(AcceptedReceivePad::Audio(claim)) => {
                match build_audio_branch(&pipeline, pad, overlay_for_pad.clone(), "loopback") {
                    Ok(worker) => claim.complete_audio(worker),
                    Err(error) => eprintln!("[webrtc] could not build audio branch: {error}"),
                }
            }
            Some(AcceptedReceivePad::Video(claim)) => {
                let Some(output) = output
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .take()
                else {
                    return;
                };
                match build_receive_branch(&pipeline, pad, output, None, "loopback") {
                    Ok(()) => {
                        let bitrate =
                            overlay_for_pad.clone().and_then(
                                |overlay| match watch_incoming_bitrate(pad, overlay) {
                                    Ok(worker) => Some(worker),
                                    Err(error) => {
                                        eprintln!(
                                            "[webrtc] incoming bitrate telemetry disabled: {error}"
                                        );
                                        None
                                    }
                                },
                            );
                        claim.complete_video(bitrate);
                    }
                    Err(error) => eprintln!("[webrtc] could not build receive branch: {error}"),
                }
            }
            None => eprintln!("[webrtc] ignoring duplicate or unexpected stream"),
        }
    });

    connect_signalling(&send_bin, &recv_bin);

    crate::run_pipeline_while_with_shutdown(&pipeline, seconds, playback.as_ref(), move || {
        recv_bin.disconnect(pad_added);
        workers.shutdown()
    })
}

#[cfg(test)]
mod loopback_lifecycle_tests {
    use super::*;

    fn requested_sink_pad_count(element: &gst::Element) -> usize {
        element
            .pads()
            .into_iter()
            .filter(|pad| pad.direction() == gst::PadDirection::Sink)
            .count()
    }

    #[test]
    fn loopback_signalling_does_not_keep_peers_alive() {
        gst::init().unwrap();

        for index in 0..3 {
            let sender = gst::ElementFactory::make("webrtcbin")
                .name(format!("lifecycle-sender-{index}"))
                .build()
                .unwrap();
            let receiver = gst::ElementFactory::make("webrtcbin")
                .name(format!("lifecycle-receiver-{index}"))
                .build()
                .unwrap();
            let sender_weak = sender.downgrade();
            let receiver_weak = receiver.downgrade();

            connect_signalling(&sender, &receiver);
            drop(sender);
            drop(receiver);

            assert!(sender_weak.upgrade().is_none());
            assert!(receiver_weak.upgrade().is_none());
        }
    }

    #[test]
    fn loopback_requested_sink_pad_returns_to_baseline_after_owner_drop() {
        gst::init().unwrap();
        let sender = gst::ElementFactory::make("webrtcbin").build().unwrap();
        let baseline = requested_sink_pad_count(&sender);

        for _ in 0..3 {
            let pad = LoopbackSender::request(&sender).unwrap();
            assert_eq!(requested_sink_pad_count(&sender), baseline + 1);
            drop(pad);
        }

        assert_eq!(requested_sink_pad_count(&sender), baseline);
    }

    #[test]
    fn loopback_linked_sender_owner_unlinks_and_releases_on_drop() {
        gst::init().unwrap();
        let pipeline = gst::Pipeline::new();
        let source = gst::ElementFactory::make("fakesrc").build().unwrap();
        let sender = gst::ElementFactory::make("webrtcbin").build().unwrap();
        pipeline.add_many([&source, &sender]).unwrap();
        let src_pad = source.static_pad("src").unwrap();
        let baseline = requested_sink_pad_count(&sender);

        let owner = link_loopback_sender(&sender, &src_pad).unwrap();

        assert!(src_pad.is_linked());
        assert_eq!(requested_sink_pad_count(&sender), baseline + 1);
        drop(owner);
        assert!(!src_pad.is_linked());
        assert_eq!(requested_sink_pad_count(&sender), baseline);

        pipeline.set_state(gst::State::Null).unwrap();
        pipeline.remove(&source).unwrap();
        pipeline.remove(&sender).unwrap();
    }

    #[test]
    fn loopback_link_error_releases_requested_sink_pad() {
        gst::init().unwrap();
        let sender = gst::ElementFactory::make("webrtcbin").build().unwrap();
        let baseline = requested_sink_pad_count(&sender);
        let source = gst::ElementFactory::make("fakesrc").build().unwrap();
        let sink = gst::ElementFactory::make("fakesink").build().unwrap();
        source.link(&sink).unwrap();
        let already_linked = source.static_pad("src").unwrap();

        let result = link_loopback_sender(&sender, &already_linked);

        assert!(result.is_err());
        assert_eq!(requested_sink_pad_count(&sender), baseline);
    }
}
