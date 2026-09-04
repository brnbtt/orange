//! Tests for [`super`], kept out of `host_branch.rs` so the branch
//! lifecycle and the cases that pin it down can be read separately.

use super::*;
use crate::test_support::run_in_bounded_subprocess;
use std::sync::atomic::AtomicUsize;
use std::time::Duration;

fn shutdown_startup_worker_bounded(mut worker: StartupKeyframeWorker) -> bool {
    let (finished, wait_for_finish) = std::sync::mpsc::sync_channel(1);
    let emergency_stop = worker.sender.as_ref().cloned();
    let shutdown = std::thread::spawn(move || {
        worker.shutdown();
        let _ = finished.send(());
    });
    let mut completed = wait_for_finish.recv_timeout(Duration::from_secs(1)).is_ok();
    if !completed {
        if let Some(stop) = emergency_stop {
            let _ = stop.send(StartupKeyframeCommand::Stop);
        }
        completed = wait_for_finish.recv_timeout(Duration::from_secs(1)).is_ok();
    }
    completed && shutdown.join().is_ok()
}

#[test]
fn peer_worker_review_startup_cadence_has_exact_clock_free_progression() {
    if run_in_bounded_subprocess(
            "ORANGE_TEST_STARTUP_CADENCE_CHILD",
            "peer::host_branch::tests::peer_worker_review_startup_cadence_has_exact_clock_free_progression",
        ) {
            return;
        }
    let (calls, receive_calls) = std::sync::mpsc::sync_channel(4);
    let sequence = Arc::new(AtomicUsize::new(0));
    let sequence_for_worker = sequence.clone();
    let (worker, trigger) = StartupKeyframeWorker::spawn_with_delays(
        move || {
            let call = sequence_for_worker.fetch_add(1, Ordering::SeqCst) + 1;
            calls.send(call).unwrap();
            true
        },
        [Duration::ZERO; 3],
    )
    .unwrap();
    trigger.request();

    let observed: Vec<_> = (0..4)
        .map(|_| receive_calls.recv_timeout(Duration::from_secs(1)).unwrap())
        .collect();
    let shutdown = shutdown_startup_worker_bounded(worker);

    assert!(shutdown);
    assert_eq!(observed, [1, 2, 3, 4]);
    assert_eq!(
        STARTUP_KEYFRAME_DELAYS,
        [
            Duration::from_millis(250),
            Duration::from_millis(500),
            Duration::from_millis(750),
        ]
    );
}

#[test]
fn peer_worker_review_startup_spawn_failure_leaves_rollback_join_free() {
    gst::init().unwrap();
    let pipeline = gst::Pipeline::new();
    let bin = gst::ElementFactory::make("identity")
        .name("startup-spawn-failure")
        .build()
        .unwrap();
    pipeline.add(&bin).unwrap();
    let mut branch = ViewerBranch {
        bin,
        links: Vec::new(),
        label: "spawn failure".to_string(),
        startup_keyframes: None,
        diagnostics: None,
    };

    let result = install_startup_keyframes(&mut branch, || {
        Err(anyhow::anyhow!("injected spawn failure"))
    });
    let owner_was_absent = branch.startup_keyframes.is_none();
    remove_viewer(&pipeline, branch);

    assert_eq!(result.err().unwrap().to_string(), "injected spawn failure");
    assert!(owner_was_absent);
    assert!(pipeline.by_name("startup-spawn-failure").is_none());
}

