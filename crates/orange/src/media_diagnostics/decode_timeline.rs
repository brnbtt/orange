//! Record where a decoder's output timeline stops following its input.
//!
//! Three viewers lost a stream when the live playout budget expired on audio
//! whose RTP and depayloader timelines were both healthy: `audio_depay` PTS ran
//! about 1.6s ahead of `audio_decoded` PTS, and the decoded PTS then stopped
//! advancing at all while the decoder kept emitting one buffer per input. The
//! two-second `media-progress` snapshot is too coarse to show that develop and
//! carries no buffer flags, so it cannot say why the decoder stalled.

use super::emit_diagnostic;
use gst::prelude::*;
use gstreamer as gst;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// A stalled decoder trips the report condition on every buffer, so rate-limit
/// it. One line per second still resolves a fault that develops over ~1.5s.
const REPORT_INTERVAL: Duration = Duration::from_secs(1);
/// Opus packets are 20ms here, so this clears ordinary reordering and jitter.
const LAG_THRESHOLD_MS: u64 = 250;

#[derive(Default)]
struct Timeline {
    input_pts_ms: Option<u64>,
    output_pts_ms: Option<u64>,
    outputs: u64,
    stalled: u64,
    reported_at: Option<Instant>,
}

#[derive(Debug, PartialEq, Eq)]
struct Report {
    lag_ms: Option<u64>,
    outputs: u64,
    stalled_outputs: u64,
}

impl Timeline {
    /// Fold one decoded buffer in, returning the report it should produce.
    fn observe(&mut self, output_pts_ms: Option<u64>, now: Instant) -> Option<Report> {
        self.outputs += 1;
        // A decoder that keeps emitting buffers without advancing its timeline
        // is the specific behaviour that starved the playout corrector.
        let stalled = matches!(
            (output_pts_ms, self.output_pts_ms),
            (Some(current), Some(previous)) if current <= previous
        );
        self.stalled += u64::from(stalled);
        // A buffer without a PTS says nothing about the timeline, so keep the
        // last real one rather than blinding the next comparison.
        self.output_pts_ms = output_pts_ms.or(self.output_pts_ms);
        let lag_ms = self
            .input_pts_ms
            .zip(output_pts_ms)
            .map(|(input, output)| input.saturating_sub(output));

        if !stalled && lag_ms.is_none_or(|lag| lag < LAG_THRESHOLD_MS) {
            return None;
        }
        if self
            .reported_at
            .is_some_and(|reported| now.saturating_duration_since(reported) < REPORT_INTERVAL)
        {
            return None;
        }
        self.reported_at = Some(now);
        Some(Report {
            lag_ms,
            outputs: std::mem::take(&mut self.outputs),
            stalled_outputs: std::mem::take(&mut self.stalled),
        })
    }
}

/// Report a decoder whose output PTS falls behind or stops tracking its input.
pub(crate) fn track_decode_timeline(input: &gst::Pad, output: &gst::Pad, role: &str) {
    let timeline = Arc::new(Mutex::new(Timeline::default()));
    let upstream = timeline.clone();
    input.add_probe(gst::PadProbeType::BUFFER, move |_, info| {
        if let Some(gst::PadProbeData::Buffer(buffer)) = &info.data {
            if let Some(pts) = buffer.pts() {
                let mut timeline = upstream.lock().unwrap_or_else(|error| error.into_inner());
                timeline.input_pts_ms = Some(pts.mseconds());
            }
        }
        gst::PadProbeReturn::Ok
    });

    let role = role.to_owned();
    output.add_probe(gst::PadProbeType::BUFFER, move |_, info| {
        let Some(gst::PadProbeData::Buffer(buffer)) = &info.data else {
            return gst::PadProbeReturn::Ok;
        };
        let output_pts_ms = buffer.pts().map(|pts| pts.mseconds());
        let mut timeline = timeline.lock().unwrap_or_else(|error| error.into_inner());
        let Some(report) = timeline.observe(output_pts_ms, Instant::now()) else {
            return gst::PadProbeReturn::Ok;
        };
        let input_pts_ms = timeline.input_pts_ms;
        drop(timeline);
        let flags = buffer.flags();
        emit_diagnostic(
            "audio-decode-timeline",
            &role,
            serde_json::json!({
                "input_pts_ms": input_pts_ms,
                "output_pts_ms": output_pts_ms,
                "lag_ms": report.lag_ms,
                "duration_ms": buffer.duration().map(|duration| duration.mseconds()),
                "outputs": report.outputs,
                "stalled_outputs": report.stalled_outputs,
                "gap": flags.contains(gst::BufferFlags::GAP),
                "discont": flags.contains(gst::BufferFlags::DISCONT),
                "resync": flags.contains(gst::BufferFlags::RESYNC),
            }),
        );
        gst::PadProbeReturn::Ok
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decoding(input_pts_ms: u64) -> Timeline {
        Timeline {
            input_pts_ms: Some(input_pts_ms),
            ..Timeline::default()
        }
    }

    #[test]
    fn a_decoder_keeping_pace_with_its_input_is_not_reported() {
        // Opus output trails its input by one packet in normal operation, and
        // a line per buffer would bury the fault this exists to find.
        let mut timeline = decoding(1_000);
        let now = Instant::now();
        assert_eq!(timeline.observe(Some(980), now), None);
        timeline.input_pts_ms = Some(1_020);
        assert_eq!(timeline.observe(Some(1_000), now), None);
    }

    #[test]
    fn a_decoder_whose_output_stops_advancing_is_reported_even_without_lag() {
        // 37 consecutive decoded buffers carried the same PTS while the sink
        // clock ran on. Repeating a timestamp is the fault, not falling behind.
        let mut timeline = decoding(2_547);
        let now = Instant::now();
        assert_eq!(timeline.observe(Some(2_547), now), None);
        assert_eq!(
            timeline.observe(Some(2_547), now),
            Some(Report {
                lag_ms: Some(0),
                outputs: 2,
                stalled_outputs: 1,
            })
        );
    }

    #[test]
    fn a_decoder_falling_behind_its_input_is_reported_once_per_interval() {
        // The observed fault held a ~1.6s gap for seconds; reporting each of
        // the 50 buffers a second would make the log unreadable.
        let mut timeline = decoding(4_499);
        let start = Instant::now();
        assert_eq!(
            timeline.observe(Some(2_547), start),
            Some(Report {
                lag_ms: Some(1_952),
                outputs: 1,
                stalled_outputs: 0,
            })
        );
        assert_eq!(
            timeline.observe(Some(2_567), start + REPORT_INTERVAL / 2),
            None
        );
        // The counters keep accumulating across the silent window, so the next
        // report still accounts for every buffer the decoder produced.
        assert_eq!(
            timeline.observe(Some(2_587), start + REPORT_INTERVAL),
            Some(Report {
                lag_ms: Some(1_912),
                outputs: 2,
                stalled_outputs: 0,
            })
        );
    }
}
