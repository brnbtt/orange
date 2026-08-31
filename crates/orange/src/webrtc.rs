//! WebRTC transport.
//!
//! The important architectural decision lives here: we use `webrtcbin` rather
//! than `webrtcsink`.
//!
//! `webrtcsink` is friendlier - it handles negotiation and codec selection -
//! but it owns the encoder and expects raw video. That would re-encode frames
//! we have already encoded on the GPU, discarding the whole reason this
//! project is cheap. `webrtcbin` accepts RTP-payloaded, already-encoded media,
//! so our NVENC output goes straight onto the wire.
//!
//! The price is that we do signalling ourselves. This module proves the media
//! path with both peers in one process, exchanging SDP by direct call.

mod transport;
pub(crate) use transport::{
    audio_rtp_caps, configure_receive_transport, video_rtp_caps as rtp_caps,
};

use anyhow::{Context, Result};
use gst::prelude::*;
use gstreamer as gst;
use gstreamer_sdp as gst_sdp;
use gstreamer_video::prelude::VideoOverlayExtManual;
use gstreamer_webrtc as gst_webrtc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Condvar, Mutex, TryLockError};
use std::time::{Duration, Instant};

use crate::media_diagnostics::{
    measure_operation, track_pad, MediaProgress, MediaStage, Operation,
};
use crate::pipeline::{
    build_capture_chain, check_elements, configure_encoder, CaptureSettings, Codec,
};

pub(crate) struct AudioControlWorker {
    stop: Option<mpsc::Sender<()>>,
    worker: Option<std::thread::JoinHandle<()>>,
}

impl AudioControlWorker {
    fn spawn(
        volume: gst::Element,
        overlay: &crate::overlay::SharedOverlay,
        initial_volume: f64,
        diagnostic_role: &str,
    ) -> Result<Self> {
        let overlay = Arc::downgrade(overlay);
        let (stop, wait_for_stop) = mpsc::channel();
        let worker = std::thread::Builder::new()
            .name(format!("{diagnostic_role}-audio-control"))
            .spawn(move || {
                let mut applied = initial_volume;
                loop {
                    match wait_for_stop.recv_timeout(Duration::from_millis(50)) {
                        Ok(()) | Err(mpsc::RecvTimeoutError::Disconnected) => return,
                        Err(mpsc::RecvTimeoutError::Timeout) => {}
                    }
                    let Some(overlay) = overlay.upgrade() else {
                        return;
                    };
                    let state = match overlay.try_lock() {
                        Ok(state) => state,
                        Err(TryLockError::Poisoned(poisoned)) => poisoned.into_inner(),
                        Err(TryLockError::WouldBlock) => continue,
                    };
                    let wanted = if state.muted { 0.0 } else { state.volume };
                    drop(state);
                    if (wanted - applied).abs() > f64::EPSILON {
                        volume.set_property("volume", wanted);
                        applied = wanted;
                    }
                }
            })
            .context("failed to spawn audio control worker")?;
        Ok(Self {
            stop: Some(stop),
            worker: Some(worker),
        })
    }

