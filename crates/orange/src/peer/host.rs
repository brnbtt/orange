use anyhow::{Context, Result};
use gst::prelude::*;
use gstreamer as gst;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;

use crate::media_diagnostics::emit_diagnostic;
use crate::pipeline::{
    build_audio_chain, build_capture_chain, check_audio_elements, check_elements,
    configure_encoder, set_encoder_gop, CaptureSettings,
};
use crate::webrtc::audio_rtp_caps;
use orange_signal::{connect, Signal};

use super::host_branch::{add_viewer, force_key_unit, ViewerBranch, ViewerTeardown};
use super::{check_promise_reply, combine_session_and_cleanup, parse_sdp, watch_bus};

const IDLE_REDRAW_INTERVAL: Duration = Duration::from_millis(50);
const IDLE_REDRAW_AFTER: Duration = Duration::from_millis(100);
const RECOVERY_KEYFRAME_INTERVAL: Duration = Duration::from_secs(2);

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

/// Host: capture a window and serve any number of viewers.
///
/// The window is captured and encoded **once**. Encoded video is fanned out to
/// a fresh RTP payloader per viewer, so late joiners receive their own RTP
/// stream and initialization while still sharing the expensive encoder.
pub(crate) async fn run_host(
    settings: &CaptureSettings,
    url: &str,
    visible_to: Vec<String>,
) -> Result<()> {
    check_elements(settings)?;
    let mut client = connect(url).await?;

    // Identity is optional: without it viewers show up as opaque ids.
    if let Some(session) = crate::auth::load_session()? {
        client.outgoing.send(Signal::Authenticate {
            session: session.token,
        })?;
    }
    client.outgoing.send(Signal::Host { visible_to })?;

    // --- pipeline ---------------------------------------------------------
    let pipeline = gst::Pipeline::new();
    let capture = gst::parse::bin_from_description(&build_capture_chain(settings), true)
        .context("failed to build capture chain")?;
    let encoder = capture
        .by_name("stream-encoder")
        .context("capture chain has no named encoder")?;
    configure_encoder(&encoder, settings.encoder, settings.codec, settings.fps);
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

    // --- signalling loop --------------------------------------------------
    let session_result: Result<()> =
        async {
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
                            // Changing mfh265enc properties while PLAYING drains and
                            // reinitializes its MFT, so the sampled rate is diagnostic only.
                            emit_diagnostic(
                                "encoder-gop",
                                "host",
                                serde_json::json!({
                                    "sampled_frames": frames,
                                    "sampled_ms": elapsed.as_millis(),
                                    "measured_gop_size": measured_gop_size,
                                    "active_gop_size": initial_gop_size,
                                }),
                            );
                        }
                        force_key_unit(&tee);
                        continue;
                    }
                    Some((peer, error)) = failed_viewers.recv() => {
                        if let Some(branch) = viewers.remove(&peer) {
                            let label = branch.label.clone();
                            viewer_teardown.enqueue(branch).await?;
                            print_viewer_status("left", &peer, &label, None, None);
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
                    Signal::ViewerJoined {
                        peer,
                        name,
                        id,
                        avatar_url,
                    } => {
                        if viewers.contains_key(&peer) {
                            eprintln!("[host] ignoring duplicate join from viewer {peer}");
                            continue;
                        }
                        let first_active_viewer = viewers.is_empty();
                        if first_active_viewer {
                            encoded_frames.store(0, Ordering::Relaxed);
                            gop_sampled_at = Instant::now();
                            recovery_keyframe.reset();
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
                                print_viewer_status("joined", &peer, &label, id, avatar_url);
                            }
                            Err(err) => eprintln!("[host] could not add viewer {peer}: {err}"),
                        }
                    }
                    Signal::ViewerLeft { peer } => {
                        if let Some(branch) = viewers.remove(&peer) {
                            let label = branch.label.clone();
                            viewer_teardown.enqueue(branch).await?;
                            println!("[host] {label} left ({} remaining)", viewers.len());
                            print_viewer_status("left", &peer, &label, None, None);
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
                            let installed = gst::Promise::with_change_func(move |reply| {
                                match check_promise_reply(reply, "installing remote answer") {
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

    let mut drain_result = Ok(());
    for (_, branch) in viewers.drain() {
        if let Err(error) = viewer_teardown.enqueue(branch).await {
            if drain_result.is_ok() {
                drain_result = Err(error);
            }
        }
    }
    let teardown_result = viewer_teardown.finish().await;
    let stop_result = pipeline
        .set_state(gst::State::Null)
        .map(|_| ())
        .map_err(anyhow::Error::from);
    client.close().await;
    let cleanup_result = combine_session_and_cleanup(drain_result, teardown_result);
    combine_session_and_cleanup(
        session_result,
        combine_session_and_cleanup(cleanup_result, stop_result),
    )
}

/// The client's process id, when the client started us.
///
/// Set by `orange-client` so whole-screen capture can leave the client's own cues
/// out of the stream. Absent when `orange host` is run straight from a shell,
/// where there is no client making noise to exclude.
fn ui_process_id() -> Option<u32> {
    std::env::var("ORANGE_UI_PID").ok()?.parse().ok()
}

/// Attach a new viewer branch to the running pipeline and start negotiating.
/// Build the audio capture chain and return its tee, so each viewer can take
/// a branch from it.
fn build_audio_tee(pipeline: &gst::Pipeline, pid: u32) -> Result<gst::Element> {
    check_audio_elements()?;

    let chain = gst::parse::bin_from_description(&build_audio_chain(pid, ui_process_id()), true)
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

/// `id` and `avatar_url` are only present on a join, and only when the viewer
/// authenticated: they are what the client needs to offer to keep this person as
/// a friend. `peer` is a routing id the relay reassigns per session, so it can
/// address a branch but can never identify anybody.
fn print_viewer_status(
    event: &str,
    peer: &str,
    label: &str,
    id: Option<String>,
    avatar_url: Option<String>,
) {
    println!(
        "[host-status] {}",
        serde_json::json!({
            "event": event,
            "peer": peer,
            "label": label,
            "id": id,
            "avatar_url": avatar_url,
        })
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_receipt_accepts_only_hosting_diagnostic_session() {
        let hosting = Signal::Hosting {
            code: "ROOM-CODE".to_string(),
            diagnostic_session: Some("host-session".to_string()),
        };
        let untrusted_stream_info = Signal::StreamInfo {
            host_name: Some("Viewer Supplied".to_string()),
            host_id: None,
            host_avatar: None,
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
    fn gop_calculations_report_measured_rate_and_bound_configured_rate() {
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
    }
}
