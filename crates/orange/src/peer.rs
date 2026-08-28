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
use gstreamer_video as gst_video;
use gstreamer_webrtc as gst_webrtc;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;

use crate::pipeline::{
    build_audio_chain, build_capture_chain, check_audio_elements, check_elements, CaptureSettings,
};
use crate::webrtc::{
    audio_rtp_caps, build_audio_branch, build_receive_branch, encoding_name, rtp_caps, Output,
};
use orange_signal::{connect, Signal};

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
                eprintln!(
                    "[{label}] warning: {} ({})",
                    w.error(),
                    w.debug().unwrap_or_default()
                );
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

/// Measure the encoded video arriving from WebRTC and expose it to the viewer.
///
/// The pad still carries RTP here, before depayloading and decoding, so this is
/// the bitrate actually received rather than an estimate based on decoded
/// frame sizes. The streaming callback only increments an atomic counter; a
/// low-frequency worker does the division and touches UI state once a second.
fn watch_incoming_bitrate(pad: &gst::Pad, overlay: crate::overlay::SharedOverlay) {
    let bytes = Arc::new(AtomicU64::new(0));
    let bytes_for_probe = bytes.clone();
    pad.add_probe(gst::PadProbeType::BUFFER, move |_, info| {
        if let Some(gst::PadProbeData::Buffer(buffer)) = &info.data {
            bytes_for_probe.fetch_add(buffer.size() as u64, Ordering::Relaxed);
        }
        gst::PadProbeReturn::Ok
    });

    let overlay = Arc::downgrade(&overlay);
    std::thread::spawn(move || {
        let mut sampled_at = Instant::now();
        loop {
            std::thread::sleep(Duration::from_secs(1));
            let elapsed = sampled_at.elapsed().as_secs_f64();
            sampled_at = Instant::now();
            let received = bytes.swap(0, Ordering::Relaxed);

            let Some(overlay) = overlay.upgrade() else {
                break;
            };
            if received == 0 || elapsed == 0.0 {
                continue;
            }

            let kbps = ((received as f64 * 8.0) / elapsed / 1000.0).round() as u32;
            if let Ok(mut state) = overlay.lock() {
                state.bitrate_kbps = Some(kbps);
            };
        }
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
/// The window is captured and encoded **once**. Encoded AV1 is fanned out to a
/// fresh RTP payloader per viewer, so late joiners receive their own RTP stream
/// and initialization while still sharing the expensive encoder.
pub async fn run_host(settings: &CaptureSettings, url: &str) -> Result<()> {
    check_elements(settings.codec)?;
    let mut client = connect(url).await?;

    // Identity is optional: without it viewers show up as opaque ids.
    if let Some(session) = crate::auth::load_session() {
        client.outgoing.send(Signal::Authenticate {
            session: session.token,
        })?;
    }
    client.outgoing.send(Signal::Host)?;

    // --- pipeline ---------------------------------------------------------
    let pipeline = gst::Pipeline::new();
    let capture = gst::parse::bin_from_description(&build_capture_chain(settings), true)
        .context("failed to build capture chain")?;
    let tee = gst::ElementFactory::make("tee")
        .property("allow-not-linked", true)
        .build()?;

    pipeline.add_many([capture.upcast_ref(), &tee])?;
    capture.link(&tee)?;

    // Audio is optional: if the process makes no sound, or capture fails, the
    // stream should still work rather than refusing to start.
    let audio_tee = match settings.audio_pid {
        Some(pid) => match build_audio_tee(&pipeline, pid) {
            Ok(tee) => {
                println!("[host] capturing audio from pid {pid}");
                Some(tee)
            }
            Err(err) => {
                eprintln!("[host] audio disabled: {err}");
                None
            }
        },
        None => None,
    };

    watch_bus(&pipeline, "host");
    // Do not let WGC emit its one guaranteed initial frame before a viewer
    // branch exists. READY keeps the graph prepared without starting capture.
    pipeline.set_state(gst::State::Ready)?;
    let mut pipeline_started = false;

    // One peer connection per viewer, keyed by the relay's peer id.
    let mut viewers: HashMap<String, ViewerBranch> = HashMap::new();

    // --- signalling loop --------------------------------------------------
    while let Some(signal) = client.incoming.recv().await {
        match signal {
            Signal::Hosting { code } => {
                println!("\n  Share this code:  {code}\n");
                println!("  Viewers run:  orange watch --code {code}\n");
            }
            Signal::ViewerJoined { peer, name } => {
                match add_viewer(
                    &pipeline,
                    &tee,
                    audio_tee.as_ref(),
                    &peer,
                    client.outgoing.clone(),
                ) {
                    Ok(branch) => {
                        viewers.insert(peer.clone(), branch);
                        if !pipeline_started {
                            pipeline.set_state(gst::State::Playing)?;
                            pipeline_started = true;
                        }
                        crate::targets::request_redraw(settings.hwnd);
                        println!(
                            "[host] {} joined ({} watching)",
                            name.clone().unwrap_or_else(|| format!("viewer {peer}")),
                            viewers.len()
                        );
                    }
                    Err(err) => eprintln!("[host] could not add viewer {peer}: {err}"),
                }
            }
            Signal::ViewerLeft { peer } => {
                if let Some(branch) = viewers.remove(&peer) {
                    remove_viewer(&pipeline, branch);
                    if viewers.is_empty() {
                        pipeline.set_state(gst::State::Ready)?;
                        pipeline_started = false;
                    }
                    println!("[host] viewer {peer} left ({} remaining)", viewers.len());
                }
            }
            Signal::Sdp { peer, kind, sdp } if kind == "answer" => {
                if let Some(branch) = viewers.get(&peer) {
                    let desc = parse_sdp(&kind, &sdp)?;
                    branch.bin.emit_by_name::<()>(
                        "set-remote-description",
                        &[&desc, &None::<gst::Promise>],
                    );
                    crate::targets::request_redraw(settings.hwnd);
                    force_key_unit(&tee);
                    println!("[host] streaming to {peer}");
                }
            }
            Signal::Ice {
                peer,
                mline,
                candidate,
            } => {
                if let Some(branch) = viewers.get(&peer) {
                    branch
                        .bin
                        .emit_by_name::<()>("add-ice-candidate", &[&mline, &candidate]);
                }
            }
            Signal::Authenticated { name } => println!("[host] signed in as {name}"),
            Signal::Error { message } => eprintln!("[host] server: {message}"),
            _ => {}
        }
    }

    pipeline.set_state(gst::State::Null)?;
    Ok(())
}

/// Attach a new viewer branch to the running pipeline and start negotiating.
/// Build the audio capture chain and return its tee, so each viewer can take
/// a branch from it.
fn build_audio_tee(pipeline: &gst::Pipeline, pid: u32) -> Result<gst::Element> {
    check_audio_elements()?;

    let chain = gst::parse::bin_from_description(&build_audio_chain(pid), true)
        .context("failed to build audio chain")?;
    let caps_filter = gst::ElementFactory::make("capsfilter")
        .property("caps", audio_rtp_caps())
        .build()?;
    let tee = gst::ElementFactory::make("tee")
        .property("allow-not-linked", true)
        .build()?;

    pipeline.add_many([chain.upcast_ref(), &caps_filter, &tee])?;
    gst::Element::link_many([chain.upcast_ref(), &caps_filter, &tee])?;
    Ok(tee)
}

/// Take a branch off a tee, through its own queue, into a viewer's webrtcbin.
///
/// The queue matters: without one per branch, a slow viewer would stall the
/// tee and with it every other viewer and the encoder itself.
struct TeeBranch {
    tee: gst::Element,
    tee_pad: gst::Pad,
    elements: Vec<gst::Element>,
    bin_pad: gst::Pad,
}

struct ViewerBranch {
    bin: gst::Element,
    links: Vec<TeeBranch>,
}

fn link_tee_branch(
    pipeline: &gst::Pipeline,
    tee: &gst::Element,
    bin: &gst::Element,
    max_buffers: u32,
    mut payload: Vec<gst::Element>,
) -> Result<TeeBranch> {
    let queue = gst::ElementFactory::make("queue")
        .property("max-size-buffers", max_buffers)
        .property_from_str("leaky", "downstream")
        .build()?;
    let mut elements = vec![queue];
    elements.append(&mut payload);
    for element in &elements {
        pipeline.add(element)?;
    }
    gst::Element::link_many(elements.iter().collect::<Vec<_>>().as_slice())?;

    let sink_pad = bin
        .request_pad_simple("sink_%u")
        .context("webrtcbin refused a sink pad")?;
    elements
        .last()
        .unwrap()
        .static_pad("src")
        .unwrap()
        .link(&sink_pad)?;

    let tee_pad = tee
        .request_pad_simple("src_%u")
        .context("tee refused a source pad")?;
    tee_pad.link(&elements[0].static_pad("sink").unwrap())?;

    for element in &elements {
        element.sync_state_with_parent()?;
    }
    Ok(TeeBranch {
        tee: tee.clone(),
        tee_pad,
        elements,
        bin_pad: sink_pad,
    })
}

fn add_viewer(
    pipeline: &gst::Pipeline,
    tee: &gst::Element,
    audio_tee: Option<&gst::Element>,
    peer: &str,
    out: mpsc::UnboundedSender<Signal>,
) -> Result<ViewerBranch> {
    let bin = make_webrtcbin(&format!("viewer-{peer}"))?;
    pipeline.add(&bin)?;

    let pay = gst::ElementFactory::make("rtpav1pay").build()?;
    let caps = gst::ElementFactory::make("capsfilter")
        .property("caps", rtp_caps())
        .build()?;
    let mut links = vec![link_tee_branch(pipeline, tee, &bin, 200, vec![pay, caps])?];
    if let Some(audio_tee) = audio_tee {
        links.push(link_tee_branch(pipeline, audio_tee, &bin, 50, Vec::new())?);
    }

    watch_connection(&bin, format!("host->{peer}"));
    forward_ice(&bin, out.clone(), peer.to_string());

    bin.sync_state_with_parent()?;

    create_offer(&bin, out, peer.to_string());
    Ok(ViewerBranch { bin, links })
}

fn remove_viewer(pipeline: &gst::Pipeline, branch: ViewerBranch) {
    let _ = branch.bin.set_state(gst::State::Null);
    for link in branch.links {
        for element in &link.elements {
            let _ = element.set_state(gst::State::Null);
        }
        let _ = link
            .tee_pad
            .unlink(&link.elements[0].static_pad("sink").unwrap());
        let _ = link
            .elements
            .last()
            .unwrap()
            .static_pad("src")
            .unwrap()
            .unlink(&link.bin_pad);
        link.tee.release_request_pad(&link.tee_pad);
        branch.bin.release_request_pad(&link.bin_pad);
        for element in link.elements {
            let _ = pipeline.remove(&element);
        }
    }
    let _ = pipeline.remove(&branch.bin);
}

fn force_key_unit(tee: &gst::Element) {
    let event = gst_video::UpstreamForceKeyUnitEvent::builder()
        .all_headers(true)
        .build();
    let _ = tee.send_event(event);
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
    if let Some(session) = crate::auth::load_session() {
        client.outgoing.send(Signal::Authenticate {
            session: session.token,
        })?;
    }
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

    // Media arrives as separate pads: one for video, one for audio. Only the
    // video pad consumes the output target.
    let pipeline_weak = pipeline.downgrade();
    // The audio branch needs the overlay to follow its volume control, so keep
    // a handle before the video branch consumes the output.
    let viewer_overlay = match &output {
        Output::Window { overlay, .. } => Some(overlay.clone()),
        Output::File(_) => None,
    };
    let viewer_hwnd = match &output {
        Output::Window { hwnd, .. } => Some(*hwnd),
        Output::File(_) => None,
    };
    let receive_audio = viewer_overlay
        .as_ref()
        .and_then(|overlay| overlay.lock().ok())
        .map(|state| !state.monitor_mode)
        .unwrap_or(true);
    let overlay_for_audio = viewer_overlay.clone();
    let overlay_for_video = viewer_overlay.clone();
    let output = std::sync::Arc::new(std::sync::Mutex::new(Some(output)));
    bin.connect_pad_added(move |_, pad| {
        let Some(pipeline) = pipeline_weak.upgrade() else {
            return;
        };
        let kind = encoding_name(pad).unwrap_or_default();
        let result = match kind.as_str() {
            "OPUS" if receive_audio => {
                build_audio_branch(&pipeline, pad, overlay_for_audio.clone())
            }
            "OPUS" => Ok(()),
            "AV1" | "H264" | "H265" => match output.lock().unwrap().take() {
                Some(output) => {
                    if let Some(overlay) = overlay_for_video.clone() {
                        watch_incoming_bitrate(pad, overlay);
                    }
                    build_receive_branch(&pipeline, pad, output)
                }
                None => Ok(()),
            },
            other => {
                eprintln!("[watch] ignoring unexpected stream '{other}'");
                Ok(())
            }
        };
        if let Err(err) = result {
            eprintln!("[watch] could not build {kind} branch: {err}");
        }
    });

    pipeline.set_state(gst::State::Playing)?;

    loop {
        let signal = if let Some(hwnd) = viewer_hwnd {
            match tokio::time::timeout(
                std::time::Duration::from_millis(100),
                client.incoming.recv(),
            )
            .await
            {
                Ok(signal) => signal,
                Err(_) if !crate::window::is_alive(hwnd) => break,
                Err(_) => continue,
            }
        } else {
            client.incoming.recv().await
        };
        let Some(signal) = signal else { break };
        match signal {
            Signal::Sdp { kind, sdp, .. } if kind == "offer" => {
                let desc = parse_sdp(&kind, &sdp)?;
                bin.emit_by_name::<()>("set-remote-description", &[&desc, &None::<gst::Promise>]);
                create_answer(&bin, client.outgoing.clone());
            }
            Signal::Ice {
                mline, candidate, ..
            } => {
                bin.emit_by_name::<()>("add-ice-candidate", &[&mline, &candidate]);
            }
            Signal::StreamInfo { host_name } => {
                if let Some(overlay) = &viewer_overlay {
                    if let Ok(mut state) = overlay.lock() {
                        state.host = host_name.clone();
                    }
                }
                if let Some(name) = host_name {
                    println!("[watch] {name}'s stream");
                }
            }

            Signal::Authenticated { name } => println!("[watch] signed in as {name}"),

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
