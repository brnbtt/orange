use anyhow::Result;
use gst::prelude::*;
use gstreamer as gst;
use gstreamer_webrtc as gst_webrtc;
use std::sync::Arc;

use crate::media_diagnostics::{
    diagnostics_enabled, emit_diagnostic, start_webrtc_diagnostics, track_pad, DiagnosticsHandle,
    MediaProgress, MediaStage,
};
use crate::webrtc::{
    accept_receive_pad, build_audio_branch, build_receive_branch, configure_receive_transport,
    encoding_name, watch_incoming_bitrate, AcceptedReceivePad, Output, ReceiveOutput,
    ReceiveWorkerRegistry,
};
use orange_signal::{connect, Signal};

use super::{
    check_promise_reply, combine_session_and_cleanup, enable_nack, forward_ice, make_webrtcbin,
    parse_sdp, watch_bus, watch_connection, ConnectionFailure, PipelineError,
};

/// Printed when a stream we were watching finishes normally.
///
/// The tray reads this off stdout. Without it, a host stopping and a viewer
/// closing their own window are the same thing from the outside: a child that
/// exited zero. They are not the same thing to the person watching, so the
/// difference has to be said out loud rather than inferred from an exit code
/// that only has two values.
///
/// The mirror of this string lives in `orange-tray`'s supervisor, which is the
/// same arrangement as `[host-status]` and `Share this code:`.
const WATCH_ENDED: &str = "[watch-status] ended";

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

async fn cleanup_watch_start_failure(
    start_error: anyhow::Error,
    cleanup: impl std::future::Future<Output = Result<()>>,
) -> Result<()> {
    combine_session_and_cleanup(Err(start_error), cleanup.await)
}

/// Viewer: join a stream by code.
async fn stop_watch_receive_pipeline(
    bin: &gst::Element,
    pad_added: gst::glib::SignalHandlerId,
    workers: ReceiveWorkerRegistry,
    diagnostics: Option<DiagnosticsHandle>,
    pipeline: &gst::Pipeline,
) -> Result<()> {
    bin.disconnect(pad_added);
    let pipeline = pipeline.clone();
    tokio::task::spawn_blocking(move || {
        let worker_result = workers.shutdown();
        drop(diagnostics);
        let stop_result = pipeline
            .set_state(gst::State::Null)
            .map(|_| ())
            .map_err(anyhow::Error::from);
        combine_session_and_cleanup(worker_result, stop_result)
    })
    .await
    .map_err(|error| anyhow::anyhow!("watch receive teardown task failed: {error}"))?
}

