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
use std::io::Write;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};
use tokio::sync::mpsc;

use crate::media_diagnostics::{
    diagnostics_enabled, emit_diagnostic, start_webrtc_diagnostics, track_pad, DiagnosticsHandle,
    MediaProgress, MediaStage,
};
use crate::pipeline::{
    build_audio_chain, build_capture_chain, check_audio_elements, check_elements,
    configure_encoder, set_encoder_gop, CaptureSettings, Codec,
};
use crate::webrtc::{
    audio_rtp_caps, build_audio_branch, build_receive_branch, build_video_payloader,
    configure_receive_transport, encoding_name, rtp_caps, Output, ReceiveOutput,
};
use orange_signal::{connect, Signal};

/// Public STUN lets peers discover their external address. Without it, two
/// machines behind different routers will never find each other.
const STUN: &str = "stun://stun.l.google.com:19302";
const IDLE_REDRAW_INTERVAL: Duration = Duration::from_millis(50);
const IDLE_REDRAW_AFTER: Duration = Duration::from_millis(100);
const RECOVERY_KEYFRAME_INTERVAL: Duration = Duration::from_secs(2);
const AUDIO_BRANCH_MAX_PACKETS: u32 = 10;
static NEXT_DIAGNOSTIC_ID: AtomicU64 = AtomicU64::new(1);
static LAST_KEYFRAME_REQUEST: OnceLock<Mutex<Option<Instant>>> = OnceLock::new();

struct PipelineError {
    source: String,
    message: String,
}

fn should_report_pipeline_error(playback_alive: Option<bool>) -> bool {
    playback_alive != Some(false)
}

fn should_request_idle_redraw(viewer_count: usize, frame_silence: Duration) -> bool {
    viewer_count > 0 && frame_silence >= IDLE_REDRAW_AFTER
}

fn recovery_gop_size(frames: u64, elapsed: Duration, configured_fps: u32) -> Option<u32> {
    if frames == 0 || elapsed.is_zero() {
        return None;
    }
    let measured_fps = (frames as f64 / elapsed.as_secs_f64()).round() as u32;
    Some(
        measured_fps
            .clamp(1, configured_fps.max(1))
            .saturating_mul(2)
            .min(i32::MAX as u32),
    )
}

fn initial_host_gop_size(configured_fps: u32) -> u32 {
    configured_fps.max(1).saturating_mul(2).min(120)
}

fn gop_update(current: u32, measured: u32) -> Option<u32> {
    let threshold = (current / 10).max(4);
    (current.abs_diff(measured) > threshold).then_some(measured)
}

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
fn watch_bus(
    pipeline: &gst::Pipeline,
    label: &'static str,
    playback: Option<crate::window::PlaybackWindowHandle>,
) -> Result<(
    mpsc::UnboundedSender<PipelineError>,
    mpsc::UnboundedReceiver<PipelineError>,
)> {
    let bus = pipeline.bus().context("pipeline has no bus")?;
    let (errors, receiver) = mpsc::unbounded_channel();
    let bus_errors = errors.clone();
    let pipeline_weak = pipeline.downgrade();
    bus.set_sync_handler(move |_, msg| {
        match msg.view() {
            gst::MessageView::Error(err) => {
                if !should_report_pipeline_error(playback.as_ref().map(|window| window.is_alive()))
                {
                    return gst::BusSyncReply::Drop;
                }
                if err.error().to_string().contains("Output window was closed") {
                    return gst::BusSyncReply::Drop;
                }
                let source = msg.src().map(|s| s.path_string()).unwrap_or_default();
                let error = format!(
                    "GStreamer error from {source}: {} ({})",
                    err.error(),
                    err.debug().unwrap_or_default()
                );
                eprintln!("[{label}] ERROR: {error}");
                let _ = bus_errors.send(PipelineError {
                    source: source.to_string(),
                    message: error,
                });
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
                emit_diagnostic("pipeline-eos", label, serde_json::json!({}));
            }
            gst::MessageView::Latency(_) => {
                if let Some(pipeline) = pipeline_weak.upgrade() {
                    pipeline.call_async(move |pipeline| {
                        if let Err(error) = pipeline.recalculate_latency() {
                            eprintln!("[{label}] warning: could not recalculate latency: {error}");
                        } else {
                            emit_diagnostic(
                                "pipeline-latency-recalculated",
                                label,
                                serde_json::json!({}),
                            );
                        }
                    });
                }
            }
            _ => {}
        }
        gst::BusSyncReply::Drop
    });
    Ok((errors, receiver))
}

/// Log ICE and DTLS state transitions.
///
/// Without this, a failed connection is indistinguishable from a working one:
/// `pad-added` fires when the transceiver is created, which happens whether or
/// not any media ever arrives. The states below are the difference between
/// "negotiated" and "actually connected".
type ConnectionFailure = Arc<dyn Fn(String) + Send + Sync>;
type ConnectionReady = Arc<dyn Fn() + Send + Sync>;

fn is_terminal_connection_state(state: gst_webrtc::WebRTCPeerConnectionState) -> bool {
    matches!(
        state,
        gst_webrtc::WebRTCPeerConnectionState::Failed
            | gst_webrtc::WebRTCPeerConnectionState::Closed
    )
}

