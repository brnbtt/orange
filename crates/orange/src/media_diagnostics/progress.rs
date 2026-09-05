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
    VideoSinkInput,
    AudioRtp,
    AudioDepay,
    AudioDecoded,
    AudioSinkInput,
}

#[derive(Default)]
struct StageCounter {
    buffers: AtomicU64,
    bytes: AtomicU64,
    last_ms: AtomicU64,
    /// Presentation timestamp of the most recent buffer, stored +1 so that 0
    /// means "nothing seen yet" and a genuine PTS of 0 is not mistaken for it.
    last_pts_ms: AtomicU64,
}

pub(crate) struct MediaProgress {
    started: Instant,
    rtp: StageCounter,
    depay: StageCounter,
    parsed: StageCounter,
    decoded: StageCounter,
    video_sink_input: StageCounter,
    audio_rtp: StageCounter,
    audio_depay: StageCounter,
    audio_decoded: StageCounter,
    audio_sink_input: StageCounter,
    keyframes: AtomicU64,
    queue_overruns: AtomicU64,
}

#[derive(Debug, Serialize)]
pub(crate) struct StageSnapshot {
    buffers: u64,
    bytes: u64,
    silent_ms: Option<u64>,
    /// Presentation timestamp of the last buffer through this stage.
    pts_ms: Option<u64>,
}

#[derive(Debug, Serialize)]
pub(crate) struct MediaSnapshot {
    rtp: StageSnapshot,
    depay: StageSnapshot,
    parsed: StageSnapshot,
    decoded: StageSnapshot,
    video_sink_input: StageSnapshot,
    audio_rtp: StageSnapshot,
    audio_depay: StageSnapshot,
    audio_decoded: StageSnapshot,
    audio_sink_input: StageSnapshot,
    /// Difference of the last sink-input PTS values, not physical lip-sync.
    av_offset_ms: Option<i64>,
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
            video_sink_input: StageCounter::default(),
            audio_rtp: StageCounter::default(),
            audio_depay: StageCounter::default(),
            audio_decoded: StageCounter::default(),
            audio_sink_input: StageCounter::default(),
            keyframes: AtomicU64::new(0),
            queue_overruns: AtomicU64::new(0),
        }
    }

    pub(crate) fn record(
        &self,
        stage: MediaStage,
        bytes: usize,
        keyframe: bool,
        pts_ms: Option<u64>,
    ) {
        self.record_at(
            stage,
            bytes as u64,
            self.started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64,
            keyframe,
            pts_ms,
        );
    }

    fn record_at(
        &self,
        stage: MediaStage,
        bytes: u64,
        at_ms: u64,
        keyframe: bool,
        pts_ms: Option<u64>,
    ) {
        let counter = match stage {
            MediaStage::Rtp => &self.rtp,
            MediaStage::Depay => &self.depay,
            MediaStage::Parsed => &self.parsed,
            MediaStage::Decoded => &self.decoded,
            MediaStage::VideoSinkInput => &self.video_sink_input,
            MediaStage::AudioRtp => &self.audio_rtp,
            MediaStage::AudioDepay => &self.audio_depay,
            MediaStage::AudioDecoded => &self.audio_decoded,
            MediaStage::AudioSinkInput => &self.audio_sink_input,
        };
        counter.buffers.fetch_add(1, Ordering::Relaxed);
        counter.bytes.fetch_add(bytes, Ordering::Relaxed);
        counter
            .last_ms
            .store(at_ms.saturating_add(1), Ordering::Relaxed);
        if let Some(pts_ms) = pts_ms {
            counter
                .last_pts_ms
                .store(pts_ms.saturating_add(1), Ordering::Relaxed);
        }
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
        let video_sink_input = snapshot_stage(&self.video_sink_input, now_ms);
        let audio_sink_input = snapshot_stage(&self.audio_sink_input, now_ms);
        MediaSnapshot {
            rtp: snapshot_stage(&self.rtp, now_ms),
            depay: snapshot_stage(&self.depay, now_ms),
            parsed: snapshot_stage(&self.parsed, now_ms),
            decoded: snapshot_stage(&self.decoded, now_ms),
            audio_rtp: snapshot_stage(&self.audio_rtp, now_ms),
            audio_depay: snapshot_stage(&self.audio_depay, now_ms),
            audio_decoded: snapshot_stage(&self.audio_decoded, now_ms),
            av_offset_ms: av_offset_ms(&video_sink_input, &audio_sink_input),
            video_sink_input,
            audio_sink_input,
            keyframes: self.keyframes.load(Ordering::Relaxed),
            queue_overruns: self.queue_overruns.load(Ordering::Relaxed),
        }
    }
}

/// Compare progress at the inputs, not what the viewer sees and hears.
///
/// A 500ms-late audio marker can reach these probes before it is played. RTP
/// reconstructs timestamps at the receiver, segments may have different starts,
/// and a stalled branch leaves an old PTS here. Consequently this value neither
/// measures device buffering nor proves A/V synchronization. Keep the field for
/// diagnostic compatibility; playout uses segment running time and the clock.
fn av_offset_ms(video: &StageSnapshot, audio: &StageSnapshot) -> Option<i64> {
    let video = i64::try_from(video.pts_ms?).ok()?;
    let audio = i64::try_from(audio.pts_ms?).ok()?;
    Some(video - audio)
}

