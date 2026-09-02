use anyhow::{Context, Result};
use gst::prelude::*;
use gstreamer as gst;
use gstreamer_video as gst_video;
use gstreamer_webrtc as gst_webrtc;
use std::io::Write;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{mpsc as std_mpsc, Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};
use tokio::sync::mpsc;

use crate::media_diagnostics::{
    diagnostics_enabled, start_webrtc_diagnostics, track_pad, DiagnosticsHandle, MediaProgress,
    MediaStage,
};
use crate::pipeline::Codec;
use crate::webrtc::{build_video_payloader, rtp_caps};
use orange_signal::Signal;

use super::{
    check_promise_reply, enable_nack, forward_ice, make_webrtcbin, watch_connection,
    ConnectionFailure, ConnectionReady,
};

const AUDIO_BRANCH_MAX_PACKETS: u32 = 10;
const STARTUP_KEYFRAME_DELAYS: [Duration; 3] = [
    Duration::from_millis(250),
    Duration::from_millis(500),
    Duration::from_millis(750),
];
static NEXT_DIAGNOSTIC_ID: AtomicU64 = AtomicU64::new(1);
static LAST_KEYFRAME_REQUEST: OnceLock<Mutex<Option<Instant>>> = OnceLock::new();

enum StartupKeyframeCommand {
    Request,
    Stop,
}

#[derive(Clone)]
struct StartupKeyframeTrigger {
    sender: std_mpsc::Sender<StartupKeyframeCommand>,
    requested: Arc<AtomicBool>,
}

impl StartupKeyframeTrigger {
    fn request(&self) {
        if !self.requested.swap(true, Ordering::AcqRel) {
            let _ = self.sender.send(StartupKeyframeCommand::Request);
        }
    }
}

struct StartupKeyframeWorker {
    sender: Option<std_mpsc::Sender<StartupKeyframeCommand>>,
    worker: Option<std::thread::JoinHandle<()>>,
}

impl StartupKeyframeWorker {
    fn spawn(
        tee: gst::glib::WeakRef<gst::Element>,
        bin: gst::glib::WeakRef<gst::Element>,
    ) -> Result<(Self, StartupKeyframeTrigger)> {
        let mut first = true;
        Self::spawn_with(move || {
            let Some(tee) = tee.upgrade() else {
                return false;
            };
            if !first {
                let Some(bin) = bin.upgrade() else {
                    return false;
                };
                if bin.property::<gst_webrtc::WebRTCPeerConnectionState>("connection-state")
                    != gst_webrtc::WebRTCPeerConnectionState::Connected
                {
                    return false;
                }
            }
            first = false;
            force_key_unit(&tee);
            true
        })
    }

    fn spawn_with(
        request: impl FnMut() -> bool + Send + 'static,
    ) -> Result<(Self, StartupKeyframeTrigger)> {
        Self::spawn_with_delays(request, STARTUP_KEYFRAME_DELAYS)
    }

    fn spawn_with_delays(
        mut request: impl FnMut() -> bool + Send + 'static,
        delays: [Duration; 3],
    ) -> Result<(Self, StartupKeyframeTrigger)> {
        let (sender, receiver) = std_mpsc::channel();
        let worker = std::thread::Builder::new()
            .name("startup-keyframes".to_string())
            .spawn(move || {
                while let Ok(command) = receiver.recv() {
                    match command {
                        StartupKeyframeCommand::Request => {
                            if !request() {
                                continue;
                            }
                            for delay in delays {
                                match receiver.recv_timeout(delay) {
                                    Ok(StartupKeyframeCommand::Stop)
                                    | Err(std_mpsc::RecvTimeoutError::Disconnected) => return,
                                    Ok(StartupKeyframeCommand::Request) => continue,
                                    Err(std_mpsc::RecvTimeoutError::Timeout) => {
                                        if !request() {
                                            break;
                                        }
                                    }
                                }
                            }
                        }
                        StartupKeyframeCommand::Stop => return,
                    }
                }
            })
            .context("failed to spawn startup keyframe worker")?;
        let trigger = StartupKeyframeTrigger {
            sender: sender.clone(),
            requested: Arc::new(AtomicBool::new(false)),
        };
        Ok((
            Self {
                sender: Some(sender),
                worker: Some(worker),
            },
            trigger,
        ))
    }