fn watch_connection(
    bin: &gst::Element,
    label: String,
    diagnostic_role: String,
    on_connected: Option<ConnectionReady>,
    on_failure: Option<ConnectionFailure>,
) {
    let l = label.clone();
    let role = diagnostic_role.clone();
    bin.connect_notify(Some("ice-connection-state"), move |bin, _| {
        let state = bin.property::<gst_webrtc::WebRTCICEConnectionState>("ice-connection-state");
        println!("[{l}] ice: {state:?}");
        emit_diagnostic("ice-state", &role, format!("{state:?}"));
    });

    let l = label.clone();
    let role = diagnostic_role.clone();
    bin.connect_notify(Some("ice-gathering-state"), move |bin, _| {
        let state = bin.property::<gst_webrtc::WebRTCICEGatheringState>("ice-gathering-state");
        println!("[{l}] gathering: {state:?}");
        emit_diagnostic("ice-gathering-state", &role, format!("{state:?}"));
    });

    bin.connect_notify(Some("connection-state"), move |bin, _| {
        let state = bin.property::<gst_webrtc::WebRTCPeerConnectionState>("connection-state");
        println!("[{label}] peer connection: {state:?}");
        emit_diagnostic(
            "peer-connection-state",
            &diagnostic_role,
            format!("{state:?}"),
        );
        if state == gst_webrtc::WebRTCPeerConnectionState::Connected {
            if let Some(on_connected) = &on_connected {
                on_connected();
            }
        }
        if is_terminal_connection_state(state) {
            if let Some(on_failure) = &on_failure {
                on_failure(format!("peer connection entered {state:?}"));
            }
        }
    });
}

fn enable_nack(transceiver: &gst_webrtc::WebRTCRTPTransceiver) {
    transceiver.set_property("do-nack", true);
}

fn enable_incoming_video_nack(bin: &gst::Element) {
    bin.connect("on-new-transceiver", false, move |values| {
        let Ok(transceiver) = values[1].get::<gst_webrtc::WebRTCRTPTransceiver>() else {
            return None;
        };
        let kind = transceiver.property::<gst_webrtc::WebRTCKind>("kind");
        if kind == gst_webrtc::WebRTCKind::Video {
            enable_nack(&transceiver);
        }
        let transceiver_for_kind = transceiver.clone();
        transceiver.connect_notify(Some("kind"), move |transceiver, _| {
            if transceiver.property::<gst_webrtc::WebRTCKind>("kind")
                == gst_webrtc::WebRTCKind::Video
            {
                enable_nack(&transceiver_for_kind);
            }
        });
        None
    });
}

fn check_promise_reply<'a>(
    reply: std::result::Result<Option<&'a gst::StructureRef>, gst::PromiseError>,
    operation: &str,
) -> Result<Option<&'a gst::StructureRef>> {
    let reply = reply.map_err(|err| anyhow::anyhow!("{operation} promise failed: {err:?}"))?;
    if let Some(reply) = reply {
        if let Ok(error) = reply.get::<gst::glib::Error>("error") {
            anyhow::bail!("{operation} failed: {error}");
        }
    }
    Ok(reply)
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
        let (Ok(mline), Ok(candidate)) = (values[1].get::<u32>(), values[2].get::<String>()) else {
            eprintln!("[webrtc] malformed ICE candidate callback");
            return None;
        };
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

fn handle_host_diagnostic_signal(signal: &Signal) {
    let Signal::Hosting {
        diagnostic_session: Some(diagnostic_session),
        ..
    } = signal
    else {
        return;
    };
    emit_diagnostic(
        "diagnostic-session",
        "host",
        serde_json::json!({ "id": diagnostic_session }),
    );
}

fn handle_watch_diagnostic_signal(signal: &Signal) {
    let Signal::StreamInfo {
        diagnostic_session: Some(diagnostic_session),
        ..
    } = signal
    else {
        return;
    };
    emit_diagnostic(
        "diagnostic-session",
        "watch",
        serde_json::json!({ "id": diagnostic_session }),
    );
}