pub(crate) async fn run_watch(code: &str, url: &str, output: Output) -> Result<()> {
    // Keep the unique owner outside every callback and declare it before the
    // pipeline so explicit Null teardown precedes HWND destruction.
    let (playback_owner, output) = match output {
        Output::Window(owner) => {
            let handle = owner.handle();
            (Some(owner), ReceiveOutput::Window(handle))
        }
        Output::File(path) => (
            None,
            ReceiveOutput::File {
                path,
                encoder: None,
            },
        ),
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
    let receive_workers = ReceiveWorkerRegistry::new();
    let workers_for_pad = receive_workers.clone();
    let pad_added = bin.connect_pad_added(move |_, pad| {
        let Some(pipeline) = pipeline_weak.upgrade() else {
            return;
        };
        let kind = encoding_name(pad).unwrap_or_default();
        emit_diagnostic(
            "pad-added",
            "watch",
            serde_json::json!({ "encoding": &kind }),
        );
        let result: Result<bool> = match accept_receive_pad(&workers_for_pad, pad) {
            Some(AcceptedReceivePad::Audio(claim)) => {
                if let Some(progress) = &media_progress_for_pad {
                    track_pad(pad, MediaStage::AudioRtp, progress.clone());
                }
                build_audio_branch(
                    &pipeline,
                    pad,
                    overlay_for_audio.clone(),
                    media_progress_for_pad.clone(),
                    "watch",
                )
                .map(|worker| {
                    claim.complete_audio(worker);
                    true
                })
            }
            Some(AcceptedReceivePad::Video(claim)) => {
                match output
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .take()
                {
                    Some(output) => {
                        if let Some(progress) = &media_progress_for_pad {
                            track_pad(pad, MediaStage::Rtp, progress.clone());
                        }
                        build_receive_branch(
                            &pipeline,
                            pad,
                            output,
                            media_progress_for_pad.clone(),
                            "watch",
                        )
                        .map(|()| {
                            let bitrate = overlay_for_video.clone().and_then(|overlay| {
                                match watch_incoming_bitrate(pad, overlay) {
                                    Ok(worker) => Some(worker),
                                    Err(error) => {
                                        eprintln!(
                                            "[watch] incoming bitrate telemetry disabled: {error}"
                                        );
                                        None
                                    }
                                }
                            });
                            claim.complete_video(bitrate);
                            true
                        })
                    }
                    None => {
                        claim.complete_video(None);
                        Ok(false)
                    }
                }
            }
            None => {
                eprintln!("[watch] ignoring duplicate or unexpected stream '{kind}'");
                Ok(false)
            }
        };
        match result {
            Err(error) => {
                eprintln!("[watch] could not build {kind} branch: {error}");
                let _ = branch_errors.send(PipelineError {
                    source: String::new(),
                    message: format!("could not build {kind} receive branch: {error}"),
                });
            }
            Ok(true) => emit_diagnostic(
                "receive-branch-ready",
                "watch",
                serde_json::json!({ "encoding": &kind }),
            ),
            Ok(false) => {}
        }
    });

    if let Err(error) = pipeline.set_state(gst::State::Playing) {
        return cleanup_watch_start_failure(
            error.into(),
            stop_watch_receive_pipeline(&bin, pad_added, receive_workers, None, &pipeline),
        )
        .await;
    }
    let diagnostics = start_webrtc_diagnostics(
        &bin,
        "watch".to_string(),
        media_progress,
        viewer_playback.clone(),
    );

    let session_result: Result<()> = async {
        // Whether the relay has put us in a room yet. It reports a room closing
        // the same way it reports a bad code - a plain Error with prose in it -
        // so the only thing separating the two is when they arrive. Before
        // StreamInfo we are still trying to join, and an Error means we failed.
        // After it, we are in the room, and the only thing the relay has left to
        // tell us is that the host is gone.
        let mut joined = false;
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
                    joined = true;
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
                    if joined {
                        // A stream you were watching finishing is the ordinary
                        // end of a session, not a failure of one. Saying so on
                        // stdout lets the tray tell it apart from the viewer
                        // closing their own window, which also exits cleanly.
                        println!("{WATCH_ENDED}");
                        break;
                    }
                    anyhow::bail!("{message}");
                }
                _ => {}
            }
        }
        Ok(())
    }
    .await;

    let stop_result =
        stop_watch_receive_pipeline(&bin, pad_added, receive_workers, diagnostics, &pipeline).await;
    client.close().await;
    combine_session_and_cleanup(session_result, stop_result)
}

fn create_answer(
    bin: &gst::Element,
    out: tokio::sync::mpsc::UnboundedSender<Signal>,
    errors: tokio::sync::mpsc::UnboundedSender<PipelineError>,
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    #[tokio::test]
    async fn peer_worker_review_playing_failure_runs_cleanup_and_reports_both_errors() {
        let order = Arc::new(Mutex::new(vec!["playing-failed"]));
        let order_for_cleanup = order.clone();

        let result = cleanup_watch_start_failure(anyhow::anyhow!("playing failed"), async move {
            order_for_cleanup.lock().unwrap().push("cleanup");
            Err(anyhow::anyhow!("cleanup failed"))
        })
        .await;

        assert_eq!(*order.lock().unwrap(), ["playing-failed", "cleanup"]);
        let message = result.unwrap_err().to_string();
        assert!(message.contains("playing failed"));
        assert!(message.contains("cleanup failed"));
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
}
