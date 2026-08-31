use gst::prelude::*;
use gstreamer as gst;
use serde::Serialize;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Instant;

#[derive(Clone, Copy)]
pub(crate) enum MediaStage {
    Rtp,
    Depay,
    Parsed,
    Decoded,
}

#[derive(Default)]
struct StageCounter {
    buffers: AtomicU64,
    bytes: AtomicU64,
    last_ms: AtomicU64,
}

pub(crate) struct MediaProgress {
    started: Instant,
    rtp: StageCounter,
    depay: StageCounter,
    parsed: StageCounter,
    decoded: StageCounter,
    keyframes: AtomicU64,
    queue_overruns: AtomicU64,
}

#[derive(Debug, Serialize)]
pub(crate) struct StageSnapshot {
    buffers: u64,
    bytes: u64,
    silent_ms: Option<u64>,
}

#[derive(Debug, Serialize)]
pub(crate) struct MediaSnapshot {
    rtp: StageSnapshot,
    depay: StageSnapshot,
    parsed: StageSnapshot,
    decoded: StageSnapshot,
    keyframes: u64,
    queue_overruns: u64,
}

impl MediaProgress {
    pub(crate) fn new() -> Self {
        Self {
            started: Instant::now(),
            rtp: StageCounter::default(),
            depay: StageCounter::default(),
            parsed: StageCounter::default(),
            decoded: StageCounter::default(),
            keyframes: AtomicU64::new(0),
            queue_overruns: AtomicU64::new(0),
        }
    }

    pub(crate) fn record(&self, stage: MediaStage, bytes: usize, keyframe: bool) {
        self.record_at(
            stage,
            bytes as u64,
            self.started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64,
            keyframe,
        );
    }

    fn record_at(&self, stage: MediaStage, bytes: u64, at_ms: u64, keyframe: bool) {
        let counter = match stage {
            MediaStage::Rtp => &self.rtp,
            MediaStage::Depay => &self.depay,
            MediaStage::Parsed => &self.parsed,
            MediaStage::Decoded => &self.decoded,
        };
        counter.buffers.fetch_add(1, Ordering::Relaxed);
        counter.bytes.fetch_add(bytes, Ordering::Relaxed);
        counter
            .last_ms
            .store(at_ms.saturating_add(1), Ordering::Relaxed);
        if keyframe {
            self.keyframes.fetch_add(1, Ordering::Relaxed);
        }
    }

    pub(crate) fn snapshot(&self) -> MediaSnapshot {
        let now_ms = self.started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64;
        self.snapshot_at(now_ms)
    }

    pub(crate) fn record_queue_overrun(&self) {
        self.queue_overruns.fetch_add(1, Ordering::Relaxed);
    }

    fn snapshot_at(&self, now_ms: u64) -> MediaSnapshot {
        MediaSnapshot {
            rtp: snapshot_stage(&self.rtp, now_ms),
            depay: snapshot_stage(&self.depay, now_ms),
            parsed: snapshot_stage(&self.parsed, now_ms),
            decoded: snapshot_stage(&self.decoded, now_ms),
            keyframes: self.keyframes.load(Ordering::Relaxed),
            queue_overruns: self.queue_overruns.load(Ordering::Relaxed),
        }
    }
}

fn snapshot_stage(counter: &StageCounter, now_ms: u64) -> StageSnapshot {
    let stored = counter.last_ms.load(Ordering::Relaxed);
    StageSnapshot {
        buffers: counter.buffers.load(Ordering::Relaxed),
        bytes: counter.bytes.load(Ordering::Relaxed),
        silent_ms: (stored != 0).then(|| now_ms.saturating_sub(stored - 1)),
    }
}

pub(crate) fn track_pad(pad: &gst::Pad, stage: MediaStage, progress: Arc<MediaProgress>) {
    pad.add_probe(gst::PadProbeType::BUFFER, move |_, info| {
        if let Some(gst::PadProbeData::Buffer(buffer)) = &info.data {
            let flags = buffer.flags();
            let keyframe = matches!(stage, MediaStage::Parsed) && is_keyframe(flags);
            progress.record(stage, buffer.size(), keyframe);
        }
        gst::PadProbeReturn::Ok
    });
}

fn is_keyframe(flags: gst::BufferFlags) -> bool {
    !flags.contains(gst::BufferFlags::DELTA_UNIT)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn progress_snapshot_identifies_the_last_advancing_media_stage() {
        let progress = MediaProgress::new();
        progress.record_at(MediaStage::Rtp, 1_000, 1_400, false);
        progress.record_at(MediaStage::Depay, 1_000, 900, false);
        progress.record_at(MediaStage::Parsed, 1_000, 800, true);
        progress.record_at(MediaStage::Decoded, 1_000, 700, false);

        let snapshot = progress.snapshot_at(1_500);

        assert_eq!(snapshot.rtp.buffers, 1);
        assert_eq!(snapshot.rtp.bytes, 1_000);
        assert_eq!(snapshot.rtp.silent_ms, Some(100));
        assert_eq!(snapshot.depay.silent_ms, Some(600));
        assert_eq!(snapshot.parsed.silent_ms, Some(700));
        assert_eq!(snapshot.decoded.silent_ms, Some(800));
        assert_eq!(snapshot.keyframes, 1);
    }

    #[test]
    fn sequence_header_keyframes_are_counted() {
        assert!(is_keyframe(
            gst::BufferFlags::HEADER | gst::BufferFlags::MARKER
        ));
        assert!(is_keyframe(gst::BufferFlags::MARKER));
        assert!(!is_keyframe(
            gst::BufferFlags::DELTA_UNIT | gst::BufferFlags::MARKER
        ));
    }
}