#[cfg(test)]
#[allow(clippy::items_after_test_module)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn host_receipt_accepts_only_hosting_diagnostic_session() {
        let hosting = Signal::Hosting {
            code: "ROOM-CODE".to_string(),
            diagnostic_session: Some("host-session".to_string()),
        };
        let untrusted_stream_info = Signal::StreamInfo {
            host_name: Some("Viewer Supplied".to_string()),
            diagnostic_session: Some("viewer-controlled".to_string()),
        };

        let captured = crate::media_diagnostics::capture_diagnostics(|| {
            super::handle_host_diagnostic_signal(&hosting);
            super::handle_host_diagnostic_signal(&untrusted_stream_info);
        });

        assert_eq!(
            captured,
            [serde_json::json!({
                "event": "diagnostic-session",
                "role": "host",
                "payload": { "id": "host-session" },
            })]
        );
    }

    #[test]
    fn watch_receipt_accepts_only_stream_info_diagnostic_session() {
        let stream_info = Signal::StreamInfo {
            host_name: Some("Host Name".to_string()),
            diagnostic_session: Some("watch-session".to_string()),
        };
        let unexpected_hosting = Signal::Hosting {
            code: "ROOM-CODE".to_string(),
            diagnostic_session: Some("unexpected-host-session".to_string()),
        };

        let captured = crate::media_diagnostics::capture_diagnostics(|| {
            super::handle_watch_diagnostic_signal(&stream_info);
            super::handle_watch_diagnostic_signal(&unexpected_hosting);
        });

        assert_eq!(
            captured,
            [serde_json::json!({
                "event": "diagnostic-session",
                "role": "watch",
                "payload": { "id": "watch-session" },
            })]
        );
    }

    #[test]
    fn closed_playback_suppresses_teardown_bus_errors() {
        assert!(!super::should_report_pipeline_error(Some(false)));
        assert!(super::should_report_pipeline_error(Some(true)));
        assert!(super::should_report_pipeline_error(None));
    }

    #[test]
    fn idle_redraw_heartbeat_only_runs_with_viewers() {
        assert!(!super::should_request_idle_redraw(
            0,
            Duration::from_millis(500)
        ));
        assert!(!super::should_request_idle_redraw(
            1,
            Duration::from_millis(20)
        ));
        assert!(super::should_request_idle_redraw(
            1,
            Duration::from_millis(150)
        ));
        assert!(super::IDLE_REDRAW_INTERVAL <= Duration::from_millis(50));
    }

    #[test]
    fn video_transceiver_enables_retransmission() {
        gst::init().unwrap();
        let bin = super::make_webrtcbin("nack-test").unwrap();
        let pad = bin.request_pad_simple("sink_%u").unwrap();
        let transceiver = pad.property::<gst_webrtc::WebRTCRTPTransceiver>("transceiver");

        super::enable_nack(&transceiver);

        assert!(transceiver.property::<bool>("do-nack"));
    }

    #[test]
    fn generated_video_offer_maps_rtx_to_the_primary_payload() {
        gst::init().unwrap();
        let bin = super::make_webrtcbin("rtx-offer-test").unwrap();
        let caps = gst::ElementFactory::make("capsfilter")
            .property("caps", rtp_caps(Codec::Av1, 60))
            .build()
            .unwrap();
        let pad = bin.request_pad_simple("sink_%u").unwrap();
        let transceiver = pad.property::<gst_webrtc::WebRTCRTPTransceiver>("transceiver");
        super::enable_nack(&transceiver);
        let pipeline = gst::Pipeline::new();
        pipeline.add_many([&caps, &bin]).unwrap();
        caps.static_pad("src").unwrap().link(&pad).unwrap();
        pipeline.set_state(gst::State::Playing).unwrap();
        let (send, receive) = std::sync::mpsc::sync_channel(1);
        let promise = gst::Promise::with_change_func(move |reply| {
            let sdp = reply.ok().flatten().and_then(|reply| {
                reply
                    .value("offer")
                    .ok()?
                    .get::<gst_webrtc::WebRTCSessionDescription>()
                    .ok()?
                    .sdp()
                    .as_text()
                    .ok()
            });
            let _ = send.send(sdp);
        });

        bin.emit_by_name::<()>("create-offer", &[&None::<gst::Structure>, &promise]);
        let sdp = receive.recv_timeout(Duration::from_secs(5));
        pipeline.set_state(gst::State::Null).unwrap();
        let sdp = sdp.unwrap().expect("offer was not generated");

        assert!(sdp.contains("a=rtpmap:96 AV1/90000"));
        assert!(sdp.contains("a=rtpmap:97 rtx/90000"));
        assert!(sdp.contains("a=fmtp:97 apt=96"));
    }

    #[test]
    fn failed_or_closed_peer_connections_end_the_session() {
        assert!(super::is_terminal_connection_state(
            gst_webrtc::WebRTCPeerConnectionState::Failed
        ));
        assert!(super::is_terminal_connection_state(
            gst_webrtc::WebRTCPeerConnectionState::Closed
        ));
        assert!(!super::is_terminal_connection_state(
            gst_webrtc::WebRTCPeerConnectionState::Disconnected
        ));
        assert!(!super::is_terminal_connection_state(
            gst_webrtc::WebRTCPeerConnectionState::Connected
        ));
    }

    #[test]
    fn recovery_gop_tracks_the_measured_frame_rate() {
        assert_eq!(
            super::recovery_gop_size(120, Duration::from_secs(2), 240),
            Some(120)
        );
        assert_eq!(
            super::recovery_gop_size(480, Duration::from_secs(2), 240),
            Some(480)
        );
        assert_eq!(
            super::recovery_gop_size(30, Duration::from_secs(2), 240),
            Some(30)
        );
        assert_eq!(
            super::recovery_gop_size(0, Duration::from_secs(2), 240),
            None
        );
        assert_eq!(super::initial_host_gop_size(240), 120);
        assert_eq!(super::initial_host_gop_size(30), 60);
        assert_eq!(super::gop_update(120, 480), Some(480));
        assert_eq!(super::gop_update(480, 475), None);
        assert_eq!(super::gop_update(480, 120), Some(120));
    }

    #[test]
    fn failed_keyframe_send_rolls_back_without_holding_the_lock() {
        let requests = Mutex::new(None);
        let now = Instant::now();

        super::request_keyframe_at(&requests, now, || {
            assert!(requests.try_lock().is_ok());
            false
        });

        assert_eq!(*requests.lock().unwrap(), None);
    }

    #[test]
    fn keyframe_reservation_throttles_and_failed_rollback_preserves_newer_request() {
        let requests = Mutex::new(None);
        let now = Instant::now();
        let newer = now + Duration::from_millis(250);
        let mut sends = 0;

        super::request_keyframe_at(&requests, now, || {
            sends += 1;
            true
        });
        super::request_keyframe_at(&requests, now + Duration::from_millis(199), || {
            sends += 1;
            true
        });
        super::request_keyframe_at(&requests, now + Duration::from_millis(200), || {
            sends += 1;
            *requests.lock().unwrap() = Some(newer);
            false
        });

        assert_eq!(sends, 2);
        assert_eq!(*requests.lock().unwrap(), Some(newer));
    }

    #[tokio::test]
    async fn viewer_teardown_worker_removes_enqueued_branch() {
        gst::init().unwrap();
        let pipeline = gst::Pipeline::new();
        let bin = gst::ElementFactory::make("identity")
            .name("viewer-teardown-test")
            .build()
            .unwrap();
        pipeline.add(&bin).unwrap();
        let teardown = super::ViewerTeardown::new(&pipeline).unwrap();

        teardown
            .enqueue(ViewerBranch {
                bin,
                links: Vec::new(),
                label: "test viewer".to_string(),
                _diagnostics: None,
            })
            .await;
        drop(teardown);

        assert!(pipeline.by_name("viewer-teardown-test").is_none());
    }
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
    if let Some(session) = crate::auth::load_session()? {
        client.outgoing.send(Signal::Authenticate {
            session: session.token,
        })?;
    }
    client.outgoing.send(Signal::Host)?;

    // --- pipeline ---------------------------------------------------------
    let pipeline = gst::Pipeline::new();
    let capture = gst::parse::bin_from_description(&build_capture_chain(settings), true)
        .context("failed to build capture chain")?;
    let encoder = capture
        .by_name("stream-encoder")
        .context("capture chain has no named encoder")?;
    configure_encoder(&encoder, settings.codec, settings.fps);
    let initial_gop_size = initial_host_gop_size(settings.fps);
    set_encoder_gop(&encoder, initial_gop_size);
    let tee = gst::ElementFactory::make("tee")
        .property("allow-not-linked", true)
        .build()?;

    pipeline.add_many([capture.upcast_ref(), &tee])?;
    capture.link(&tee)?;
    let capture_clock = Instant::now();
    let last_video_frame = Arc::new(AtomicU64::new(0));
    let encoded_frames = Arc::new(AtomicU64::new(0));
    let last_video_frame_for_probe = last_video_frame.clone();
    let encoded_frames_for_probe = encoded_frames.clone();
    tee.static_pad("sink")
        .context("video tee has no sink pad")?
        .add_probe(gst::PadProbeType::BUFFER, move |_, _| {
            last_video_frame_for_probe.store(
                capture_clock
                    .elapsed()
                    .as_millis()
                    .min(u128::from(u64::MAX)) as u64,
                Ordering::Relaxed,
            );
            encoded_frames_for_probe.fetch_add(1, Ordering::Relaxed);
            gst::PadProbeReturn::Ok
        });

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

    let (_session_errors, mut bus_errors) = watch_bus(&pipeline, "host", None)?;
    let (viewer_failures, mut failed_viewers) = mpsc::unbounded_channel();
    let viewer_teardown = ViewerTeardown::new(&pipeline)?;
    // Do not let WGC emit its one guaranteed initial frame before a viewer
    // branch exists. READY keeps the graph prepared without starting capture.
    if let Err(error) = pipeline.set_state(gst::State::Ready) {
        let _ = pipeline.set_state(gst::State::Null);
        return Err(error.into());
    }
    let mut pipeline_started = false;

    // One peer connection per viewer, keyed by the relay's peer id.
    let mut viewers: HashMap<String, ViewerBranch> = HashMap::new();
    let mut idle_redraw = tokio::time::interval(IDLE_REDRAW_INTERVAL);
    idle_redraw.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut recovery_keyframe = tokio::time::interval(RECOVERY_KEYFRAME_INTERVAL);
    recovery_keyframe.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut gop_sampled_at = Instant::now();
    let mut current_gop_size = initial_gop_size;

    // --- signalling loop --------------------------------------------------
    let session_result: Result<()> = async {
        loop {
            let signal = tokio::select! {
                Some(error) = bus_errors.recv() => {
                    if let Some(peer) = viewers
                        .keys()
                        .find(|peer| error.source.contains(&format!("viewer-{peer}")))
                        .cloned()
                    {
                        let _ = viewer_failures.send((peer, error.message));
                        continue;
                    }
                    anyhow::bail!(error.message)
                },
                _ = idle_redraw.tick(), if !viewers.is_empty() => {
                    let now = capture_clock
                        .elapsed()
                        .as_millis()
                        .min(u128::from(u64::MAX)) as u64;
                    let last = last_video_frame.load(Ordering::Relaxed);
                    if should_request_idle_redraw(
                        viewers.len(),
                        Duration::from_millis(now.saturating_sub(last)),
                    ) {
                        crate::targets::request_redraw(settings.hwnd);
                    }
                    continue;
                }
                _ = recovery_keyframe.tick(), if !viewers.is_empty() => {
                    let elapsed = gop_sampled_at.elapsed();
                    gop_sampled_at = Instant::now();
                    let frames = encoded_frames.swap(0, Ordering::Relaxed);
                    if let Some(measured_gop_size) =
                        recovery_gop_size(frames, elapsed, settings.fps)
                    {
                        if let Some(gop_size) = gop_update(current_gop_size, measured_gop_size) {
                            set_encoder_gop(&encoder, gop_size);
                            current_gop_size = gop_size;
                        }
                        emit_diagnostic(
                            "encoder-gop",
                            "host",
                            serde_json::json!({
                                "sampled_frames": frames,
                                "sampled_ms": elapsed.as_millis(),
                                "measured_gop_size": measured_gop_size,
                                "active_gop_size": current_gop_size,
                            }),
                        );
                    }
                    force_key_unit(&tee);
                    continue;
                }
                Some((peer, error)) = failed_viewers.recv() => {
                    if let Some(branch) = viewers.remove(&peer) {
                        let label = branch.label.clone();
                        viewer_teardown.enqueue(branch).await;
                        print_viewer_status("left", &peer, &label);
                        println!(
                            "[host] {label} left ({} remaining)",
                            viewers.len()
                        );
                    }
                    eprintln!("[host] viewer {peer} negotiation failed: {error}");
                    continue;
                }
                signal = client.incoming.recv() => signal,
            };
            let Some(signal) = signal else { break };
            handle_host_diagnostic_signal(&signal);
            match signal {
                Signal::Hosting { code, .. } => {
                    println!("\n  Share this code:  {code}\n");
                    println!("  Viewers run:  orange watch --code {code}\n");
                }
                Signal::ViewerJoined { peer, name } => {
                    if viewers.contains_key(&peer) {
                        eprintln!("[host] ignoring duplicate join from viewer {peer}");
                        continue;
                    }
                    let first_active_viewer = viewers.is_empty();
                    if first_active_viewer {
                        encoded_frames.store(0, Ordering::Relaxed);
                        gop_sampled_at = Instant::now();
                        recovery_keyframe.reset();
                        if current_gop_size != initial_gop_size {
                            set_encoder_gop(&encoder, initial_gop_size);
                            current_gop_size = initial_gop_size;
                        }
                    }
                    let label = name.unwrap_or_else(|| format!("viewer {peer}"));
                    match add_viewer(
                        &pipeline,
                        &tee,
                        audio_tee.as_ref(),
                        &peer,
                        label.clone(),
                        settings.codec,
                        settings.fps,
                        client.outgoing.clone(),
                        viewer_failures.clone(),
                    ) {
                        Ok(branch) => {
                            viewers.insert(peer.clone(), branch);
                            if !pipeline_started {
                                pipeline.set_state(gst::State::Playing)?;
                                pipeline_started = true;
                            }
                            crate::targets::request_redraw(settings.hwnd);
                            println!("[host] {label} joined ({} watching)", viewers.len());
                            print_viewer_status("joined", &peer, &label);
                        }
                        Err(err) => eprintln!("[host] could not add viewer {peer}: {err}"),
                    }
                }
                Signal::ViewerLeft { peer } => {
                    if let Some(branch) = viewers.remove(&peer) {
                        let label = branch.label.clone();
                        viewer_teardown.enqueue(branch).await;
                        println!("[host] {label} left ({} remaining)", viewers.len());
                        print_viewer_status("left", &peer, &label);
                    }
                }
                Signal::Sdp { peer, kind, sdp } if kind == "answer" => {
                    if let Some(branch) = viewers.get(&peer) {
                        let desc = match parse_sdp(&kind, &sdp) {
                            Ok(desc) => desc,
                            Err(error) => {
                                let _ = viewer_failures
                                    .send((peer.clone(), format!("malformed answer: {error}")));
                                continue;
                            }
                        };
                        let viewer_failures = viewer_failures.clone();
                        let hwnd = settings.hwnd;
                        let peer = peer.clone();
                        let installed =
                            gst::Promise::with_change_func(move |reply| match check_promise_reply(
                                reply,
                                "installing remote answer",
                            ) {
                                Ok(_) => {
                                    crate::targets::request_redraw(hwnd);
                                    println!("[host] streaming to {peer}");
                                }
                                Err(error) => {
                                    let _ = viewer_failures.send((
                                        peer.clone(),
                                        format!("could not install answer: {error}"),
                                    ));
                                }
                            });
                        branch
                            .bin
                            .emit_by_name::<()>("set-remote-description", &[&desc, &installed]);
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
        Ok(())
    }
    .await;

    for (_, branch) in viewers.drain() {
        viewer_teardown.enqueue(branch).await;
    }
    drop(viewer_teardown);
    let stop_result = pipeline.set_state(gst::State::Null);
    client.close().await;
    session_result?;
    stop_result?;
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

    let elements = [chain.upcast_ref(), &caps_filter, &tee];
    let mut added = 0;
    let result = (|| -> Result<()> {
        for element in elements {
            pipeline.add(element)?;
            added += 1;
        }
        gst::Element::link_many(elements)?;
        Ok(())
    })();
    if let Err(error) = result {
        for element in elements.into_iter().take(added).rev() {
            let _ = element.set_state(gst::State::Null);
            let _ = pipeline.remove(element);
        }
        return Err(error);
    }
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
    label: String,
    _diagnostics: Option<DiagnosticsHandle>,
}

struct ViewerTeardown {
    sender: Option<mpsc::Sender<ViewerBranch>>,
    pipeline: gst::Pipeline,
    worker: Option<std::thread::JoinHandle<()>>,
}

impl ViewerTeardown {
    fn new(pipeline: &gst::Pipeline) -> Result<Self> {
        let (sender, mut receiver) = mpsc::channel(1);
        let worker_pipeline = pipeline.clone();
        let worker = std::thread::Builder::new()
            .name("viewer-teardown".to_string())
            .spawn(move || {
                while let Some(branch) = receiver.blocking_recv() {
                    if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        remove_viewer(&worker_pipeline, branch);
                    }))
                    .is_err()
                    {
                        let _ =
                            writeln!(std::io::stderr().lock(), "[host] viewer teardown panicked");
                    }
                }
            })
            .context("failed to spawn viewer teardown worker")?;
        Ok(Self {
            sender: Some(sender),
            pipeline: pipeline.clone(),
            worker: Some(worker),
        })
    }

    async fn enqueue(&self, branch: ViewerBranch) {
        let sender = self
            .sender
            .as_ref()
            .expect("sender exists until viewer teardown drop");
        if let Err(error) = sender.send(branch).await {
            remove_viewer(&self.pipeline, error.0);
        }
    }
}

