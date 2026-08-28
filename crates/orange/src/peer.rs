//! Host and viewer peers talking through the signalling relay.
//!
//! GStreamer callbacks arrive on its own threads while signalling lives in
//! tokio, so the two are bridged with channels rather than shared locks.
//!
//! Note the asymmetry: only the host creates offers. Negotiation is deferred
//! until a viewer actually joins, so an idle host is not sitting in a
//! half-negotiated state.

use anyhow::{Context, Result};
use gst::prelude::*;
use gstreamer as gst;
use gstreamer_sdp as gst_sdp;
use gstreamer_webrtc as gst_webrtc;
use std::collections::HashMap;
use tokio::sync::mpsc;

use crate::pipeline::{build_capture_chain, check_elements, CaptureSettings};
use orange_signal::{connect, Signal};
use crate::webrtc::{build_receive_branch, rtp_caps, Output};

/// Public STUN lets peers discover their external address. Without it, two
/// machines behind different routers will never find each other.
const STUN: &str = "stun://stun.l.google.com:19302";

fn make_webrtcbin(name: &str) -> Result<gst::Element> {
    gst::ElementFactory::make("webrtcbin")
        .name(name)
        .property_from_str("bundle-policy", "max-bundle")
        .property("stun-server", STUN)
        .build()
        .context("webrtcbin missing")
}

/// Surface pipeline errors.
///
/// Without this, a failure inside the receive branch (a decoder refusing caps,
/// an element failing to start) is completely silent: the peer connection
/// reports Connected and nothing ever explains why no frames appear.
fn watch_bus(pipeline: &gst::Pipeline, label: &'static str) {
    let Some(bus) = pipeline.bus() else { return };
    std::thread::spawn(move || loop {
        let Some(msg) = bus.timed_pop(gst::ClockTime::from_mseconds(500)) else {
            continue;
        };
        match msg.view() {
            gst::MessageView::Error(err) => {
                eprintln!(
                    "[{label}] ERROR from {}: {} ({})",
                    msg.src().map(|s| s.path_string()).unwrap_or_default(),
                    err.error(),
                    err.debug().unwrap_or_default()
                );
            }
            gst::MessageView::Warning(w) => {
                eprintln!("[{label}] warning: {} ({})", w.error(), w.debug().unwrap_or_default());
            }
            gst::MessageView::Eos(_) => {
                println!("[{label}] end of stream");
                break;
            }
            _ => {}
        }
    });
}

/// Log ICE and DTLS state transitions.
///
/// Without this, a failed connection is indistinguishable from a working one:
/// `pad-added` fires when the transceiver is created, which happens whether or
/// not any media ever arrives. The states below are the difference between
/// "negotiated" and "actually connected".
fn watch_connection(bin: &gst::Element, label: String) {
    let l = label.clone();
    bin.connect_notify(Some("ice-connection-state"), move |bin, _| {
        let state = bin.property::<gst_webrtc::WebRTCICEConnectionState>("ice-connection-state");
        println!("[{l}] ice: {state:?}");
    });

    let l = label.clone();
    bin.connect_notify(Some("ice-gathering-state"), move |bin, _| {
        let state = bin.property::<gst_webrtc::WebRTCICEGatheringState>("ice-gathering-state");
        println!("[{l}] gathering: {state:?}");
    });

    bin.connect_notify(Some("connection-state"), move |bin, _| {
        let state = bin.property::<gst_webrtc::WebRTCPeerConnectionState>("connection-state");
        println!("[{label}] peer connection: {state:?}");
    });
}

/// Forward locally-gathered ICE candidates to the other peer.
fn forward_ice(bin: &gst::Element, out: mpsc::UnboundedSender<Signal>, peer: String) {
    bin.connect("on-ice-candidate", false, move |values| {
        let mline = values[1].get::<u32>().unwrap();
        let candidate = values[2].get::<String>().unwrap();
        let _ = out.send(Signal::Ice {
            peer: peer.clone(),
            mline,
            candidate,
        });
        None
    });
}