    fn shutdown(&mut self) {
        if let Some(sender) = self.sender.take() {
            let _ = sender.send(StartupKeyframeCommand::Stop);
        }
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

impl Drop for StartupKeyframeWorker {
    fn drop(&mut self) {
        self.shutdown();
    }
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

pub(super) struct ViewerBranch {
    pub(super) bin: gst::Element,
    links: Vec<TeeBranch>,
    pub(super) label: String,
    startup_keyframes: Option<StartupKeyframeWorker>,
    diagnostics: Option<DiagnosticsHandle>,
}

pub(super) struct ViewerTeardown {
    sender: Option<mpsc::Sender<ViewerBranch>>,
    pipeline: gst::Pipeline,
    worker: Option<std::thread::JoinHandle<()>>,
}

async fn run_viewer_teardown_blocking(teardown: impl FnOnce() + Send + 'static) -> Result<()> {
    tokio::task::spawn_blocking(teardown)
        .await
        .map_err(|error| anyhow::anyhow!("fallback viewer teardown task failed: {error}"))
}

impl ViewerTeardown {
    pub(super) fn new(pipeline: &gst::Pipeline) -> Result<Self> {
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
                        std::process::abort();
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

    pub(super) async fn enqueue(&self, branch: ViewerBranch) -> Result<()> {
        let sender = self
            .sender
            .as_ref()
            .expect("sender exists until viewer teardown drop");
        if let Err(error) = sender.send(branch).await {
            let pipeline = self.pipeline.clone();
            run_viewer_teardown_blocking(move || remove_viewer(&pipeline, error.0)).await?;
        }
        Ok(())
    }

    pub(super) async fn finish(mut self) -> Result<()> {
        self.sender.take();
        let worker = self.worker.take();
        tokio::task::spawn_blocking(move || {
            if let Some(worker) = worker {
                worker
                    .join()
                    .map_err(|_| anyhow::anyhow!("viewer teardown worker panicked"))?;
            }
            Ok(())
        })
        .await
        .map_err(|error| anyhow::anyhow!("viewer teardown join task failed: {error}"))?
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
    is_video: bool,
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
    let transceiver = sink_pad.property::<gst_webrtc::WebRTCRTPTransceiver>("transceiver");
    if is_video {
        enable_nack(&transceiver);
    } else if let Some(sender) = transceiver.sender() {
        sender.set_priority(gst_webrtc::WebRTCPriorityType::High);
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
pub(super) fn add_viewer(
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
        startup_keyframes: None,
        diagnostics: None,
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

    let tee_for_startup = tee.downgrade();
    let bin_for_startup = branch.bin.downgrade();
    let startup_trigger = match install_startup_keyframes(&mut branch, move || {
        StartupKeyframeWorker::spawn(tee_for_startup, bin_for_startup)
    }) {
        Ok(trigger) => trigger,
        Err(error) => {
            remove_viewer(pipeline, branch);
            return Err(error);
        }
    };

    let diagnostic_label = format!(
        "host-viewer-{}",
        NEXT_DIAGNOSTIC_ID.fetch_add(1, Ordering::Relaxed)
    );
    let failed_peer = peer.to_string();
    let connection_failures = failures.clone();
    let on_connection_failure: ConnectionFailure = Arc::new(move |error| {
        let _ = connection_failures.send((failed_peer.clone(), error));
    });
    let on_connected: ConnectionReady = Arc::new(move || {
        startup_trigger.request();
    });
    watch_connection(
        &branch.bin,
        format!("host->{peer}"),
        diagnostic_label.clone(),
        Some(on_connected),
        Some(on_connection_failure),
    );
    branch.diagnostics = start_webrtc_diagnostics(&branch.bin, diagnostic_label, progress, None);
    forward_ice(&branch.bin, out.clone(), peer.to_string());

    create_offer(&branch.bin, out, failures, peer.to_string());
    Ok(branch)
}

fn shutdown_startup_then(worker: &mut StartupKeyframeWorker, teardown: impl FnOnce()) {
    worker.shutdown();
    teardown();
}

fn install_startup_keyframes(
    branch: &mut ViewerBranch,
    spawn: impl FnOnce() -> Result<(StartupKeyframeWorker, StartupKeyframeTrigger)>,
) -> Result<StartupKeyframeTrigger> {
    let (worker, trigger) = spawn()?;
    branch.startup_keyframes = Some(worker);
    Ok(trigger)
}

fn remove_viewer(pipeline: &gst::Pipeline, mut branch: ViewerBranch) {
    if let Some(worker) = &mut branch.startup_keyframes {
        shutdown_startup_then(worker, || {
            branch.diagnostics.take();
        });
    } else {
        branch.diagnostics.take();
    }
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

pub(super) fn force_key_unit(tee: &gst::Element) {
    let event = gst_video::UpstreamForceKeyUnitEvent::builder()
        .all_headers(true)
        .build();
    request_keyframe_at(
        LAST_KEYFRAME_REQUEST.get_or_init(|| Mutex::new(None)),
        Instant::now(),
        || tee.send_event(event),
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

#[cfg(test)]
mod tests {
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
}
