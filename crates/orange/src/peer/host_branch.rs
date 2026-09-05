use anyhow::{Context, Result};
use gst::prelude::*;
use gstreamer as gst;
use gstreamer_video as gst_video;
use gstreamer_webrtc as gst_webrtc;
use std::io::Write;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{mpsc as std_mpsc, Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, oneshot};

use crate::media_diagnostics::{
    diagnostics_enabled, start_webrtc_diagnostics, track_pad, DiagnosticsHandle, MediaProgress,
    MediaStage,
};
use crate::pipeline::Codec;
use crate::webrtc::{build_video_payloader, rtp_caps};
use orange_signal::Signal;

use super::{
    check_promise_reply, enable_nack, forward_ice, make_webrtcbin, watch_connection,
    ConnectionFailureHandler, ConnectionReadyHandler,
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

enum ViewerTeardownCommand {
    Remove(ViewerBranch),
    Suspend(oneshot::Sender<Result<()>>),
}

impl ViewerTeardownCommand {
    fn run(self, pipeline: &gst::Pipeline) {
        match self {
            Self::Remove(branch) => remove_viewer(pipeline, branch),
            Self::Suspend(completed) => {
                let result = pipeline
                    .set_state(gst::State::Ready)
                    .map(|_| ())
                    .context("failed to suspend shared host media");
                let _ = completed.send(result);
            }
        }
    }
}

pub(super) struct ViewerTeardown {
    sender: Option<mpsc::Sender<ViewerTeardownCommand>>,
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
        let (sender, mut receiver) = mpsc::channel::<ViewerTeardownCommand>(1);
        let worker_pipeline = pipeline.clone();
        let worker = std::thread::Builder::new()
            .name("viewer-teardown".to_string())
            .spawn(move || {
                while let Some(command) = receiver.blocking_recv() {
                    if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        command.run(&worker_pipeline);
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
        self.enqueue_command(ViewerTeardownCommand::Remove(branch))
            .await
    }

    // The single-owner host awaits this barrier before accepting another join.
    // FIFO removal finishes first, so READY cannot race a new viewer branch or
    // leave WASAPI running after the final video branch has disappeared.
    pub(super) async fn suspend(&self) -> Result<()> {
        let (completed, completion) = oneshot::channel();
        self.enqueue_command(ViewerTeardownCommand::Suspend(completed))
            .await?;
        completion
            .await
            .context("viewer teardown stopped before suspending shared host media")?
    }

    async fn enqueue_command(&self, command: ViewerTeardownCommand) -> Result<()> {
        let sender = self
            .sender
            .as_ref()
            .expect("sender exists until viewer teardown drop");
        if let Err(error) = sender.send(command).await {
            let pipeline = self.pipeline.clone();
            run_viewer_teardown_blocking(move || error.0.run(&pipeline)).await?;
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
    let on_connection_failure: ConnectionFailureHandler = Arc::new(move |error| {
        let _ = connection_failures.send((failed_peer.clone(), error));
    });
    let on_connected: ConnectionReadyHandler = Arc::new(move || {
        startup_trigger.request();
    });
    watch_connection(
        &branch.bin,
        format!("host->{peer}"),
        diagnostic_label.clone(),
        Some(on_connected),
        Some(on_connection_failure),
        None,
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
#[path = "host_branch_tests.rs"]
mod tests;
