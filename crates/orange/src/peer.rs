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
use std::sync::Arc;
use tokio::sync::mpsc;

use crate::media_diagnostics::emit_diagnostic;
use orange_signal::Signal;

mod host;
mod host_branch;
mod watch;

pub(crate) use host::run_host;
pub(crate) use watch::run_watch;

/// Public STUN lets peers discover their external address. Without it, two
/// machines behind different routers will never find each other.
const STUN: &str = "stun://stun.l.google.com:19302";

struct PipelineError {
    source: String,
    message: String,
}

fn should_report_pipeline_error(playback_alive: Option<bool>) -> bool {
    playback_alive != Some(false)
}

fn combine_session_and_cleanup(session: Result<()>, cleanup: Result<()>) -> Result<()> {
    match (session, cleanup) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(error), Ok(())) | (Ok(()), Err(error)) => Err(error),
        (Err(session), Err(cleanup)) => Err(anyhow::anyhow!("{session:#}; {cleanup:#}")),
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn peer_worker_review_normal_teardown_failure_is_never_masked() {
        let success = combine_session_and_cleanup(Ok(()), Ok(()));
        let session_only =
            combine_session_and_cleanup(Err(anyhow::anyhow!("session-only failure")), Ok(()))
                .unwrap_err();
        let cleanup_only =
            combine_session_and_cleanup(Ok(()), Err(anyhow::anyhow!("cleanup-only failure")))
                .unwrap_err();
        let both = combine_session_and_cleanup(
            Err(anyhow::anyhow!("session failure")),
            Err(anyhow::anyhow!("cleanup failure")),
        )
        .unwrap_err();

        assert!(success.is_ok());
        assert_eq!(session_only.to_string(), "session-only failure");
        assert_eq!(cleanup_only.to_string(), "cleanup-only failure");
        let both = both.to_string();
        assert!(both.contains("session failure"));
        assert!(both.contains("cleanup failure"));
    }

    #[test]
    fn closed_playback_suppresses_teardown_bus_errors() {
        assert!(!super::should_report_pipeline_error(Some(false)));
        assert!(super::should_report_pipeline_error(Some(true)));
        assert!(super::should_report_pipeline_error(None));
    }

    #[test]
    fn video_transceiver_enables_retransmission() {
        gst::init().unwrap();
        let bin = super::make_webrtcbin("nack-test").unwrap();
        let pad = bin.request_pad_simple("sink_%u").unwrap();
        let transceiver = pad.property::<gst_webrtc::WebRTCRTPTransceiver>("transceiver");

        super::enable_nack(&transceiver);

        assert!(transceiver.property::<bool>("do-nack"));
        bin.release_request_pad(&pad);
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
}