#[tokio::test(flavor = "current_thread")]
async fn peer_worker_review_dead_enqueue_fallback_does_not_block_tokio() {
    if run_in_bounded_subprocess(
        "ORANGE_TEST_DEAD_ENQUEUE_CHILD",
        "peer::host_branch::tests::peer_worker_review_dead_enqueue_fallback_does_not_block_tokio",
    ) {
        return;
    }
    gst::init().unwrap();
    let pipeline = gst::Pipeline::new();
    let bin = gst::ElementFactory::make("identity").build().unwrap();
    pipeline.add(&bin).unwrap();
    let (sender, receiver) = mpsc::channel(1);
    drop(receiver);
    let teardown = ViewerTeardown {
        sender: Some(sender),
        pipeline: pipeline.clone(),
        worker: None,
    };
    let (entered, wait_for_entry) = std::sync::mpsc::sync_channel(1);
    let (release, wait_for_release) = std::sync::mpsc::sync_channel(1);
    let (startup, trigger) = StartupKeyframeWorker::spawn_with(move || {
        let _ = entered.try_send(());
        let _ = wait_for_release.recv_timeout(Duration::from_secs(1));
        false
    })
    .unwrap();
    trigger.request();
    wait_for_entry.recv_timeout(Duration::from_secs(1)).unwrap();
    let (heartbeat_signal, wait_for_heartbeat) = std::sync::mpsc::sync_channel(1);
    let (release_order, wait_for_release_order) = std::sync::mpsc::sync_channel(1);
    let release_thread = std::thread::spawn(move || {
        let heartbeat_preceded_release = wait_for_heartbeat
            .recv_timeout(Duration::from_secs(1))
            .is_ok();
        let _ = release.send(());
        let _ = release_order.send(heartbeat_preceded_release);
    });
    let heartbeat = tokio::spawn(async move {
        let _ = heartbeat_signal.send(());
    });

    let enqueue_result = tokio::time::timeout(
        Duration::from_secs(2),
        teardown.enqueue(ViewerBranch {
            bin,
            links: Vec::new(),
            label: "dead enqueue".to_string(),
            startup_keyframes: Some(startup),
            diagnostics: None,
        }),
    )
    .await;
    let heartbeat_result = heartbeat.await;
    let heartbeat_preceded_release = wait_for_release_order
        .recv_timeout(Duration::from_secs(2))
        .unwrap_or(false);
    let release_joined = release_thread.join().is_ok();

    assert!(matches!(enqueue_result, Ok(Ok(()))));
    assert!(heartbeat_result.is_ok());
    assert!(heartbeat_preceded_release);
    assert!(release_joined);
}

#[tokio::test]
async fn peer_worker_review_dead_enqueue_fallback_surfaces_join_error() {
    let result = tokio::time::timeout(
        Duration::from_secs(1),
        run_viewer_teardown_blocking(|| panic!("injected teardown panic")),
    )
    .await
    .unwrap();

    let error = result.unwrap_err().to_string();
    assert!(error.contains("fallback viewer teardown task failed"));
}

#[test]
fn peer_worker_review_viewer_teardown_drop_joins_synchronous_fallback() {
    if run_in_bounded_subprocess(
            "ORANGE_TEST_VIEWER_DROP_CHILD",
            "peer::host_branch::tests::peer_worker_review_viewer_teardown_drop_joins_synchronous_fallback",
        ) {
            return;
        }
    gst::init().unwrap();
    let pipeline = gst::Pipeline::new();
    let (sender, receiver) = mpsc::channel(1);
    drop(receiver);
    let (worker_closing, wait_for_worker_closing) = std::sync::mpsc::sync_channel(1);
    let (release_worker, wait_for_release) = std::sync::mpsc::sync_channel(1);
    let worker = std::thread::spawn(move || {
        let _ = worker_closing.send(());
        let _ = wait_for_release.recv_timeout(Duration::from_secs(1));
    });
    let teardown = ViewerTeardown {
        sender: Some(sender),
        pipeline,
        worker: Some(worker),
    };
    let (drop_finished, wait_for_drop) = std::sync::mpsc::sync_channel(1);
    let dropper = std::thread::spawn(move || {
        drop(teardown);
        let _ = drop_finished.send(());
    });

    let closing = wait_for_worker_closing.recv_timeout(Duration::from_secs(1));
    let blocked = wait_for_drop.try_recv().is_err();
    let _ = release_worker.send(());
    let finished = wait_for_drop.recv_timeout(Duration::from_secs(2));
    let joined = dropper.join();

    assert!(closing.is_ok());
    assert!(blocked);
    assert!(finished.is_ok());
    assert!(joined.is_ok());
}

#[test]
fn startup_keyframe_worker_trigger_is_one_shot() {
    if run_in_bounded_subprocess(
        "ORANGE_TEST_STARTUP_ONESHOT_CHILD",
        "peer::host_branch::tests::startup_keyframe_worker_trigger_is_one_shot",
    ) {
        return;
    }
    let requests = Arc::new(AtomicUsize::new(0));
    let requests_for_worker = requests.clone();
    let (requested, wait_for_request) = std::sync::mpsc::sync_channel(1);
    let (worker, trigger) = StartupKeyframeWorker::spawn_with(move || {
        requests_for_worker.fetch_add(1, Ordering::SeqCst);
        let _ = requested.try_send(());
        true
    })
    .unwrap();

    trigger.request();
    trigger.request();
    let observed = wait_for_request.recv_timeout(Duration::from_secs(1));
    let shutdown = shutdown_startup_worker_bounded(worker);

    assert!(shutdown);
    assert!(observed.is_ok());
    assert_eq!(requests.load(Ordering::SeqCst), 1);
}

