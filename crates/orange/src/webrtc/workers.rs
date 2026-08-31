use anyhow::{Context, Result};
use gst::prelude::*;
use gstreamer as gst;
use std::io::Write;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Condvar, Mutex, TryLockError};
use std::time::{Duration, Instant};

fn panic_detail(panic: &(dyn std::any::Any + Send)) -> &str {
    panic
        .downcast_ref::<&str>()
        .copied()
        .or_else(|| panic.downcast_ref::<String>().map(String::as_str))
        .unwrap_or("unknown panic payload")
}

fn join_worker(worker: std::thread::JoinHandle<()>, label: &str) -> Result<()> {
    worker
        .join()
        .map_err(|panic| anyhow::anyhow!("{label} panicked: {}", panic_detail(&*panic)))
}

fn combine_shutdown_results(first: Result<()>, second: Result<()>) -> Result<()> {
    match (first, second) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(error), Ok(())) | (Ok(()), Err(error)) => Err(error),
        (Err(first), Err(second)) => Err(anyhow::anyhow!("{first:#}; {second:#}")),
    }
}

pub(crate) struct AudioControlWorker {
    stop: Option<mpsc::Sender<()>>,
    worker: Option<std::thread::JoinHandle<()>>,
}

impl AudioControlWorker {
    pub(super) fn spawn(
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

    fn shutdown(&mut self) -> Result<()> {
        self.stop.take();
        if let Some(worker) = self.worker.take() {
            join_worker(worker, "audio control worker")?;
        }
        Ok(())
    }
}

impl Drop for AudioControlWorker {
    fn drop(&mut self) {
        if let Err(error) = self.shutdown() {
            let _ = writeln!(
                std::io::stderr().lock(),
                "[webrtc] audio control shutdown failed: {error:#}"
            );
        }
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

    fn shutdown(&mut self) -> Result<()> {
        self.stop.take();
        if let (Some(pad), Some(probe)) = (self.pad.upgrade(), self.probe.take()) {
            pad.remove_probe(probe);
        }
        if let Some(worker) = self.worker.take() {
            join_worker(worker, "incoming bitrate worker")?;
        }
        Ok(())
    }
}

impl Drop for IncomingBitrateWorker {
    fn drop(&mut self) {
        if let Err(error) = self.shutdown() {
            let _ = writeln!(
                std::io::stderr().lock(),
                "[webrtc] incoming bitrate shutdown failed: {error:#}"
            );
        }
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
    watch_incoming_bitrate_with_counter_and_interval(pad, overlay, bytes, Duration::from_secs(1))
}

fn watch_incoming_bitrate_with_counter_and_interval(
    pad: &gst::Pad,
    overlay: crate::overlay::SharedOverlay,
    bytes: Arc<AtomicU64>,
    sample_interval: Duration,
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
            match stop.recv_timeout(sample_interval) {
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

    pub(super) fn claim_audio(&self) -> Option<ReceivePadClaim> {
        self.claim(ReceiveKind::Audio)
    }

    pub(super) fn claim_video(&self) -> Option<ReceivePadClaim> {
        self.claim(ReceiveKind::Video)
    }

    fn close_and_take(&self) -> ReceiveWorkers {
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

    pub(crate) fn shutdown(&self) -> Result<()> {
        self.close_and_take().shutdown()
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

struct ReceiveWorkers {
    audio: Option<AudioControlWorker>,
    bitrate: Option<IncomingBitrateWorker>,
}

impl ReceiveWorkers {
    fn shutdown(mut self) -> Result<()> {
        let bitrate = self
            .bitrate
            .as_mut()
            .map_or(Ok(()), IncomingBitrateWorker::shutdown);
        let audio = self
            .audio
            .as_mut()
            .map_or(Ok(()), AudioControlWorker::shutdown);
        combine_shutdown_results(bitrate, audio)
    }
}

#[cfg(test)]
mod tests {
    use super::super::accept_receive_pad;
    use super::*;
    use crate::test_support::run_in_bounded_subprocess;
    use std::sync::atomic::Ordering;
    use std::time::{Duration, Instant};

    fn wait_for_thread(worker: Option<&std::thread::JoinHandle<()>>) -> bool {
        let deadline = Instant::now() + Duration::from_secs(1);
        while worker.is_some_and(|worker| !worker.is_finished()) && Instant::now() < deadline {
            std::thread::yield_now();
        }
        worker.is_none_or(std::thread::JoinHandle::is_finished)
    }

    fn shutdown_receive_workers_bounded(mut workers: ReceiveWorkers) -> Result<()> {
        if let Some(worker) = &mut workers.audio {
            worker.stop.take();
        }
        if let Some(worker) = &mut workers.bitrate {
            worker.stop.take();
        }
        let audio_finished = workers
            .audio
            .as_ref()
            .is_none_or(|worker| wait_for_thread(worker.worker.as_ref()));
        let bitrate_finished = workers
            .bitrate
            .as_ref()
            .is_none_or(|worker| wait_for_thread(worker.worker.as_ref()));
        if !audio_finished || !bitrate_finished {
            anyhow::bail!("receive worker did not stop before test deadline");
        }
        workers.shutdown()
    }

    fn test_overlay() -> crate::overlay::SharedOverlay {
        Arc::new(Mutex::new(crate::overlay::OverlayState::new(
            crate::window::PlaybackProfile::FriendViewer { cascade: 0 },
        )))
    }

    fn panicking_audio_worker(message: &'static str) -> AudioControlWorker {
        let (stop, _wait_for_stop) = mpsc::channel();
        AudioControlWorker {
            stop: Some(stop),
            worker: Some(std::thread::spawn(move || panic!("{message}"))),
        }
    }

    fn panicking_bitrate_worker(message: &'static str) -> IncomingBitrateWorker {
        let pad = gst::Pad::builder(gst::PadDirection::Src).build();
        let (stop, _wait_for_stop) = mpsc::channel();
        IncomingBitrateWorker {
            pad: pad.downgrade(),
            probe: None,
            stop: Some(stop),
            worker: Some(std::thread::spawn(move || panic!("{message}"))),
        }
    }

    #[test]
    fn remaining_peer_review_explicit_worker_shutdown_identifies_panics() {
        gst::init().unwrap();
        let mut audio = panicking_audio_worker("audio panic detail");
        let mut bitrate = panicking_bitrate_worker("bitrate panic detail");

        let audio_error = audio.shutdown().unwrap_err().to_string();
        let bitrate_error = bitrate.shutdown().unwrap_err().to_string();

        assert!(audio_error.contains("audio control worker panicked"));
        assert!(audio_error.contains("audio panic detail"));
        assert!(bitrate_error.contains("incoming bitrate worker panicked"));
        assert!(bitrate_error.contains("bitrate panic detail"));
    }

    #[test]
    fn remaining_peer_review_receive_shutdown_aggregates_both_panics() {
        gst::init().unwrap();
        let registry = ReceiveWorkerRegistry::new();
        registry
            .claim_audio()
            .unwrap()
            .complete_audio(Some(panicking_audio_worker("audio aggregate detail")));
        registry
            .claim_video()
            .unwrap()
            .complete_video(Some(panicking_bitrate_worker("bitrate aggregate detail")));

        let error = registry.shutdown().unwrap_err().to_string();

        assert!(error.contains("audio aggregate detail"));
        assert!(error.contains("bitrate aggregate detail"));
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

    #[test]
    fn audio_control_receive_worker_cancels_while_overlay_is_locked() {
        if run_in_bounded_subprocess(
            "ORANGE_TEST_AUDIO_CANCEL_CHILD",
            "webrtc::workers::tests::audio_control_receive_worker_cancels_while_overlay_is_locked",
        ) {
            return;
        }
        gst::init().unwrap();
        let overlay = test_overlay();
        let guard = overlay.lock().unwrap();
        let volume = gst::ElementFactory::make("volume").build().unwrap();
        let mut worker = AudioControlWorker::spawn(volume, &overlay, 0.3, "test").unwrap();
        worker.stop.take();
        let prompt_completion = wait_for_thread(worker.worker.as_ref());
        drop(guard);
        let shutdown_result = worker.shutdown();

        assert!(prompt_completion);
        assert!(shutdown_result.is_ok());
    }

    #[test]
    fn incoming_bitrate_receive_worker_cancels_while_overlay_is_locked() {
        if run_in_bounded_subprocess(
            "ORANGE_TEST_BITRATE_CANCEL_CHILD",
            "webrtc::workers::tests::incoming_bitrate_receive_worker_cancels_while_overlay_is_locked",
        ) {
            return;
        }
        gst::init().unwrap();
        let overlay = test_overlay();
        let guard = overlay.lock().unwrap();
        let pad = gst::Pad::builder(gst::PadDirection::Src).build();
        let mut worker = watch_incoming_bitrate(&pad, overlay.clone()).unwrap();
        worker.stop.take();
        let prompt_completion = wait_for_thread(worker.worker.as_ref());
        drop(guard);
        let shutdown_result = worker.shutdown();

        assert!(prompt_completion);
        assert!(shutdown_result.is_ok());
    }

    #[test]
    fn incoming_bitrate_receive_worker_removes_probe_on_shutdown() {
        if run_in_bounded_subprocess(
            "ORANGE_TEST_BITRATE_PROBE_CHILD",
            "webrtc::workers::tests::incoming_bitrate_receive_worker_removes_probe_on_shutdown",
        ) {
            return;
        }
        gst::init().unwrap();
        let pipeline = gst::parse::launch(
            "appsrc name=bitrate-source is-live=true format=time ! fakesink name=bitrate-sink sync=false",
        )
        .unwrap()
        .downcast::<gst::Pipeline>()
        .unwrap();
        let source = pipeline.by_name("bitrate-source").unwrap();
        let pad = source.static_pad("src").unwrap();
        let sink_pad = pipeline
            .by_name("bitrate-sink")
            .unwrap()
            .static_pad("sink")
            .unwrap();
        let (arrived, wait_for_arrival) = std::sync::mpsc::sync_channel(2);
        sink_pad.add_probe(gst::PadProbeType::BUFFER, move |_, info| {
            if let Some(gst::PadProbeData::Buffer(buffer)) = &info.data {
                let _ = arrived.try_send(buffer.size());
            }
            gst::PadProbeReturn::Ok
        });
        let bytes = Arc::new(AtomicU64::new(0));
        let mut worker = watch_incoming_bitrate_with_counter_and_interval(
            &pad,
            test_overlay(),
            bytes.clone(),
            Duration::from_secs(60),
        )
        .unwrap();
        pipeline.set_state(gst::State::Playing).unwrap();
        let first_push = source.emit_by_name::<gst::FlowReturn>(
            "push-buffer",
            &[&gst::Buffer::with_size(17).unwrap()],
        );
        let first_arrival = wait_for_arrival.recv_timeout(Duration::from_secs(1));
        let first_observed = bytes.load(Ordering::SeqCst) == 17;
        worker.stop.take();
        let worker_finished = wait_for_thread(worker.worker.as_ref());
        let shutdown_result = worker.shutdown();
        bytes.store(0, Ordering::SeqCst);
        let second_push = source.emit_by_name::<gst::FlowReturn>(
            "push-buffer",
            &[&gst::Buffer::with_size(23).unwrap()],
        );
        let second_arrival = wait_for_arrival.recv_timeout(Duration::from_secs(1));
        let after_shutdown = bytes.load(Ordering::SeqCst);
        let stop_result = pipeline.set_state(gst::State::Null);

        assert_eq!(first_push, gst::FlowSuccess::Ok.into());
        assert_eq!(first_arrival.unwrap(), 17);
        assert!(first_observed);
        assert!(worker_finished);
        assert!(shutdown_result.is_ok());
        assert_eq!(second_push, gst::FlowSuccess::Ok.into());
        assert_eq!(second_arrival.unwrap(), 23);
        assert_eq!(after_shutdown, 0);
        assert!(stop_result.is_ok());
    }

    #[test]
    fn receive_pad_acceptance_keeps_first_audio_and_video_owners() {
        if run_in_bounded_subprocess(
            "ORANGE_TEST_RECEIVE_OWNERS_CHILD",
            "webrtc::workers::tests::receive_pad_acceptance_keeps_first_audio_and_video_owners",
        ) {
            return;
        }
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
        let shutdown_result = shutdown_receive_workers_bounded(workers);

        assert!(duplicate_audio);
        assert!(duplicate_video);
        assert_eq!(retained, (true, true));
        assert!(shutdown_result.is_ok());
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
        registry.close_and_take().shutdown().unwrap();
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
        registry.close_and_take().shutdown().unwrap();

        assert!(duplicate_audio);
        assert!(duplicate_video);
    }

    #[test]
    fn receive_worker_registry_close_waits_for_active_pad_callback() {
        if run_in_bounded_subprocess(
            "ORANGE_TEST_REGISTRY_CLOSE_CHILD",
            "webrtc::workers::tests::receive_worker_registry_close_waits_for_active_pad_callback",
        ) {
            return;
        }
        let registry = ReceiveWorkerRegistry::new();
        let claim = registry.claim_audio().unwrap();
        let registry_for_close = registry.clone();
        let (closing, wait_for_closing) = std::sync::mpsc::sync_channel(1);
        let (closed, wait_for_close) = std::sync::mpsc::sync_channel(1);
        let closer = std::thread::spawn(move || {
            let _ = closing.send(());
            let result = registry_for_close.close_and_take().shutdown();
            closed.send(result).unwrap();
        });
        let closing_started = wait_for_closing.recv_timeout(Duration::from_secs(1));
        let blocked = wait_for_close.try_recv().is_err();

        claim.complete_audio(None);

        let completed = wait_for_close.recv_timeout(Duration::from_secs(1));
        let joined = completed.as_ref().ok().map(|_| closer.join());

        assert!(closing_started.is_ok());
        assert!(blocked);
        assert!(matches!(completed, Ok(Ok(()))));
        assert!(matches!(joined, Some(Ok(()))));
    }

    #[test]
    fn closed_receive_worker_registry_rejects_late_install() {
        let registry = ReceiveWorkerRegistry::new();
        registry.close_and_take().shutdown().unwrap();

        assert!(registry.claim_audio().is_none());
        assert!(registry.claim_video().is_none());
    }
}