impl Drop for ViewerTeardown {
    fn drop(&mut self) {
        self.sender.take();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

fn link_tee_branch(
    pipeline: &gst::Pipeline,
    tee: &gst::Element,
    bin: &gst::Element,
    max_buffers: u32,
    retransmit: bool,
    progress: Option<Arc<MediaProgress>>,
    mut payload: Vec<gst::Element>,
) -> Result<TeeBranch> {
    let queue = gst::ElementFactory::make("queue")
        .property("max-size-buffers", max_buffers)
        .property_from_str("leaky", "downstream")
        .build()?;
    if let Some(progress) = &progress {
        let progress = progress.clone();
        queue.connect("overrun", false, move |_| {
            progress.record_queue_overrun();
            None
        });
    }
    let mut elements = vec![queue];
    elements.append(&mut payload);

    let sink_pad = bin
        .request_pad_simple("sink_%u")
        .context("webrtcbin refused a sink pad")?;
    if retransmit {
        let transceiver = sink_pad.property::<gst_webrtc::WebRTCRTPTransceiver>("transceiver");
        enable_nack(&transceiver);
    }
    let Some(tee_pad) = tee.request_pad_simple("src_%u") else {
        bin.release_request_pad(&sink_pad);
        anyhow::bail!("tee refused a source pad");
    };
    let branch = TeeBranch {
        tee: tee.clone(),
        tee_pad,
        elements,
        bin_pad: sink_pad,
    };
    if let Some(progress) = progress {
        track_pad(
            &branch.elements[0].static_pad("sink").unwrap(),
            MediaStage::Parsed,
            progress.clone(),
        );
        track_pad(
            &branch.elements.last().unwrap().static_pad("src").unwrap(),
            MediaStage::Rtp,
            progress,
        );
    }

    let result = (|| -> Result<()> {
        for element in &branch.elements {
            pipeline.add(element)?;
        }
        gst::Element::link_many(branch.elements.iter().collect::<Vec<_>>().as_slice())?;
        branch
            .elements
            .last()
            .unwrap()
            .static_pad("src")
            .unwrap()
            .link(&branch.bin_pad)?;

        Ok(())
    })();
    if let Err(error) = result {
        remove_tee_branch(pipeline, bin, branch);
        return Err(error);
    }
    Ok(branch)
}

// Keeping the media branch inputs explicit makes their ownership and teardown
// order visible; grouping them would only move this lifecycle-sensitive API.
#[allow(clippy::too_many_arguments)]
fn add_viewer(
    pipeline: &gst::Pipeline,
    tee: &gst::Element,
    audio_tee: Option<&gst::Element>,
    peer: &str,
    label: String,
    codec: Codec,
    frame_rate: u32,
    out: mpsc::UnboundedSender<Signal>,
    failures: mpsc::UnboundedSender<(String, String)>,
) -> Result<ViewerBranch> {
    let bin = make_webrtcbin(&format!("viewer-{peer}"))?;
    pipeline.add(&bin)?;
    let mut branch = ViewerBranch {
        bin,
        links: Vec::new(),
        label,
        _diagnostics: None,
    };
    let progress = diagnostics_enabled().then(|| Arc::new(MediaProgress::new()));

    let result = (|| -> Result<()> {
        let pay = build_video_payloader(codec)?;
        let caps = gst::ElementFactory::make("capsfilter")
            .property("caps", rtp_caps(codec, frame_rate))
            .build()?;
        branch.links.push(link_tee_branch(
            pipeline,
            tee,
            &branch.bin,
            200,
            true,
            progress.clone(),
            vec![pay, caps],
        )?);
        if let Some(audio_tee) = audio_tee {
            branch.links.push(link_tee_branch(
                pipeline,
                audio_tee,
                &branch.bin,
                AUDIO_BRANCH_MAX_PACKETS,
                false,
                None,
                Vec::new(),
            )?);
        }
        branch.bin.sync_state_with_parent()?;
        for link in &branch.links {
            for element in &link.elements {
                element.sync_state_with_parent()?;
            }
            link.tee_pad
                .link(&link.elements[0].static_pad("sink").unwrap())?;
        }
        Ok(())
    })();
    if let Err(error) = result {
        remove_viewer(pipeline, branch);
        return Err(error);
    }

    let diagnostic_label = format!(
        "host-viewer-{}",
        NEXT_DIAGNOSTIC_ID.fetch_add(1, Ordering::Relaxed)
    );
    let failed_peer = peer.to_string();
    let connection_failures = failures.clone();
    let on_connection_failure: ConnectionFailure = Arc::new(move |error| {
        let _ = connection_failures.send((failed_peer.clone(), error));
    });
    let tee_for_connected = tee.downgrade();
    let bin_for_connected = branch.bin.downgrade();
    let started_keyframes = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let on_connected: ConnectionReady = Arc::new(move || {
        if started_keyframes.swap(true, Ordering::AcqRel) {
            return;
        }
        request_startup_keyframes(tee_for_connected.clone(), bin_for_connected.clone());
    });
    watch_connection(
        &branch.bin,
        format!("host->{peer}"),
        diagnostic_label.clone(),
        Some(on_connected),
        Some(on_connection_failure),
    );
    branch._diagnostics = start_webrtc_diagnostics(&branch.bin, diagnostic_label, progress, None);
    forward_ice(&branch.bin, out.clone(), peer.to_string());

    create_offer(&branch.bin, out, failures, peer.to_string());
    Ok(branch)
}

fn remove_viewer(pipeline: &gst::Pipeline, branch: ViewerBranch) {
    for link in &branch.links {
        block_and_unlink_tee_branch(link);
    }
    let _ = branch.bin.set_state(gst::State::Null);
    for link in branch.links {
        remove_tee_branch(pipeline, &branch.bin, link);
    }
    let _ = pipeline.remove(&branch.bin);
}

fn block_and_unlink_tee_branch(branch: &TeeBranch) {
    let Some(sink_pad) = branch.elements[0].static_pad("sink") else {
        return;
    };
    if !branch.tee_pad.is_linked() {
        return;
    }

    let (blocked, wait_for_block) = std::sync::mpsc::sync_channel(1);
    let probe = branch
        .tee_pad
        .add_probe(gst::PadProbeType::IDLE, move |_, _| {
            let _ = blocked.try_send(());
            gst::PadProbeReturn::Ok
        });
    if probe.is_some() {
        let _ = wait_for_block.recv_timeout(Duration::from_secs(1));
    }
    let _ = branch.tee_pad.unlink(&sink_pad);
    if let Some(probe) = probe {
        branch.tee_pad.remove_probe(probe);
    }
}

fn remove_tee_branch(pipeline: &gst::Pipeline, bin: &gst::Element, branch: TeeBranch) {
    for element in &branch.elements {
        let _ = element.set_state(gst::State::Null);
    }
    if let Some(src_pad) = branch
        .elements
        .last()
        .and_then(|element| element.static_pad("src"))
    {
        let _ = src_pad.unlink(&branch.bin_pad);
    }
    branch.tee.release_request_pad(&branch.tee_pad);
    bin.release_request_pad(&branch.bin_pad);
    for element in branch.elements {
        let _ = pipeline.remove(&element);
    }
}

fn request_keyframe_at(
    requests: &Mutex<Option<Instant>>,
    now: Instant,
    send: impl FnOnce() -> bool,
) {
    {
        let mut last_request = requests
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if last_request.is_some_and(|last| now.duration_since(last) < Duration::from_millis(200)) {
            return;
        }
        *last_request = Some(now);
    }

    if !send() {
        let mut last_request = requests
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if *last_request == Some(now) {
            *last_request = None;
        }
    }
}

fn force_key_unit(tee: &gst::Element) {
    let event = gst_video::UpstreamForceKeyUnitEvent::builder()
        .all_headers(true)
        .build();
    request_keyframe_at(
        LAST_KEYFRAME_REQUEST.get_or_init(|| Mutex::new(None)),
        Instant::now(),
        || tee.send_event(event),
    );
}

fn request_startup_keyframes(
    tee: gst::glib::WeakRef<gst::Element>,
    bin: gst::glib::WeakRef<gst::Element>,
) {
    if let Some(tee) = tee.upgrade() {
        force_key_unit(&tee);
    }
    std::thread::spawn(move || {
        for delay in [250, 500, 750] {
            std::thread::sleep(Duration::from_millis(delay));
            let Some(tee) = tee.upgrade() else { break };
            let Some(bin) = bin.upgrade() else { break };
            if bin.property::<gst_webrtc::WebRTCPeerConnectionState>("connection-state")
                != gst_webrtc::WebRTCPeerConnectionState::Connected
            {
                break;
            }
            force_key_unit(&tee);
        }
    });
}

fn print_viewer_status(event: &str, peer: &str, label: &str) {
    println!(
        "[host-status] {}",
        serde_json::json!({ "event": event, "peer": peer, "label": label })
    );
}

fn create_offer(
    bin: &gst::Element,
    out: mpsc::UnboundedSender<Signal>,
    failures: mpsc::UnboundedSender<(String, String)>,
    peer: String,
) {
    let bin_clone = bin.clone();
    let promise = gst::Promise::with_change_func(move |reply| {
        let reply = match check_promise_reply(reply, "creating offer") {
            Ok(Some(reply)) => reply,
            Ok(None) => {
                let _ = failures.send((
                    peer.clone(),
                    "creating offer returned no description".into(),
                ));
                return;
            }
            Err(error) => {
                let _ = failures.send((peer.clone(), error.to_string()));
                return;
            }
        };
        let Ok(offer_value) = reply.value("offer") else {
            let _ = failures.send((
                peer.clone(),
                "creating offer returned no description".into(),
            ));
            return;
        };
        let Ok(offer) = offer_value.get::<gst_webrtc::WebRTCSessionDescription>() else {
            let _ = failures.send((
                peer.clone(),
                "creating offer returned an invalid description".into(),
            ));
            return;
        };
        let sdp = offer.sdp().as_text().unwrap_or_default();
        let installed = gst::Promise::with_change_func(move |reply| {
            match check_promise_reply(reply, "installing local offer") {
                Ok(_) => {
                    let _ = out.send(Signal::Sdp {
                        peer,
                        kind: "offer".into(),
                        sdp,
                    });
                }
                Err(error) => {
                    let _ = failures.send((peer, error.to_string()));
                }
            }
        });
        bin_clone.emit_by_name::<()>("set-local-description", &[&offer, &installed]);
    });
    bin.emit_by_name::<()>("create-offer", &[&None::<gst::Structure>, &promise]);
}

/// Viewer: join a stream by code.
pub async fn run_watch(code: &str, url: &str, output: Output) -> Result<()> {
    // Keep the unique owner outside every callback and declare it before the
    // pipeline so explicit Null teardown precedes HWND destruction.
    let (playback_owner, output) = match output {
        Output::Window(owner) => {
            let handle = owner.handle();
            (Some(owner), ReceiveOutput::Window(handle))
        }
        Output::File(path) => (None, ReceiveOutput::File(path)),
    };
    let viewer_playback = playback_owner.as_ref().map(|owner| owner.handle());
    let mut client = connect(url).await?;
    if let Some(session) = crate::auth::load_session()? {
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
    configure_receive_transport(&bin, matches!(&output, ReceiveOutput::Window(_)))?;
    pipeline.add(&bin)?;
    let (session_errors, mut bus_errors) = watch_bus(&pipeline, "watch", viewer_playback.clone())?;

    let connection_errors = session_errors.clone();
    let on_connection_failure: ConnectionFailure = Arc::new(move |error| {
        let _ = connection_errors.send(PipelineError {
            source: String::new(),
            message: error,
        });
    });
    watch_connection(
        &bin,
        "watch".to_string(),
        "watch".to_string(),
        None,
        Some(on_connection_failure),
    );
    enable_incoming_video_nack(&bin);
    forward_ice(&bin, client.outgoing.clone(), String::new());

    // Media arrives as separate pads: one for video, one for audio. Only the
    // video pad consumes the output target.
    let pipeline_weak = pipeline.downgrade();
    // The audio branch needs the overlay to follow its volume control, so keep
    // a handle before the video branch consumes the output.
    let viewer_overlay = viewer_playback
        .as_ref()
        .map(|playback| playback.overlay().clone());
    let overlay_for_audio = viewer_overlay.clone();
    let overlay_for_video = viewer_overlay.clone();
    let media_progress = diagnostics_enabled().then(|| Arc::new(MediaProgress::new()));
    let media_progress_for_pad = media_progress.clone();
    let output = std::sync::Arc::new(std::sync::Mutex::new(Some(output)));
    let branch_errors = session_errors.clone();
    bin.connect_pad_added(move |_, pad| {
        let Some(pipeline) = pipeline_weak.upgrade() else {
            return;
        };
        let kind = encoding_name(pad).unwrap_or_default();
        emit_diagnostic(
            "pad-added",
            "watch",
            serde_json::json!({ "encoding": &kind }),
        );
        let (result, branch_ready) = match kind.as_str() {
            "OPUS" => (
                build_audio_branch(&pipeline, pad, overlay_for_audio.clone(), "watch"),
                true,
            ),
            "AV1" | "H264" | "H265" => match output.lock().unwrap().take() {
                Some(output) => {
                    if let Some(progress) = &media_progress_for_pad {
                        track_pad(pad, MediaStage::Rtp, progress.clone());
                    }
                    if let Some(overlay) = overlay_for_video.clone() {
                        watch_incoming_bitrate(pad, overlay);
                    }
                    (
                        build_receive_branch(
                            &pipeline,
                            pad,
                            output,
                            media_progress_for_pad.clone(),
                            "watch",
                        ),
                        true,
                    )
                }
                None => (Ok(()), false),
            },
            other => {
                eprintln!("[watch] ignoring unexpected stream '{other}'");
                (Ok(()), false)
            }
        };
        if let Err(err) = result {
            eprintln!("[watch] could not build {kind} branch: {err}");
            let _ = branch_errors.send(PipelineError {
                source: String::new(),
                message: format!("could not build {kind} receive branch: {err}"),
            });
        } else if branch_ready {
            emit_diagnostic(
                "receive-branch-ready",
                "watch",
                serde_json::json!({ "encoding": &kind }),
            );
        }
    });

    if let Err(error) = pipeline.set_state(gst::State::Playing) {
        let _ = pipeline.set_state(gst::State::Null);
        return Err(error.into());
    }
    let _diagnostics = start_webrtc_diagnostics(
        &bin,
        "watch".to_string(),
        media_progress,
        viewer_playback.clone(),
    );

    let session_result: Result<()> = async {
        loop {
            let signal = if let Some(playback) = &viewer_playback {
                tokio::select! {
                    Some(error) = bus_errors.recv() => {
                        if !playback.is_alive() {
                            break;
                        }
                        anyhow::bail!(error.message)
                    },
                    signal = tokio::time::timeout(
                        std::time::Duration::from_millis(100),
                        client.incoming.recv(),
                    ) => match signal {
                        Ok(signal) => signal,
                        Err(_) if !playback.is_alive() => break,
                        Err(_) => continue,
                    },
                }
            } else {
                tokio::select! {
                    Some(error) = bus_errors.recv() => anyhow::bail!(error.message),
                    signal = client.incoming.recv() => signal,
                }
            };
            let Some(signal) = signal else { break };
            handle_watch_diagnostic_signal(&signal);
            match signal {
                Signal::Sdp { kind, sdp, .. } if kind == "offer" => {
                    let desc = parse_sdp(&kind, &sdp)?;
                    let bin_for_answer = bin.clone();
                    let outgoing = client.outgoing.clone();
                    let session_errors = session_errors.clone();
                    let installed = gst::Promise::with_change_func(move |reply| {
                        match check_promise_reply(reply, "installing remote offer") {
                            Ok(_) => {
                                create_answer(&bin_for_answer, outgoing, session_errors.clone())
                            }
                            Err(error) => {
                                let _ = session_errors.send(PipelineError {
                                    source: String::new(),
                                    message: format!("could not install host offer: {error}"),
                                });
                            }
                        }
                    });
                    bin.emit_by_name::<()>("set-remote-description", &[&desc, &installed]);
                }
                Signal::Ice {
                    mline, candidate, ..
                } => {
                    bin.emit_by_name::<()>("add-ice-candidate", &[&mline, &candidate]);
                }
                Signal::StreamInfo { host_name, .. } => {
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
        Ok(())
    }
    .await;

    drop(_diagnostics);
    let stop_result = pipeline.set_state(gst::State::Null);
    client.close().await;
    session_result?;
    stop_result?;
    Ok(())
}

fn create_answer(
    bin: &gst::Element,
    out: mpsc::UnboundedSender<Signal>,
    errors: mpsc::UnboundedSender<PipelineError>,
) {
    let bin_clone = bin.clone();
    let promise = gst::Promise::with_change_func(move |reply| {
        let reply = match check_promise_reply(reply, "creating answer") {
            Ok(Some(reply)) => reply,
            Ok(None) => {
                let _ = errors.send(PipelineError {
                    source: String::new(),
                    message: "creating answer returned no description".into(),
                });
                return;
            }
            Err(error) => {
                let _ = errors.send(PipelineError {
                    source: String::new(),
                    message: error.to_string(),
                });
                return;
            }
        };
        let Ok(answer_value) = reply.value("answer") else {
            let _ = errors.send(PipelineError {
                source: String::new(),
                message: "creating answer returned no description".into(),
            });
            return;
        };
        let Ok(answer) = answer_value.get::<gst_webrtc::WebRTCSessionDescription>() else {
            let _ = errors.send(PipelineError {
                source: String::new(),
                message: "creating answer returned an invalid description".into(),
            });
            return;
        };
        let sdp = answer.sdp().as_text().unwrap_or_default();
        let installed = gst::Promise::with_change_func(move |reply| {
            match check_promise_reply(reply, "installing local answer") {
                Ok(_) => {
                    let _ = out.send(Signal::Sdp {
                        peer: String::new(),
                        kind: "answer".into(),
                        sdp,
                    });
                }
                Err(error) => {
                    let _ = errors.send(PipelineError {
                        source: String::new(),
                        message: error.to_string(),
                    });
                }
            }
        });
        bin_clone.emit_by_name::<()>("set-local-description", &[&answer, &installed]);
    });
    bin.emit_by_name::<()>("create-answer", &[&None::<gst::Structure>, &promise]);
}