    fn shutdown(&mut self) {
        self.stop.take();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

impl Drop for AudioControlWorker {
    fn drop(&mut self) {
        self.shutdown();
    }
}

pub(crate) struct IncomingBitrateWorker {
    pad: gst::glib::WeakRef<gst::Pad>,
    probe: Option<gst::PadProbeId>,
    stop: Option<mpsc::Sender<()>>,
    worker: Option<std::thread::JoinHandle<()>>,
}

impl IncomingBitrateWorker {
    fn spawn_with_probe(
        pad: &gst::Pad,
        probe: gst::PadProbeId,
        run: impl FnOnce(mpsc::Receiver<()>) + Send + 'static,
    ) -> Result<Self> {
        let (stop, wait_for_stop) = mpsc::channel();
        let worker = match std::thread::Builder::new()
            .name("incoming-bitrate".to_string())
            .spawn(move || run(wait_for_stop))
        {
            Ok(worker) => worker,
            Err(error) => {
                pad.remove_probe(probe);
                return Err(error).context("failed to spawn incoming bitrate worker");
            }
        };
        Ok(Self {
            pad: pad.downgrade(),
            probe: Some(probe),
            stop: Some(stop),
            worker: Some(worker),
        })
    }

    fn shutdown(&mut self) {
        self.stop.take();
        if let (Some(pad), Some(probe)) = (self.pad.upgrade(), self.probe.take()) {
            pad.remove_probe(probe);
        }
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

impl Drop for IncomingBitrateWorker {
    fn drop(&mut self) {
        self.shutdown();
    }
}

pub(crate) fn watch_incoming_bitrate(
    pad: &gst::Pad,
    overlay: crate::overlay::SharedOverlay,
) -> Result<IncomingBitrateWorker> {
    let bytes = Arc::new(AtomicU64::new(0));
    watch_incoming_bitrate_with_counter(pad, overlay, bytes)
}

fn watch_incoming_bitrate_with_counter(
    pad: &gst::Pad,
    overlay: crate::overlay::SharedOverlay,
    bytes: Arc<AtomicU64>,
) -> Result<IncomingBitrateWorker> {
    let bytes_for_probe = bytes.clone();
    let probe = pad
        .add_probe(gst::PadProbeType::BUFFER, move |_, info| {
            if let Some(gst::PadProbeData::Buffer(buffer)) = &info.data {
                bytes_for_probe.fetch_add(buffer.size() as u64, Ordering::Relaxed);
            }
            gst::PadProbeReturn::Ok
        })
        .context("could not install incoming bitrate probe")?;
    let overlay = Arc::downgrade(&overlay);
    IncomingBitrateWorker::spawn_with_probe(pad, probe, move |stop| {
        let mut sampled_at = Instant::now();
        loop {
            match stop.recv_timeout(Duration::from_secs(1)) {
                Ok(()) | Err(mpsc::RecvTimeoutError::Disconnected) => return,
                Err(mpsc::RecvTimeoutError::Timeout) => {}
            }
            let elapsed = sampled_at.elapsed().as_secs_f64();
            sampled_at = Instant::now();
            let received = bytes.swap(0, Ordering::Relaxed);
            let Some(overlay) = overlay.upgrade() else {
                return;
            };
            if received == 0 || elapsed == 0.0 {
                continue;
            }
            let kbps = ((received as f64 * 8.0) / elapsed / 1000.0).round() as u32;
            let mut state = match overlay.try_lock() {
                Ok(state) => state,
                Err(TryLockError::Poisoned(poisoned)) => poisoned.into_inner(),
                Err(TryLockError::WouldBlock) => continue,
            };
            state.bitrate_kbps = Some(kbps);
        }
    })
}

#[derive(Clone, Copy)]
enum ReceiveKind {
    Audio,
    Video,
}

struct ReceiveWorkerState {
    accepting: bool,
    active_callbacks: usize,
    audio_claimed: bool,
    video_claimed: bool,
    audio: Option<AudioControlWorker>,
    bitrate: Option<IncomingBitrateWorker>,
}

#[derive(Clone)]
pub(crate) struct ReceiveWorkerRegistry {
    shared: Arc<(Mutex<ReceiveWorkerState>, Condvar)>,
}

pub(crate) enum AcceptedReceivePad {
    Audio(ReceivePadClaim),
    Video(ReceivePadClaim),
}

pub(crate) fn accept_receive_pad(
    registry: &ReceiveWorkerRegistry,
    pad: &gst::Pad,
) -> Option<AcceptedReceivePad> {
    match encoding_name(pad).as_deref() {
        Some("OPUS") => registry.claim_audio().map(AcceptedReceivePad::Audio),
        Some("AV1" | "H264" | "H265") => registry.claim_video().map(AcceptedReceivePad::Video),
        _ => None,
    }
}

impl ReceiveWorkerRegistry {
    pub(crate) fn new() -> Self {
        Self {
            shared: Arc::new((
                Mutex::new(ReceiveWorkerState {
                    accepting: true,
                    active_callbacks: 0,
                    audio_claimed: false,
                    video_claimed: false,
                    audio: None,
                    bitrate: None,
                }),
                Condvar::new(),
            )),
        }
    }

    fn claim(&self, kind: ReceiveKind) -> Option<ReceivePadClaim> {
        let mut state = self
            .shared
            .0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let claimed = match kind {
            ReceiveKind::Audio => state.audio_claimed,
            ReceiveKind::Video => state.video_claimed,
        };
        if !state.accepting || claimed {
            return None;
        }
        match kind {
            ReceiveKind::Audio => state.audio_claimed = true,
            ReceiveKind::Video => state.video_claimed = true,
        }
        state.active_callbacks += 1;
        Some(ReceivePadClaim {
            shared: self.shared.clone(),
            kind,
            completed: false,
        })
    }

    pub(crate) fn claim_audio(&self) -> Option<ReceivePadClaim> {
        self.claim(ReceiveKind::Audio)
    }

    pub(crate) fn claim_video(&self) -> Option<ReceivePadClaim> {
        self.claim(ReceiveKind::Video)
    }

    pub(crate) fn close_and_take(&self) -> ReceiveWorkers {
        let (lock, callbacks_finished) = &*self.shared;
        let mut state = lock.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        state.accepting = false;
        while state.active_callbacks != 0 {
            state = callbacks_finished
                .wait(state)
                .unwrap_or_else(|poisoned| poisoned.into_inner());
        }
        ReceiveWorkers {
            audio: state.audio.take(),
            bitrate: state.bitrate.take(),
        }
    }
}

pub(crate) struct ReceivePadClaim {
    shared: Arc<(Mutex<ReceiveWorkerState>, Condvar)>,
    kind: ReceiveKind,
    completed: bool,
}

impl ReceivePadClaim {
    pub(crate) fn complete_audio(mut self, worker: Option<AudioControlWorker>) {
        debug_assert!(matches!(self.kind, ReceiveKind::Audio));
        let mut state = self
            .shared
            .0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state.audio = worker;
        self.completed = true;
    }

    pub(crate) fn complete_video(mut self, worker: Option<IncomingBitrateWorker>) {
        debug_assert!(matches!(self.kind, ReceiveKind::Video));
        let mut state = self
            .shared
            .0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state.bitrate = worker;
        self.completed = true;
    }
}

impl Drop for ReceivePadClaim {
    fn drop(&mut self) {
        let (lock, callbacks_finished) = &*self.shared;
        let mut state = lock.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        if !self.completed {
            match self.kind {
                ReceiveKind::Audio => state.audio_claimed = false,
                ReceiveKind::Video => state.video_claimed = false,
            }
        }
        state.active_callbacks -= 1;
        callbacks_finished.notify_all();
    }
}

pub(crate) struct ReceiveWorkers {
    audio: Option<AudioControlWorker>,
    bitrate: Option<IncomingBitrateWorker>,
}

impl ReceiveWorkers {
    pub(crate) fn shutdown(mut self) {
        if let Some(worker) = &mut self.bitrate {
            worker.shutdown();
        }
        if let Some(worker) = &mut self.audio {
            worker.shutdown();
        }
    }
}

pub fn build_video_payloader(codec: Codec) -> Result<gst::Element> {
    let factory = codec.payloader();
    match codec {
        Codec::Av1 => gst::ElementFactory::make(factory).build(),
        Codec::H264 => gst::ElementFactory::make(factory)
            .property("config-interval", -1i32)
            .property_from_str("aggregate-mode", "zero-latency")
            .build(),
        Codec::H265 => gst::ElementFactory::make(factory)
            .property("config-interval", -1i32)
            .build(),
    }
    .with_context(|| format!("{factory} missing"))
}

struct Peers {
    sender: gst::Element,
    receiver: gst::Element,
}

/// Wire the two `webrtcbin` elements together: offer/answer plus ICE.
///
/// Normally these messages would cross a network via a signalling server. Here
/// they are function calls, which isolates the media path from any networking
/// concerns while we verify it.
fn connect_signalling(peers: Arc<Mutex<Peers>>) {
    let (sender, receiver) = {
        let p = peers.lock().unwrap();
        (p.sender.clone(), p.receiver.clone())
    };

    // Trickle ICE, in both directions.
    let rx = receiver.clone();
    sender.connect("on-ice-candidate", false, move |values| {
        let mlineindex = values[1].get::<u32>().unwrap();
        let candidate = values[2].get::<String>().unwrap();
        rx.emit_by_name::<()>("add-ice-candidate", &[&mlineindex, &candidate]);
        None
    });

    let tx = sender.clone();
    receiver.connect("on-ice-candidate", false, move |values| {
        let mlineindex = values[1].get::<u32>().unwrap();
        let candidate = values[2].get::<String>().unwrap();
        tx.emit_by_name::<()>("add-ice-candidate", &[&mlineindex, &candidate]);
        None
    });

    // The sender drives negotiation as soon as its sink pad is linked.
    let peers_for_neg = peers.clone();
    sender.connect("on-negotiation-needed", false, move |_| {
        let peers = peers_for_neg.clone();
        let (sender, receiver) = {
            let p = peers.lock().unwrap();
            (p.sender.clone(), p.receiver.clone())
        };
        // The closure below takes ownership, so keep a handle for the emit.
        let sender_for_offer = sender.clone();

        let promise = gst::Promise::with_change_func(move |reply| {
            let Ok(Some(reply)) = reply else {
                eprintln!("[webrtc] offer failed");
                return;
            };
            let offer = reply
                .value("offer")
                .unwrap()
                .get::<gst_webrtc::WebRTCSessionDescription>()
                .unwrap();

            sender.emit_by_name::<()>("set-local-description", &[&offer, &None::<gst::Promise>]);
            receiver.emit_by_name::<()>("set-remote-description", &[&offer, &None::<gst::Promise>]);

            // Answer back the other way.
            let sender2 = sender.clone();
            let receiver2 = receiver.clone();
            let answer_promise = gst::Promise::with_change_func(move |reply| {
                let Ok(Some(reply)) = reply else {
                    eprintln!("[webrtc] answer failed");
                    return;
                };
                let answer = reply
                    .value("answer")
                    .unwrap()
                    .get::<gst_webrtc::WebRTCSessionDescription>()
                    .unwrap();
                receiver2
                    .emit_by_name::<()>("set-local-description", &[&answer, &None::<gst::Promise>]);
                sender2.emit_by_name::<()>(
                    "set-remote-description",
                    &[&answer, &None::<gst::Promise>],
                );
                println!("[webrtc] negotiation complete");
            });
            receiver
                .emit_by_name::<()>("create-answer", &[&None::<gst::Structure>, &answer_promise]);
        });

        sender_for_offer.emit_by_name::<()>("create-offer", &[&None::<gst::Structure>, &promise]);
        None
    });

    // Silence the unused warning on the SDP import while keeping it available
    // for the real signalling module that replaces this.
    let _ = gst_sdp::SDPMessage::new();
}

/// Where the received video should end up.
pub enum Output {
    /// Render into a window we own, by HWND, with controls composited on top.
    /// `d3d11videosink` implements `GstVideoOverlay`, so it draws into our
    /// borderless frame instead of creating a bare window of its own.
    Window(crate::window::PlaybackWindow),
    /// Write to a file, so the result can be verified without a display.
    File(String),
}

pub(crate) enum ReceiveOutput {
    Window(crate::window::PlaybackWindowHandle),
    File(String),
}

/// Capture a window, send it over WebRTC, receive it back, and output it.
///
/// Both peers live in this process. If this works, the encode -> payload ->
/// transport -> depayload -> decode path is sound and only signalling stands
/// between us and streaming to another machine.
pub fn run_loopback(settings: &CaptureSettings, output: Output, seconds: u64) -> Result<()> {
    // Keep the unique owner outside every callback and declare it before the
    // pipeline so the sink reaches Null before owner-driven HWND destruction.
    let (playback_owner, output) = match output {
        Output::Window(owner) => {
            let handle = owner.handle();
            (Some(owner), ReceiveOutput::Window(handle))
        }
        Output::File(path) => (None, ReceiveOutput::File(path)),
    };
    let playback = playback_owner.as_ref().map(|owner| owner.handle());
    check_elements(settings.codec)?;

    let pipeline = gst::Pipeline::new();

    // --- sending half -------------------------------------------------------
    let capture = gst::parse::bin_from_description(&build_capture_chain(settings), true)
        .context("failed to build capture chain")?;
    let encoder = capture
        .by_name("stream-encoder")
        .context("capture chain has no named encoder")?;
    configure_encoder(&encoder, settings.codec, settings.fps);
    let pay = build_video_payloader(settings.codec)?;
    let caps_filter = gst::ElementFactory::make("capsfilter")
        .property("caps", rtp_caps(settings.codec, settings.fps))
        .build()?;
    let send_bin = gst::ElementFactory::make("webrtcbin")
        .name("sender")
        .property_from_str("bundle-policy", "max-bundle")
        .build()
        .context("webrtcbin missing")?;

    // --- receiving half -----------------------------------------------------
    let recv_bin = gst::ElementFactory::make("webrtcbin")
        .name("receiver")
        .property_from_str("bundle-policy", "max-bundle")
        .build()?;
    configure_receive_transport(&recv_bin, matches!(&output, ReceiveOutput::Window(_)))?;

    pipeline.add_many([
        capture.upcast_ref(),
        &pay,
        &caps_filter,
        &send_bin,
        &recv_bin,
    ])?;
    gst::Element::link_many([capture.upcast_ref(), &pay, &caps_filter])?;

    // webrtcbin takes media on request pads.
    let src_pad = caps_filter.static_pad("src").unwrap();
    let sink_pad = send_bin
        .request_pad_simple("sink_%u")
        .context("webrtcbin refused a sink pad")?;
    src_pad.link(&sink_pad)?;

    // The receiver's pad appears only once media starts flowing.
    let pipeline_weak = pipeline.downgrade();
    let output = Arc::new(Mutex::new(Some(output)));
    let overlay = playback.as_ref().map(|playback| playback.overlay().clone());
    let workers = ReceiveWorkerRegistry::new();
    let workers_for_pad = workers.clone();
    let overlay_for_pad = overlay.clone();
    let pad_added = recv_bin.connect_pad_added(move |_, pad| {
        let Some(pipeline) = pipeline_weak.upgrade() else {
            return;
        };
        match accept_receive_pad(&workers_for_pad, pad) {
            Some(AcceptedReceivePad::Audio(claim)) => {
                match build_audio_branch(&pipeline, pad, overlay_for_pad.clone(), "loopback") {
                    Ok(worker) => claim.complete_audio(worker),
                    Err(error) => eprintln!("[webrtc] could not build audio branch: {error}"),
                }
            }
            Some(AcceptedReceivePad::Video(claim)) => {
                let Some(output) = output
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .take()
                else {
                    return;
                };
                match build_receive_branch(&pipeline, pad, output, None, "loopback") {
                    Ok(()) => {
                        let bitrate =
                            overlay_for_pad.clone().and_then(
                                |overlay| match watch_incoming_bitrate(pad, overlay) {
                                    Ok(worker) => Some(worker),
                                    Err(error) => {
                                        eprintln!(
                                            "[webrtc] incoming bitrate telemetry disabled: {error}"
                                        );
                                        None
                                    }
                                },
                            );
                        claim.complete_video(bitrate);
                    }
                    Err(error) => eprintln!("[webrtc] could not build receive branch: {error}"),
                }
            }
            None => eprintln!("[webrtc] ignoring duplicate or unexpected stream"),
        }
    });

    connect_signalling(Arc::new(Mutex::new(Peers {
        sender: send_bin,
        receiver: recv_bin.clone(),
    })));

    crate::run_pipeline_while_with_shutdown(&pipeline, seconds, playback.as_ref(), move || {
        recv_bin.disconnect(pad_added);
        workers.close_and_take().shutdown();
    })
}

/// Which codec a newly-arrived pad carries, so the right branch is built.
pub fn encoding_name(pad: &gst::Pad) -> Option<String> {
    let caps = pad.current_caps().or_else(|| pad.allowed_caps())?;
    let structure = caps.structure(0)?;
    structure.get::<String>("encoding-name").ok()
}

/// Add and link a dynamic receive branch as one transaction. GStreamer does
/// not roll back partially added elements or pad links when a later operation
/// fails, so the caller must do it explicitly before keeping the session alive.
struct ReceiveElement {
    element: gst::Element,
    logical_name: &'static str,
    factory: &'static str,
}

fn build_receive_element(
    diagnostic_role: &str,
    logical_name: &'static str,
    factory: &'static str,
    build: impl FnOnce() -> Result<gst::Element>,
) -> Result<ReceiveElement> {
    let element = measure_operation(
        diagnostic_role,
        Operation::element("element-create", logical_name, factory),
        build,
    )?;
    Ok(ReceiveElement {
        element,
        logical_name,
        factory,
    })
}

fn attach_receive_elements(
    pipeline: &gst::Pipeline,
    pad: &gst::Pad,
    elements: &[ReceiveElement],
    diagnostic_role: &str,
    link_operation: &'static str,
    pad_link_operation: &'static str,
    remove_probe_operation: &'static str,
) -> Result<()> {
    let mut added = 0;
    let block_probe = pad.add_probe(gst::PadProbeType::BLOCK_DOWNSTREAM, |_, _| {
        gst::PadProbeReturn::Ok
    });
    let result = (|| -> Result<()> {
        for element in elements {
            measure_operation(
                diagnostic_role,
                Operation::element("pipeline-add", element.logical_name, element.factory),
                || pipeline.add(&element.element),
            )?;
            added += 1;
        }
        measure_operation(diagnostic_role, Operation::named(link_operation), || {
            gst::Element::link_many(elements.iter().map(|element| &element.element))
        })?;
        measure_operation(
            diagnostic_role,
            Operation::named(pad_link_operation),
            || pad.link(&elements[0].element.static_pad("sink").unwrap()),
        )?;
        for element in elements {
            measure_operation(
                diagnostic_role,
                Operation::element(
                    "sync-state-with-parent",
                    element.logical_name,
                    element.factory,
                ),
                || element.element.sync_state_with_parent(),
            )?;
        }
        Ok(())
    })();
    if let Err(error) = result {
        if let Some(sink_pad) = elements
            .first()
            .and_then(|element| element.element.static_pad("sink"))
        {
            let _ = pad.unlink(&sink_pad);
        }
        for element in elements.iter().take(added).rev() {
            let _ = element.element.set_state(gst::State::Null);
            let _ = pipeline.remove(&element.element);
        }
        if let Some(block_probe) = block_probe {
            measure_operation(
                diagnostic_role,
                Operation::named(remove_probe_operation),
                || {
                    pad.remove_probe(block_probe);
                },
            );
        }
        return Err(error);
    }
    if let Some(block_probe) = block_probe {
        measure_operation(
            diagnostic_role,
            Operation::named(remove_probe_operation),
            || {
                pad.remove_probe(block_probe);
            },
        );
    }
    Ok(())
}

fn frame_rate_from_rtp_caps(caps: &gst::CapsRef) -> Option<u32> {
    caps.structure(0)?
        .get::<String>("a-framerate")
        .ok()?
        .parse::<u32>()
        .ok()
        .filter(|rate| (1..=480).contains(rate))
}

fn av1_decoder_factory(selection: Option<&str>, file_output: bool) -> &'static str {
    match (selection, file_output) {
        (_, true) => "d3d11av1dec",
        (Some("software"), false) => "dav1ddec",
        _ => "d3d11av1dec",
    }
}

fn build_av1_decoder(
    selection: Option<&str>,
    file_output: bool,
    diagnostic_role: &str,
) -> Result<ReceiveElement> {
    let factory = av1_decoder_factory(selection, file_output);
    build_receive_element(diagnostic_role, "video-decoder", factory, || {
        gst::ElementFactory::make(factory)
            .property("automatic-request-sync-points", true)
            .property("discard-corrupted-frames", true)
            .build()
            .with_context(|| format!("{factory} is unavailable"))
    })
}

fn build_video_decoder(
    codec: Codec,
    selection: Option<&str>,
    file_output: bool,
    diagnostic_role: &str,
) -> Result<ReceiveElement> {
    if codec == Codec::Av1 {
        return build_av1_decoder(selection, file_output, diagnostic_role);
    }
    let factory = codec.decoder();
    build_receive_element(diagnostic_role, "video-decoder", factory, || {
        gst::ElementFactory::make(factory)
            .property("automatic-request-sync-points", true)
            .property("discard-corrupted-frames", true)
            .build()
            .with_context(|| format!("{factory} is unavailable"))
    })
}

fn build_video_depayloader(codec: Codec, diagnostic_role: &str) -> Result<ReceiveElement> {
    let factory = codec.depayloader();
    build_receive_element(diagnostic_role, "video-depayloader", factory, || {
        gst::ElementFactory::make(factory)
            .property("request-keyframe", true)
            .property("wait-for-keyframe", true)
            .build()
            .with_context(|| format!("{factory} is unavailable"))
    })
}

#[cfg(test)]
fn build_av1_depayloader(diagnostic_role: &str) -> Result<ReceiveElement> {
    build_video_depayloader(Codec::Av1, diagnostic_role)
}

fn build_live_video_queue(diagnostic_role: &str) -> Result<ReceiveElement> {
    build_receive_element(diagnostic_role, "video-presentation-queue", "queue", || {
        gst::ElementFactory::make("queue")
            .property("max-size-buffers", 1u32)
            .property("max-size-bytes", 0u32)
            .property("max-size-time", 0u64)
            .property_from_str("leaky", "downstream")
            .build()
            .context("video presentation queue is unavailable")
    })
}

fn build_video_sink(diagnostic_role: &str) -> Result<ReceiveElement> {
    build_receive_element(diagnostic_role, "video-sink", "d3d11videosink", || {
        gst::ElementFactory::make("d3d11videosink")
            .property("async", false)
            .property("sync", false)
            .property("force-aspect-ratio", true)
            .build()
            .context("d3d11videosink is unavailable")
    })
}

fn build_audio_sink(diagnostic_role: &str) -> Result<ReceiveElement> {
    let primary = measure_operation(
        diagnostic_role,
        Operation::element("element-create", "audio-sink", "wasapi2sink"),
        || {
            gst::ElementFactory::make("wasapi2sink")
                .property("async", false)
                .property("buffer-time", 40_000i64)
                .property("latency-time", 10_000i64)
                .build()
        },
    );
    match primary {
        Ok(element) => Ok(ReceiveElement {
            element,
            logical_name: "audio-sink",
            factory: "wasapi2sink",
        }),
        Err(_) => build_receive_element(diagnostic_role, "audio-sink", "wasapisink", || {
            gst::ElementFactory::make("wasapisink")
                .property("async", false)
                .property("buffer-time", 40_000i64)
                .property("latency-time", 10_000i64)
                .build()
                .context("audio sink is unavailable")
        }),
    }
}

fn file_output_factories(codec: Codec) -> (&'static str, &'static str) {
    (codec.encoder(), codec.parser())
}

fn build_audio_decoder(diagnostic_role: &str) -> Result<ReceiveElement> {
    build_receive_element(diagnostic_role, "audio-decoder", "opusdec", || {
        Ok(gst::ElementFactory::make("opusdec")
            .property("plc", true)
            .build()?)
    })
}

/// Attach depayload -> parse -> hardware decode -> output to the receiver.
pub fn build_receive_branch(
    pipeline: &gst::Pipeline,
    pad: &gst::Pad,
    output: ReceiveOutput,
    progress: Option<Arc<MediaProgress>>,
    diagnostic_role: &str,
) -> Result<()> {
    let reveal_playback = match &output {
        ReceiveOutput::Window(playback) => Some(playback.clone()),
        ReceiveOutput::File(_) => None,
    };
    let encoding = encoding_name(pad).context("video RTP pad has no encoding name")?;
    let codec = Codec::from_rtp_encoding(&encoding).context("unsupported video RTP encoding")?;
    let depay = build_video_depayloader(codec, diagnostic_role)?;
    let parser = codec.parser();
    let parse = build_receive_element(diagnostic_role, "video-parser", parser, || {
        gst::ElementFactory::make(parser)
            .build()
            .with_context(|| format!("{parser} is unavailable"))
    })?;
    let decoder_selection = std::env::var("ORANGE_AV1_DECODER").ok();
    let dec = build_video_decoder(
        codec,
        decoder_selection.as_deref(),
        matches!(&output, ReceiveOutput::File(_)),
        diagnostic_role,
    )?;
    let advertised_rate = pad
        .current_caps()
        .as_ref()
        .and_then(|caps| frame_rate_from_rtp_caps(caps));
    if let Some(progress) = progress {
        track_pad(
            &depay
                .element
                .static_pad("src")
                .context("depayloader has no src pad")?,
            MediaStage::Depay,
            progress.clone(),
        );
        track_pad(
            &parse
                .element
                .static_pad("src")
                .context("parser has no src pad")?,
            MediaStage::Parsed,
            progress.clone(),
        );
        track_pad(
            &dec.element
                .static_pad("src")
                .context("decoder has no src pad")?,
            MediaStage::Decoded,
            progress,
        );
    }

    let tail: Vec<ReceiveElement> = match output {
        ReceiveOutput::Window(playback) => {
            if let Some(rate) = advertised_rate {
                if let Ok(mut state) = playback.overlay().lock() {
                    state.fps = Some(rate as f64);
                }
            }
            // Controls are composited into the frame here, on the GPU, rather
            // than drawn by a second window that would have to chase this one.
            let composition = build_receive_element(
                diagnostic_role,
                "video-overlay",
                "overlaycomposition",
                || {
                    gst::ElementFactory::make("overlaycomposition")
                        .build()
                        .context("overlaycomposition missing")
                },
            )?;
            crate::overlay::attach(&composition.element, &playback)?;
            let queue = build_live_video_queue(diagnostic_role)?;

            let sink = build_video_sink(diagnostic_role)?;
            let overlay_iface = sink
                .element
                .dynamic_cast_ref::<gstreamer_video::VideoOverlay>()
                .context("d3d11videosink does not implement GstVideoOverlay")?;
            let hwnd = playback.hwnd().context("playback window is unavailable")?;
            // SAFETY: Window ReceiveOutput values are created only by
            // run_loopback/run_watch, where the unique owner is declared
            // before and outlives the receiver pipeline and its callbacks.
            unsafe { overlay_iface.set_window_handle(hwnd as usize) };

            vec![queue, composition, sink]
        }
        ReceiveOutput::File(path) => {
            // Re-encode only because writing raw frames to disk is impractical.
            // This branch exists for verification, not for the real product.
            let (encoder, parser) = file_output_factories(codec);
            let enc =
                build_receive_element(diagnostic_role, "video-file-encoder", encoder, || {
                    Ok(gst::ElementFactory::make(encoder)
                        .property("bitrate", 25_000u32)
                        .build()?)
                })?;
            let parse2 =
                build_receive_element(diagnostic_role, "video-file-parser", parser, || {
                    Ok(gst::ElementFactory::make(parser).build()?)
                })?;
            let mux =
                build_receive_element(diagnostic_role, "video-file-muxer", "matroskamux", || {
                    Ok(gst::ElementFactory::make("matroskamux").build()?)
                })?;
            let sink =
                build_receive_element(diagnostic_role, "video-file-sink", "filesink", || {
                    Ok(gst::ElementFactory::make("filesink")
                        .property("location", path)
                        .build()?)
                })?;
            vec![enc, parse2, mux, sink]
        }
    };

    let mut all = vec![depay, parse, dec];
    all.extend(tail);

    attach_receive_elements(
        pipeline,
        pad,
        &all,
        diagnostic_role,
        "link-video-receive-elements",
        "link-incoming-video-rtp-pad",
        "remove-incoming-video-block-probe",
    )?;
    if let Some(playback) = reveal_playback {
        playback.reveal();
    }
    println!("[webrtc] receiving video");
    Ok(())
}

/// Attach the audio branch: depayload -> decode -> volume -> speakers.
///
/// The overlay's volume and mute state is applied here. A short poll is used
/// rather than a callback because the state is owned by the window thread and
/// changes only on user input; at 20 Hz the cost is unmeasurable.
pub(crate) fn build_audio_branch(
    pipeline: &gst::Pipeline,
    pad: &gst::Pad,
    overlay: Option<crate::overlay::SharedOverlay>,
    diagnostic_role: &str,
) -> Result<Option<AudioControlWorker>> {
    let initial_volume = overlay
        .as_ref()
        .and_then(|overlay| overlay.lock().ok())
        .map(|state| if state.muted { 0.0 } else { state.volume })
        .unwrap_or(0.3);
    let depay =
        build_receive_element(diagnostic_role, "audio-depayloader", "rtpopusdepay", || {
            Ok(gst::ElementFactory::make("rtpopusdepay").build()?)
        })?;
    let dec = build_audio_decoder(diagnostic_role)?;
    let convert =
        build_receive_element(diagnostic_role, "audio-converter", "audioconvert", || {
            Ok(gst::ElementFactory::make("audioconvert").build()?)
        })?;
    let resample =
        build_receive_element(diagnostic_role, "audio-resampler", "audioresample", || {
            Ok(gst::ElementFactory::make("audioresample").build()?)
        })?;
    let volume = build_receive_element(diagnostic_role, "audio-volume", "volume", || {
        Ok(gst::ElementFactory::make("volume")
            .name("viewer-volume")
            .property("volume", initial_volume)
            .build()?)
    })?;
    let sink = build_audio_sink(diagnostic_role)?;

    let all = [depay, dec, convert, resample, volume, sink];
    attach_receive_elements(
        pipeline,
        pad,
        &all,
        diagnostic_role,
        "link-audio-receive-elements",
        "link-incoming-audio-rtp-pad",
        "remove-incoming-audio-block-probe",
    )?;

    let worker = overlay.as_ref().and_then(|overlay| {
        match AudioControlWorker::spawn(
            all[4].element.clone(),
            overlay,
            initial_volume,
            diagnostic_role,
        ) {
            Ok(worker) => Some(worker),
            Err(error) => {
                eprintln!("[{diagnostic_role}] audio controls disabled: {error}");
                None
            }
        }
    });
    println!("[webrtc] receiving audio");
    Ok(worker)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::Ordering;
    use std::time::{Duration, Instant};

    fn run_bounded(run: impl FnOnce() + Send + 'static) -> bool {
        let (finished, wait_for_finish) = std::sync::mpsc::sync_channel(1);
        let worker = std::thread::spawn(move || {
            run();
            let _ = finished.send(());
        });
        let completed = wait_for_finish.recv_timeout(Duration::from_secs(1));
        completed.is_ok() && worker.join().is_ok()
    }

    fn test_overlay() -> crate::overlay::SharedOverlay {
        Arc::new(Mutex::new(crate::overlay::OverlayState::new(
            crate::window::PlaybackProfile::FriendViewer { cascade: 0 },
        )))
    }

    fn rtp_test_pads(encoding: &str) -> (gst::Pad, gst::Pad) {
        let caps = gst::Caps::builder("application/x-rtp")
            .field("encoding-name", encoding)
            .build();
        let src = gst::Pad::builder(gst::PadDirection::Src).build();
        let sink = gst::Pad::builder(gst::PadDirection::Sink)
            .event_function(|_, _, _| true)
            .build();
        src.link(&sink).unwrap();
        src.set_active(true).unwrap();
        sink.set_active(true).unwrap();
        assert!(src.push_event(gst::event::Caps::new(&caps)));
        (src, sink)
    }

    fn wait_for_counter(counter: &AtomicU64, expected: u64, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        while counter.load(Ordering::SeqCst) != expected && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(1));
        }
        counter.load(Ordering::SeqCst) == expected
    }

    #[test]
    fn audio_control_receive_worker_cancels_while_overlay_is_locked() {
        gst::init().unwrap();
        let overlay = test_overlay();
        let guard = overlay.lock().unwrap();
        let volume = gst::ElementFactory::make("volume").build().unwrap();
        let mut worker = AudioControlWorker::spawn(volume, &overlay, 0.3, "test").unwrap();
        std::thread::sleep(Duration::from_millis(75));
        let (finished, wait_for_finish) = std::sync::mpsc::sync_channel(1);
        let shutdown = std::thread::spawn(move || {
            let stopped_at = Instant::now();
            worker.shutdown();
            let _ = finished.send(stopped_at.elapsed());
        });
        let prompt = wait_for_finish.recv_timeout(Duration::from_millis(100));
        let prompt_completion = prompt.is_ok();
        drop(guard);
        let elapsed = prompt
            .or_else(|_| wait_for_finish.recv_timeout(Duration::from_secs(1)))
            .ok();
        let joined = if elapsed.is_some() {
            shutdown.join().is_ok()
        } else {
            false
        };

        assert!(prompt_completion);
        assert!(elapsed.is_some_and(|elapsed| elapsed < Duration::from_millis(100)));
        assert!(joined);
    }

    #[test]
    fn incoming_bitrate_receive_worker_cancels_while_overlay_is_locked() {
        gst::init().unwrap();
        let overlay = test_overlay();
        let guard = overlay.lock().unwrap();
        let pad = gst::Pad::builder(gst::PadDirection::Src).build();
        let mut worker = watch_incoming_bitrate(&pad, overlay.clone()).unwrap();
        let (finished, wait_for_finish) = std::sync::mpsc::sync_channel(1);
        let shutdown = std::thread::spawn(move || {
            let stopped_at = Instant::now();
            worker.shutdown();
            let _ = finished.send(stopped_at.elapsed());
        });
        let prompt = wait_for_finish.recv_timeout(Duration::from_millis(100));
        let prompt_completion = prompt.is_ok();
        drop(guard);
        let elapsed = prompt
            .or_else(|_| wait_for_finish.recv_timeout(Duration::from_secs(1)))
            .ok();
        let joined = if elapsed.is_some() {
            shutdown.join().is_ok()
        } else {
            false
        };

        assert!(prompt_completion);
        assert!(elapsed.is_some_and(|elapsed| elapsed < Duration::from_millis(100)));
        assert!(joined);
    }

    #[test]
    fn incoming_bitrate_receive_worker_removes_probe_on_shutdown() {
        gst::init().unwrap();
        let pipeline = gst::parse::launch(
            "appsrc name=bitrate-source is-live=true format=time ! fakesink sync=false",
        )
        .unwrap()
        .downcast::<gst::Pipeline>()
        .unwrap();
        let source = pipeline.by_name("bitrate-source").unwrap();
        let pad = source.static_pad("src").unwrap();
        let bytes = Arc::new(AtomicU64::new(0));
        let mut worker =
            watch_incoming_bitrate_with_counter(&pad, test_overlay(), bytes.clone()).unwrap();
        pipeline.set_state(gst::State::Playing).unwrap();
        let first_push = source.emit_by_name::<gst::FlowReturn>(
            "push-buffer",
            &[&gst::Buffer::with_size(17).unwrap()],
        );
        let first_observed = wait_for_counter(&bytes, 17, Duration::from_secs(1));
        let shutdown_completed = run_bounded(move || worker.shutdown());
        bytes.store(0, Ordering::SeqCst);
        let second_push = source.emit_by_name::<gst::FlowReturn>(
            "push-buffer",
            &[&gst::Buffer::with_size(23).unwrap()],
        );
        std::thread::sleep(Duration::from_millis(25));
        let after_shutdown = bytes.load(Ordering::SeqCst);
        let stop_result = pipeline.set_state(gst::State::Null);

        assert_eq!(first_push, gst::FlowSuccess::Ok.into());
        assert!(first_observed);
        assert!(shutdown_completed);
        assert_eq!(second_push, gst::FlowSuccess::Ok.into());
        assert_eq!(after_shutdown, 0);
        assert!(stop_result.is_ok());
    }

    #[test]
    fn receive_pad_acceptance_keeps_first_audio_and_video_owners() {
        gst::init().unwrap();
        let registry = ReceiveWorkerRegistry::new();
        let overlay = test_overlay();
        let volume = gst::ElementFactory::make("volume").build().unwrap();
        let audio_worker = AudioControlWorker::spawn(volume, &overlay, 0.3, "test").unwrap();
        let (audio_pad, _audio_sink) = rtp_test_pads("OPUS");
        let (duplicate_audio_pad, _duplicate_audio_sink) = rtp_test_pads("OPUS");
        let (video_pad, _video_sink) = rtp_test_pads("AV1");
        let (duplicate_video_pad, _duplicate_video_sink) = rtp_test_pads("AV1");
        let bitrate_worker = watch_incoming_bitrate(&video_pad, overlay).unwrap();
        let audio = match accept_receive_pad(&registry, &audio_pad) {
            Some(AcceptedReceivePad::Audio(claim)) => claim,
            _ => panic!("first audio pad was not accepted"),
        };
        let video = match accept_receive_pad(&registry, &video_pad) {
            Some(AcceptedReceivePad::Video(claim)) => claim,
            _ => panic!("first video pad was not accepted"),
        };
        audio.complete_audio(Some(audio_worker));
        video.complete_video(Some(bitrate_worker));

        let duplicate_audio = accept_receive_pad(&registry, &duplicate_audio_pad).is_none();
        let duplicate_video = accept_receive_pad(&registry, &duplicate_video_pad).is_none();
        let workers = registry.close_and_take();
        let retained = (workers.audio.is_some(), workers.bitrate.is_some());
        let shutdown_completed = run_bounded(move || workers.shutdown());

        assert!(duplicate_audio);
        assert!(duplicate_video);
        assert_eq!(retained, (true, true));
        assert!(shutdown_completed);
    }

    #[test]
    fn receive_pad_acceptance_rolls_back_failed_claim_and_rejects_closed_registry() {
        gst::init().unwrap();
        let registry = ReceiveWorkerRegistry::new();
        let (opus, _opus_sink) = rtp_test_pads("OPUS");
        let failed = match accept_receive_pad(&registry, &opus) {
            Some(AcceptedReceivePad::Audio(claim)) => claim,
            _ => panic!("first OPUS claim was not accepted"),
        };
        drop(failed);
        let retry = accept_receive_pad(&registry, &opus);
        let retried = matches!(retry, Some(AcceptedReceivePad::Audio(_)));
        if let Some(AcceptedReceivePad::Audio(claim)) = retry {
            claim.complete_audio(None);
        }
        registry.close_and_take().shutdown();
        let rejected_after_close = accept_receive_pad(&registry, &opus).is_none();

        assert!(retried);
        assert!(rejected_after_close);
    }

    #[test]
    fn receive_worker_registry_rejects_duplicate_audio_and_video() {
        let registry = ReceiveWorkerRegistry::new();
        let audio = registry.claim_audio().unwrap();
        let video = registry.claim_video().unwrap();

        let duplicate_audio = registry.claim_audio().is_none();
        let duplicate_video = registry.claim_video().is_none();

        audio.complete_audio(None);
        video.complete_video(None);
        registry.close_and_take().shutdown();

        assert!(duplicate_audio);
        assert!(duplicate_video);
    }

    #[test]
    fn receive_worker_registry_close_waits_for_active_pad_callback() {
        let registry = ReceiveWorkerRegistry::new();
        let claim = registry.claim_audio().unwrap();
        let registry_for_close = registry.clone();
        let (closed, wait_for_close) = std::sync::mpsc::sync_channel(1);
        let closer = std::thread::spawn(move || {
            registry_for_close.close_and_take().shutdown();
            closed.send(()).unwrap();
        });
        let blocked = wait_for_close
            .recv_timeout(Duration::from_millis(25))
            .is_err();

        claim.complete_audio(None);

        let completed = wait_for_close.recv_timeout(Duration::from_secs(1));
        let joined = closer.join();

        assert!(blocked);
        assert!(completed.is_ok());
        assert!(joined.is_ok());
    }

    #[test]
    fn closed_receive_worker_registry_rejects_late_install() {
        let registry = ReceiveWorkerRegistry::new();
        registry.close_and_take().shutdown();

        assert!(registry.claim_audio().is_none());
        assert!(registry.claim_video().is_none());
    }

    #[test]
    fn rtp_caps_advertise_the_configured_frame_rate() {
        gst::init().unwrap();
        let caps = rtp_caps(Codec::H264, 120);

        assert_eq!(
            caps.structure(0)
                .unwrap()
                .get::<String>("a-framerate")
                .unwrap(),
            "120"
        );
        assert_eq!(frame_rate_from_rtp_caps(&caps), Some(120));
    }

    #[test]
    fn h264_transport_resends_headers_and_recovers_after_loss() {
        gst::init().unwrap();
        let caps = rtp_caps(Codec::H264, 60);
        let pay = build_video_payloader(Codec::H264).unwrap();
        let depay = build_video_depayloader(Codec::H264, "test").unwrap();

        assert_eq!(
            caps.structure(0)
                .unwrap()
                .get::<String>("encoding-name")
                .unwrap(),
            "H264"
        );
        assert_eq!(pay.property::<i32>("config-interval"), -1);
        assert!(depay.element.property::<bool>("request-keyframe"));
        assert!(depay.element.property::<bool>("wait-for-keyframe"));
        assert_eq!(Codec::from_rtp_encoding("H264"), Some(Codec::H264));
    }

    #[test]
    fn h264_file_output_does_not_require_av1_encoding() {
        assert_eq!(
            file_output_factories(Codec::H264),
            ("mfh264enc", "h264parse")
        );
    }

    #[test]
    fn h265_file_output_uses_cross_vendor_media_foundation() {
        assert_eq!(
            file_output_factories(Codec::H265),
            ("mfh265enc", "h265parse")
        );
    }

    #[test]
    fn opus_decoder_conceals_loss_without_fec_lookahead() {
        gst::init().unwrap();
        let decoder = build_audio_decoder("test").unwrap();

        assert!(decoder.element.property::<bool>("plc"));
        assert!(!decoder.element.property::<bool>("use-inband-fec"));
    }

    #[test]
    fn software_decoder_can_be_selected_for_diagnostic_comparison() {
        assert_eq!(av1_decoder_factory(Some("software"), false), "dav1ddec");
        assert_eq!(av1_decoder_factory(Some("software"), true), "d3d11av1dec");
        assert_eq!(av1_decoder_factory(Some("hardware"), false), "d3d11av1dec");
        assert_eq!(
            av1_decoder_factory(Some("unexpected"), false),
            "d3d11av1dec"
        );
        assert_eq!(av1_decoder_factory(None, false), "d3d11av1dec");
    }

    #[test]
    fn av1_decoder_rejects_corrupt_output_and_requests_recovery() {
        gst::init().unwrap();
        let decoder = build_av1_decoder(Some("software"), false, "test").unwrap();

        assert!(decoder
            .element
            .property::<bool>("automatic-request-sync-points"));
        assert!(decoder.element.property::<bool>("discard-corrupted-frames"));
    }

    #[test]
    fn av1_depayloader_requests_and_waits_for_recovery_keyframes() {
        gst::init().unwrap();
        let depay = build_av1_depayloader("test").unwrap();

        assert!(depay.element.property::<bool>("request-keyframe"));
        assert!(depay.element.property::<bool>("wait-for-keyframe"));
    }

    #[test]
    fn presentation_queue_keeps_only_the_live_decoded_frame() {
        gst::init().unwrap();
        let queue = build_live_video_queue("test").unwrap();

        assert_eq!(queue.element.property::<u32>("max-size-buffers"), 1);
        assert_eq!(queue.element.property::<u32>("max-size-bytes"), 0);
        assert_eq!(queue.element.property::<u64>("max-size-time"), 0);
    }

    #[test]
    fn live_sinks_do_not_wait_for_preroll() {
        gst::init().unwrap();
        let video = build_video_sink("test").unwrap();
        let audio = build_audio_sink("test").unwrap();

        assert!(!video.element.property::<bool>("async"));
        assert!(!video.element.property::<bool>("sync"));
        assert!(!audio.element.property::<bool>("async"));
        assert_eq!(audio.element.property::<i64>("buffer-time"), 40_000);
        assert_eq!(audio.element.property::<i64>("latency-time"), 10_000);
    }
}