fn snapshot_stage(counter: &StageCounter, now_ms: u64) -> StageSnapshot {
    let stored = counter.last_ms.load(Ordering::Relaxed);
    let pts = counter.last_pts_ms.load(Ordering::Relaxed);
    StageSnapshot {
        buffers: counter.buffers.load(Ordering::Relaxed),
        bytes: counter.bytes.load(Ordering::Relaxed),
        silent_ms: (stored != 0).then(|| now_ms.saturating_sub(stored - 1)),
        pts_ms: (pts != 0).then(|| pts - 1),
    }
}

pub(crate) fn track_pad(pad: &gst::Pad, stage: MediaStage, progress: Arc<MediaProgress>) {
    pad.add_probe(gst::PadProbeType::BUFFER, move |_, info| {
        if let Some(gst::PadProbeData::Buffer(buffer)) = &info.data {
            let flags = buffer.flags();
            let keyframe = matches!(stage, MediaStage::Parsed) && is_keyframe(flags);
            progress.record(stage, buffer.size(), keyframe, buffer_pts_ms(buffer));
        }
        gst::PadProbeReturn::Ok
    });
}

/// Preserve the last observed PTS when this buffer has none. Its age is not
/// bounded by the newest buffer's silent_ms, so it is only progress telemetry.
fn buffer_pts_ms(buffer: &gst::BufferRef) -> Option<u64> {
    Some(buffer.pts()?.mseconds())
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
        progress.record_at(MediaStage::Rtp, 1_000, 1_400, false, None);
        progress.record_at(MediaStage::Depay, 1_000, 900, false, None);
        progress.record_at(MediaStage::Parsed, 1_000, 800, true, None);
        progress.record_at(MediaStage::Decoded, 1_000, 700, false, None);
        progress.record_at(MediaStage::AudioRtp, 160, 1_450, false, None);
        progress.record_at(MediaStage::AudioDepay, 120, 1_440, false, None);
        progress.record_at(MediaStage::AudioDecoded, 1_920, 1_430, false, None);
        progress.record_at(MediaStage::AudioSinkInput, 1_920, 1_420, false, None);

        let snapshot = progress.snapshot_at(1_500);

        assert_eq!(snapshot.rtp.buffers, 1);
        assert_eq!(snapshot.rtp.bytes, 1_000);
        assert_eq!(snapshot.rtp.silent_ms, Some(100));
        assert_eq!(snapshot.depay.silent_ms, Some(600));
        assert_eq!(snapshot.parsed.silent_ms, Some(700));
        assert_eq!(snapshot.decoded.silent_ms, Some(800));
        assert_eq!(snapshot.audio_rtp.silent_ms, Some(50));
        assert_eq!(snapshot.audio_depay.silent_ms, Some(60));
        assert_eq!(snapshot.audio_decoded.silent_ms, Some(70));
        assert_eq!(snapshot.audio_sink_input.silent_ms, Some(80));
        assert_eq!(snapshot.keyframes, 1);

        let json = serde_json::to_value(&snapshot).unwrap();
        assert_eq!(json["audio_rtp"]["buffers"], 1);
        assert_eq!(json["audio_depay"]["buffers"], 1);
        assert_eq!(json["audio_decoded"]["buffers"], 1);
        assert_eq!(json["audio_sink_input"]["buffers"], 1);
        assert!(json.get("audio_output").is_none());
    }

    #[test]
    fn audio_arriving_later_than_the_picture_reports_a_positive_offset() {
        // The reported symptom, in the units the log carries: at one instant
        // the sink is showing a frame stamped 10.000 s while the sound card is
        // being handed 9.800 s, so the sound is 200 ms behind the picture.
        let progress = MediaProgress::new();
        progress.record_at(MediaStage::VideoSinkInput, 1_000, 10, false, Some(10_000));
        progress.record_at(MediaStage::AudioSinkInput, 1_920, 10, false, Some(9_800));

        let snapshot = progress.snapshot_at(20);

        assert_eq!(snapshot.video_sink_input.pts_ms, Some(10_000));
        assert_eq!(snapshot.audio_sink_input.pts_ms, Some(9_800));
        assert_eq!(snapshot.av_offset_ms, Some(200));
        assert_eq!(
            serde_json::to_value(&snapshot).unwrap()["av_offset_ms"],
            200
        );
    }

    #[test]
    fn a_zero_timestamp_is_not_mistaken_for_a_missing_one() {
        // The first buffer of a stream is legitimately stamped 0. Storing the
        // timestamp +1 is what keeps that apart from "nothing seen yet", and
        // without it the offset would silently vanish at the start of playback.
        let progress = MediaProgress::new();
        assert_eq!(progress.snapshot_at(0).video_sink_input.pts_ms, None);
        assert_eq!(progress.snapshot_at(0).av_offset_ms, None);

        progress.record_at(MediaStage::VideoSinkInput, 1_000, 0, false, Some(0));
        progress.record_at(MediaStage::AudioSinkInput, 1_920, 0, false, Some(0));

        let snapshot = progress.snapshot_at(0);
        assert_eq!(snapshot.video_sink_input.pts_ms, Some(0));
        assert_eq!(snapshot.av_offset_ms, Some(0));
    }

    #[test]
    fn one_silent_branch_reports_no_offset_rather_than_a_wrong_one() {
        // Audio that never arrives is a different fault from audio that arrives
        // late, and reporting the video timestamp as an offset would disguise
        // the first as an enormous instance of the second.
        let progress = MediaProgress::new();
        progress.record_at(MediaStage::VideoSinkInput, 1_000, 10, false, Some(10_000));

        assert_eq!(progress.snapshot_at(20).av_offset_ms, None);
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