fn parse_sdp(kind: &str, sdp: &str) -> Result<gst_webrtc::WebRTCSessionDescription> {
    let msg = gst_sdp::SDPMessage::parse_buffer(sdp.as_bytes())
        .map_err(|_| anyhow::anyhow!("malformed SDP"))?;
    let sdp_type = match kind {
        "offer" => gst_webrtc::WebRTCSDPType::Offer,
        "answer" => gst_webrtc::WebRTCSDPType::Answer,
        other => anyhow::bail!("unknown SDP type '{other}'"),
    };
    Ok(gst_webrtc::WebRTCSessionDescription::new(sdp_type, msg))
}

/// Host: capture a window and serve any number of viewers.
///
/// The window is captured and encoded **once**. A `tee` after the payloader
/// fans the encoded stream out to one `webrtcbin` per viewer, so adding a
/// viewer costs upload bandwidth but no extra GPU or CPU work.
pub async fn run_host(settings: &CaptureSettings, url: &str) -> Result<()> {
    check_elements(settings.codec)?;
    let mut client = connect(url).await?;
    client.outgoing.send(Signal::Host)?;

    // --- pipeline ---------------------------------------------------------
    let pipeline = gst::Pipeline::new();
    let capture = gst::parse::bin_from_description(&build_capture_chain(settings), true)
        .context("failed to build capture chain")?;
    let pay = gst::ElementFactory::make("rtpav1pay").build()?;
    let caps_filter = gst::ElementFactory::make("capsfilter")
        .property("caps", rtp_caps())
        .build()?;
    let tee = gst::ElementFactory::make("tee")
        .property("allow-not-linked", true)
        .build()?;

    pipeline.add_many([capture.upcast_ref(), &pay, &caps_filter, &tee])?;
    gst::Element::link_many([capture.upcast_ref(), &pay, &caps_filter, &tee])?;
    watch_bus(&pipeline, "host");
    pipeline.set_state(gst::State::Playing)?;

    // One peer connection per viewer, keyed by the relay's peer id.
    let mut viewers: HashMap<String, gst::Element> = HashMap::new();

    // --- signalling loop --------------------------------------------------
    while let Some(signal) = client.incoming.recv().await {
        match signal {
            Signal::Hosting { code } => {
                println!("\n  Share this code:  {code}\n");
                println!("  Viewers run:  orange watch --code {code}\n");
            }
            Signal::ViewerJoined { peer } => {
                match add_viewer(&pipeline, &tee, &peer, client.outgoing.clone()) {
                    Ok(bin) => {
                        viewers.insert(peer.clone(), bin);
                        println!("[host] viewer {peer} joined ({} total)", viewers.len());
                    }
                    Err(err) => eprintln!("[host] could not add viewer {peer}: {err}"),
                }
            }
            Signal::ViewerLeft { peer } => {
                if let Some(bin) = viewers.remove(&peer) {
                    remove_viewer(&pipeline, &bin);
                    println!("[host] viewer {peer} left ({} remaining)", viewers.len());
                }
            }
            Signal::Sdp { peer, kind, sdp } if kind == "answer" => {
                if let Some(bin) = viewers.get(&peer) {
                    let desc = parse_sdp(&kind, &sdp)?;
                    bin.emit_by_name::<()>(
                        "set-remote-description",
                        &[&desc, &None::<gst::Promise>],
                    );
                    println!("[host] streaming to {peer}");
                }
            }
            Signal::Ice {
                peer,
                mline,
                candidate,
            } => {
                if let Some(bin) = viewers.get(&peer) {
                    bin.emit_by_name::<()>("add-ice-candidate", &[&mline, &candidate]);
                }
            }
            Signal::Error { message } => eprintln!("[host] server: {message}"),
            _ => {}
        }
    }

    pipeline.set_state(gst::State::Null)?;
    Ok(())
}

/// Attach a new viewer branch to the running pipeline and start negotiating.
fn add_viewer(
    pipeline: &gst::Pipeline,
    tee: &gst::Element,
    peer: &str,
    out: mpsc::UnboundedSender<Signal>,
) -> Result<gst::Element> {
    // A queue per branch so one slow viewer cannot stall the others or the
    // encoder. Leaky because dropping frames beats blocking the whole tee.
    let queue = gst::ElementFactory::make("queue")
        .property("max-size-buffers", 200u32)
        .property_from_str("leaky", "downstream")
        .build()?;
    let bin = make_webrtcbin(&format!("viewer-{peer}"))?;

    pipeline.add_many([&queue, &bin])?;
    queue.link(&bin)?;

    let tee_pad = tee
        .request_pad_simple("src_%u")
        .context("tee refused a source pad")?;
    tee_pad.link(&queue.static_pad("sink").unwrap())?;

    watch_connection(&bin, format!("host->{peer}"));
    forward_ice(&bin, out.clone(), peer.to_string());

    queue.sync_state_with_parent()?;
    bin.sync_state_with_parent()?;

    create_offer(&bin, out, peer.to_string());
    Ok(bin)
}

