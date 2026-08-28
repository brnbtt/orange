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
use tokio::sync::mpsc;

use crate::pipeline::{build_capture_chain, check_elements, CaptureSettings};
use crate::signal::{connect, Signal};
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

/// Forward locally-gathered ICE candidates to the other peer.
fn forward_ice(bin: &gst::Element, out: mpsc::UnboundedSender<Signal>) {
    bin.connect("on-ice-candidate", false, move |values| {
        let mline = values[1].get::<u32>().unwrap();
        let candidate = values[2].get::<String>().unwrap();
        let _ = out.send(Signal::Ice { mline, candidate });
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

/// Host: capture a window and wait for viewers.
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
    let bin = make_webrtcbin("host")?;

    pipeline.add_many([capture.upcast_ref(), &pay, &caps_filter, &bin])?;
    gst::Element::link_many([capture.upcast_ref(), &pay, &caps_filter])?;
    let sink_pad = bin
        .request_pad_simple("sink_%u")
        .context("webrtcbin refused a sink pad")?;
    caps_filter.static_pad("src").unwrap().link(&sink_pad)?;

    forward_ice(&bin, client.outgoing.clone());
    pipeline.set_state(gst::State::Playing)?;

    // --- signalling loop --------------------------------------------------
    while let Some(signal) = client.incoming.recv().await {
        match signal {
            Signal::Hosting { code } => {
                println!("\n  Share this code:  {code}\n");
                println!("  Viewers run:  orange watch --code {code}\n");
            }
            Signal::ViewerJoined => {
                println!("[host] viewer joined, negotiating");
                create_offer(&bin, client.outgoing.clone());
            }
            Signal::Sdp { kind, sdp } if kind == "answer" => {
                let desc = parse_sdp(&kind, &sdp)?;
                bin.emit_by_name::<()>("set-remote-description", &[&desc, &None::<gst::Promise>]);
                println!("[host] streaming");
            }
            Signal::Ice { mline, candidate } => {
                bin.emit_by_name::<()>("add-ice-candidate", &[&mline, &candidate]);
            }
            Signal::Error { message } => eprintln!("[host] server: {message}"),
            _ => {}
        }
    }

    pipeline.set_state(gst::State::Null)?;
    Ok(())
}

fn create_offer(bin: &gst::Element, out: mpsc::UnboundedSender<Signal>) {
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

    forward_ice(&bin, client.outgoing.clone());

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
            Signal::Sdp { kind, sdp } if kind == "offer" => {
                let desc = parse_sdp(&kind, &sdp)?;
                bin.emit_by_name::<()>("set-remote-description", &[&desc, &None::<gst::Promise>]);
                create_answer(&bin, client.outgoing.clone());
            }
            Signal::Ice { mline, candidate } => {
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
            kind: "answer".into(),
            sdp: answer.sdp().as_text().unwrap_or_default(),
        });
    });
    bin.emit_by_name::<()>("create-answer", &[&None::<gst::Structure>, &promise]);
}