#[test]
fn startup_keyframe_worker_stop_interrupts_wait() {
    if run_in_bounded_subprocess(
        "ORANGE_TEST_STARTUP_STOP_CHILD",
        "peer::host_branch::tests::startup_keyframe_worker_stop_interrupts_wait",
    ) {
        return;
    }
    let (started, wait_for_start) = std::sync::mpsc::sync_channel(1);
    let (worker, trigger) = StartupKeyframeWorker::spawn_with_delays(
        move || {
            let _ = started.send(());
            true
        },
        [Duration::from_secs(60); 3],
    )
    .unwrap();
    trigger.request();
    wait_for_start.recv_timeout(Duration::from_secs(1)).unwrap();

    let completed = shutdown_startup_worker_bounded(worker);

    assert!(completed);
}

#[test]
fn startup_keyframe_worker_sends_nothing_after_stop() {
    if run_in_bounded_subprocess(
        "ORANGE_TEST_STARTUP_NO_DELAY_CHILD",
        "peer::host_branch::tests::startup_keyframe_worker_sends_nothing_after_stop",
    ) {
        return;
    }
    let requests = Arc::new(AtomicUsize::new(0));
    let requests_for_worker = requests.clone();
    let (started, wait_for_start) = std::sync::mpsc::sync_channel(1);
    let (worker, trigger) = StartupKeyframeWorker::spawn_with(move || {
        requests_for_worker.fetch_add(1, Ordering::SeqCst);
        let _ = started.try_send(());
        true
    })
    .unwrap();
    trigger.request();
    wait_for_start.recv_timeout(Duration::from_secs(1)).unwrap();

    let shutdown = shutdown_startup_worker_bounded(worker);

    assert!(shutdown);
    assert_eq!(requests.load(Ordering::SeqCst), 1);
}

#[test]
fn audio_sender_uses_high_network_priority() {
    gst::init().unwrap();
    let pipeline = gst::Pipeline::new();
    let tee = gst::ElementFactory::make("tee").build().unwrap();
    let bin = super::make_webrtcbin("audio-priority-test").unwrap();
    pipeline.add_many([&tee, &bin]).unwrap();
    let branch = super::link_tee_branch(
        &pipeline,
        &tee,
        &bin,
        AUDIO_BRANCH_MAX_PACKETS,
        false,
        None,
        Vec::new(),
    )
    .unwrap();
    let transceiver = branch
        .bin_pad
        .property::<gst_webrtc::WebRTCRTPTransceiver>("transceiver");

    assert_eq!(
        transceiver.sender().unwrap().priority(),
        gst_webrtc::WebRTCPriorityType::High
    );
    super::remove_tee_branch(&pipeline, &bin, branch);
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
    caps.static_pad("src").unwrap().unlink(&pad).unwrap();
    bin.release_request_pad(&pad);
    let sdp = sdp.unwrap().expect("offer was not generated");

    assert!(sdp.contains("a=rtpmap:96 AV1/90000"));
    assert!(sdp.contains("a=rtpmap:97 rtx/90000"));
    assert!(sdp.contains("a=fmtp:97 apt=96"));
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
            startup_keyframes: Some(StartupKeyframeWorker::spawn_with(|| false).unwrap().0),
            diagnostics: None,
        })
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(2), teardown.finish())
        .await
        .unwrap()
        .unwrap();

    assert!(pipeline.by_name("viewer-teardown-test").is_none());
}

#[test]
fn viewer_teardown_starts_only_after_startup_worker_joins() {
    if run_in_bounded_subprocess(
        "ORANGE_TEST_STARTUP_ORDER_CHILD",
        "peer::host_branch::tests::viewer_teardown_starts_only_after_startup_worker_joins",
    ) {
        return;
    }
    struct MarkStopped(Arc<AtomicBool>);
    impl Drop for MarkStopped {
        fn drop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }

    let worker_stopped = Arc::new(AtomicBool::new(false));
    let marker = MarkStopped(worker_stopped.clone());
    let (mut startup_keyframes, _) = StartupKeyframeWorker::spawn_with(move || {
        let _ = &marker;
        false
    })
    .unwrap();
    let teardown_started_after_join = Arc::new(AtomicBool::new(false));
    let teardown_observation = teardown_started_after_join.clone();
    let worker_stopped_for_teardown = worker_stopped.clone();
    let (finished, wait_for_finish) = std::sync::mpsc::sync_channel(1);
    let shutdown = std::thread::spawn(move || {
        shutdown_startup_then(&mut startup_keyframes, || {
            teardown_observation.store(
                worker_stopped_for_teardown.load(Ordering::SeqCst),
                Ordering::SeqCst,
            );
        });
        let _ = finished.send(());
    });
    let completed = wait_for_finish.recv_timeout(Duration::from_secs(1));
    let joined = if completed.is_ok() {
        shutdown.join().is_ok()
    } else {
        false
    };

    assert!(completed.is_ok());
    assert!(joined);
    assert!(teardown_started_after_join.load(Ordering::SeqCst));
}