fn remove_viewer(pipeline: &gst::Pipeline, bin: &gst::Element) {
    let _ = bin.set_state(gst::State::Null);
    let _ = pipeline.remove(bin);
}

fn create_offer(bin: &gst::Element, out: mpsc::UnboundedSender<Signal>, peer: String) {
    let bin_clone = bin.clone();
    let promise = gst::Promise::with_change_func(move |reply| {
        let Ok(Some(reply)) = reply else {
            eprintln!("[host] create-offer failed");
            return;
        };
        let offer = reply
            .value("offer")
            .unwrap()
            .get::<gst_webrtc::WebRTCSessionDescription>()
            .unwrap();
        bin_clone.emit_by_name::<()>("set-local-description", &[&offer, &None::<gst::Promise>]);
        let _ = out.send(Signal::Sdp {
            peer,
            kind: "offer".into(),
            sdp: offer.sdp().as_text().unwrap_or_default(),
        });
    });
    bin.emit_by_name::<()>("create-offer", &[&None::<gst::Structure>, &promise]);
}

/// Viewer: join a stream by code.
pub async fn run_watch(code: &str, url: &str, output: Output) -> Result<()> {
    let mut client = connect(url).await?;
    client.outgoing.send(Signal::Join {
        code: code.to_string(),
    })?;
    println!("[watch] joining {code}...");

    let pipeline = gst::Pipeline::new();
    let bin = make_webrtcbin("viewer")?;
    pipeline.add(&bin)?;
    watch_bus(&pipeline, "watch");

    watch_connection(&bin, "watch".to_string());
    forward_ice(&bin, client.outgoing.clone(), String::new());

    // The receive branch cannot be built until media arrives and we know the
    // pad exists.
    let pipeline_weak = pipeline.downgrade();
    let output = std::sync::Arc::new(std::sync::Mutex::new(Some(output)));
    bin.connect_pad_added(move |_, pad| {
        let Some(pipeline) = pipeline_weak.upgrade() else {
            return;
        };
        let Some(output) = output.lock().unwrap().take() else {
            return;
        };
        if let Err(err) = build_receive_branch(&pipeline, pad, output) {
            eprintln!("[watch] could not build receive branch: {err}");
        }
    });

    pipeline.set_state(gst::State::Playing)?;

    while let Some(signal) = client.incoming.recv().await {
        match signal {
            Signal::Sdp { kind, sdp, .. } if kind == "offer" => {
                let desc = parse_sdp(&kind, &sdp)?;
                bin.emit_by_name::<()>("set-remote-description", &[&desc, &None::<gst::Promise>]);
                create_answer(&bin, client.outgoing.clone());
            }
            Signal::Ice { mline, candidate, .. } => {
                bin.emit_by_name::<()>("add-ice-candidate", &[&mline, &candidate]);
            }
            Signal::Error { message } => {
                anyhow::bail!("{message}");
            }
            _ => {}
        }
    }

    pipeline.set_state(gst::State::Null)?;
    Ok(())
}

fn create_answer(bin: &gst::Element, out: mpsc::UnboundedSender<Signal>) {
    let bin_clone = bin.clone();
    let promise = gst::Promise::with_change_func(move |reply| {
        let Ok(Some(reply)) = reply else {
            eprintln!("[watch] create-answer failed");
            return;
        };
        let answer = reply
            .value("answer")
            .unwrap()
            .get::<gst_webrtc::WebRTCSessionDescription>()
            .unwrap();
        bin_clone.emit_by_name::<()>("set-local-description", &[&answer, &None::<gst::Promise>]);
        let _ = out.send(Signal::Sdp {
            peer: String::new(),
            kind: "answer".into(),
            sdp: answer.sdp().as_text().unwrap_or_default(),
        });
    });
    bin.emit_by_name::<()>("create-answer", &[&None::<gst::Structure>, &promise]);
}
