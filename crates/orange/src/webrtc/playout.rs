//! Keep both live outputs on one clock, including recovery from late RTP time.

use anyhow::{Context, Result};
use gstreamer as gst;
use gstreamer_base::{prelude::*, BaseSink};
use std::sync::{Arc, Mutex};

// A late stream gets room for the two WASAPI periods plus scheduling jitter.
// Do not chase each frame's jitter, or the correction would grow continuously.
const MIN_LEAD_NS: i128 = 20_000_000;
const RECOVERY_LEAD_NS: i128 = 60_000_000;
pub(super) const MAX_CORRECTION_NS: u64 = 1_000_000_000;

#[derive(Default)]
struct State {
    sinks: Vec<gst::glib::WeakRef<BaseSink>>,
    offset: i64,
    resync_audio: bool,
    expired_since: [Option<gst::ClockTime>; 2],
}

#[derive(Default)]
pub(crate) struct LivePlayout(Mutex<State>);

impl LivePlayout {
    pub(crate) fn new(pipeline: &gst::Pipeline) -> Arc<Self> {
        // Audio is optional and attached after Playing. It must not become a
        // new clock origin, or wait for its own stopped ringbuffer at startup.
        pipeline.use_clock(Some(&gst::SystemClock::obtain()));
        Arc::new(Self::default())
    }

    pub(super) fn attach(
        self: &Arc<Self>,
        element: &gst::Element,
        audio: bool,
        role: &str,
    ) -> Result<()> {
        let sink = element
            .dynamic_cast_ref::<BaseSink>()
            .context("playback output is not a clocked sink")?;
        {
            let mut state = self.0.lock().unwrap_or_else(|error| error.into_inner());
            sink.set_ts_offset(state.offset);
            state.sinks.retain(|sink| sink.upgrade().is_some());
            state.sinks.push(sink.downgrade());
        }
        let sink_weak = sink.downgrade();
        let timing = self.clone();
        let role = role.to_owned();
        element
            .static_pad("sink")
            .context("playback output has no sink pad")?
            .add_probe(gst::PadProbeType::BUFFER, move |pad, info| {
                let Some(gst::PadProbeData::Buffer(buffer)) = &mut info.data else {
                    return gst::PadProbeReturn::Ok;
                };
                // Decoder-generated startup concealment can start at PTS zero
                // although the first real RTP packet is seconds into the call.
                // It must not rebase both outputs onto that synthetic timeline.
                if buffer.flags().contains(gst::BufferFlags::GAP) {
                    return gst::PadProbeReturn::Ok;
                }
                let Some(sink) = sink_weak.upgrade() else {
                    return gst::PadProbeReturn::Ok;
                };
                let Some(now) = sink.current_running_time() else {
                    return gst::PadProbeReturn::Ok;
                };
                let Some(running) = buffer.pts().and_then(|pts| {
                    pad.sticky_event::<gst::event::Segment>(0)?
                        .segment()
                        .downcast_ref::<gst::ClockTime>()?
                        .to_running_time(pts)
                }) else {
                    return gst::PadProbeReturn::Ok;
                };
                let mut state = timing.0.lock().unwrap_or_else(|error| error.into_inner());
                match correction(
                    state.offset,
                    now,
                    running,
                    sink.latency(),
                    sink.render_delay(),
                    audio,
                ) {
                    Ok(Some(offset)) => {
                        state.offset = offset;
                        state.resync_audio = true;
                        for output in state.sinks.iter().filter_map(|sink| sink.upgrade()) {
                            output.set_ts_offset(offset);
                        }
                        crate::media_diagnostics::emit_diagnostic(
                            "playout-correction",
                            &role,
                            serde_json::json!({
                                "offset_ms": offset / 1_000_000,
                                "audio": audio,
                                "observed_running_ms": now.mseconds(),
                                "buffer_running_ms": running.mseconds(),
                                "sink_latency_ms": sink.latency().mseconds(),
                            }),
                        );
                    }
                    Err(()) => {
                        // Discard isolated obsolete startup/burst buffers. A
                        // continuously invalid timeline used to post a fatal
                        // error here, which tore the whole viewer down: three
                        // watchers lost a stream whose video had dropped no
                        // packets at all, because only the audio branch was
                        // late. Give up on scheduling this one output instead,
                        // so the viewer keeps both its picture and its sound.
                        let since = *state.expired_since[usize::from(audio)].get_or_insert(now);
                        if now.saturating_sub(since) < gst::ClockTime::SECOND {
                            return gst::PadProbeReturn::Drop;
                        }
                        drop(state);
                        // AudioBaseSink only aligns samples to their timestamps
                        // while sync is on; clearing it appends them at the
                        // ringbuffer write pointer instead, so a frozen PTS
                        // still plays rather than being discarded as late.
                        sink.set_sync(false);
                        crate::media_diagnostics::emit_diagnostic(
                            "playout-unsynchronized",
                            &role,
                            serde_json::json!({
                                "audio": audio,
                                "observed_running_ms": now.mseconds(),
                                "buffer_running_ms": running.mseconds(),
                                "sink_latency_ms": sink.latency().mseconds(),
                                "expired_for_ms": now.saturating_sub(since).mseconds(),
                            }),
                        );
                        // Remove still passes this buffer downstream; the
                        // output is unsynchronized now, so there is nothing
                        // left for the probe to correct.
                        return gst::PadProbeReturn::Remove;
                    }
                    Ok(None) => {}
                }
                state.expired_since[usize::from(audio)] = None;
                if audio && state.resync_audio {
                    // Otherwise AudioBaseSink's continuity alignment can undo
                    // the new deadline by appending to the previous sample.
                    buffer.make_mut().set_flags(gst::BufferFlags::RESYNC);
                    state.resync_audio = false;
                }
                gst::PadProbeReturn::Ok
            })
            .context("could not install playback timing probe")?;
        Ok(())
    }
}

fn correction(
    current: i64,
    now: gst::ClockTime,
    running: gst::ClockTime,
    latency: gst::ClockTime,
    render_delay: gst::ClockTime,
    audio: bool,
) -> Result<Option<i64>, ()> {
    let base = i128::from(running.nseconds()) + i128::from(latency.nseconds())
        - i128::from(render_delay.nseconds());
    let now = i128::from(now.nseconds());
    // Video is serialized behind the previous clock wait. A healthy 120fps
    // frame has only 8ms lead, so demanding audio's device lead would rebase
    // every frame and progressively delay both outputs.
    let minimum_lead = if audio { MIN_LEAD_NS } else { -40_000_000 };
    if base + i128::from(current) >= now + minimum_lead {
        return Ok(None);
    }
    let required = now + RECOVERY_LEAD_NS - base;
    if required > i128::from(MAX_CORRECTION_NS) {
        return Err(());
    }
    Ok(Some(required as i64))
}

#[cfg(test)]
mod tests {
    use super::*;
    fn ms(value: u64) -> gst::ClockTime {
        gst::ClockTime::from_mseconds(value)
    }

    #[test]
    fn clock_paced_high_frame_rate_video_does_not_increase_the_audio_delay() {
        // A clocked video sink returns from the previous frame at its deadline.
        // At 120fps, the next frame is only 8ms in the future: that is healthy.
        assert_eq!(
            correction(0, ms(1_000), ms(1_008), ms(0), ms(0), false),
            Ok(None)
        );
    }

    #[test]
    fn a_late_stream_gets_one_stable_playable_deadline() {
        // Turning sync on alone discarded every sample in the 500ms delayed
        // native reproduction. Both streams need the same forward correction.
        assert_eq!(
            correction(0, ms(1_500), ms(900), ms(140), ms(0), true),
            Ok(Some(520_000_000))
        );
        assert_eq!(
            correction(520_000_000, ms(1_510), ms(910), ms(140), ms(0), true),
            Ok(None)
        );
        assert_eq!(
            correction(0, ms(1_000), ms(900), ms(140), ms(0), true),
            Ok(None)
        );
    }

    #[test]
    fn scheduling_accounts_for_device_render_delay_and_rejects_obsolete_media() {
        assert_eq!(
            correction(0, ms(1_500), ms(900), ms(140), ms(10), true),
            Ok(Some(530_000_000))
        );
        assert_eq!(
            correction(0, ms(5_000), ms(0), ms(140), ms(0), true),
            Err(())
        );
    }

    fn input(
        pipeline: &gst::Pipeline,
    ) -> (
        gst::Element,
        gst::Element,
        std::sync::mpsc::Receiver<gst::BufferFlags>,
    ) {
        let source = gst::ElementFactory::make("appsrc")
            .property("is-live", true)
            .property_from_str("format", "time")
            .property("handle-segment-change", true)
            .build()
            .unwrap();
        let sink = gst::ElementFactory::make("fakesink")
            .property("sync", false)
            .property("async", false)
            .property("signal-handoffs", true)
            .build()
            .unwrap();
        let (sent, received) = std::sync::mpsc::sync_channel(4);
        sink.connect("handoff", false, move |values| {
            let buffer = values[1].get::<gst::Buffer>().unwrap();
            let _ = sent.try_send(buffer.flags());
            None
        });
        pipeline.add_many([&source, &sink]).unwrap();
        source.link(&sink).unwrap();
        (source, sink, received)
    }

    fn push(source: &gst::Element, running: gst::ClockTime, start: gst::ClockTime) {
        let mut buffer = gst::Buffer::with_size(16).unwrap();
        buffer.get_mut().unwrap().set_pts(start + running);
        let mut segment = gst::FormattedSegment::<gst::ClockTime>::new();
        segment.set_start(start);
        let sample = gst::Sample::builder()
            .buffer(&buffer)
            .segment(&segment)
            .build();
        assert_eq!(
            source.emit_by_name::<gst::FlowReturn>("push-sample", &[&sample]),
            gst::FlowReturn::Ok
        );
    }

    #[test]
    fn recovery_uses_segment_time_and_is_inherited_by_late_audio() {
        // MF video PTS starts at 1000 hours. Raw PTS comparisons would silently
        // skip recovery, and a late-created audio sink would miss the correction.
        gst::init().unwrap();
        let pipeline = gst::Pipeline::new();
        let timing = LivePlayout::new(&pipeline);
        let (video_source, video_sink, video_frames) = input(&pipeline);
        timing.attach(&video_sink, false, "test").unwrap();
        pipeline.set_state(gst::State::Playing).unwrap();
        let now = pipeline.clock().unwrap().time();
        video_sink.set_base_time(now - gst::ClockTime::from_seconds(2));
        push(
            &video_source,
            ms(1_500),
            gst::ClockTime::from_seconds(3_600_000),
        );
        video_frames
            .recv_timeout(std::time::Duration::from_secs(1))
            .unwrap();
        let offset = video_sink.property::<i64>("ts-offset");

        let (audio_source, audio_sink, audio_frames) = input(&pipeline);
        timing.attach(&audio_sink, true, "test").unwrap();
        let inherited = audio_sink.property::<i64>("ts-offset");
        audio_sink.sync_state_with_parent().unwrap();
        audio_source.sync_state_with_parent().unwrap();
        audio_sink.set_base_time(now - gst::ClockTime::from_seconds(2));
        push(&audio_source, ms(1_510), gst::ClockTime::ZERO);
        let first = audio_frames
            .recv_timeout(std::time::Duration::from_secs(1))
            .unwrap();
        push(&audio_source, ms(1_520), gst::ClockTime::ZERO);
        let second = audio_frames
            .recv_timeout(std::time::Duration::from_secs(1))
            .unwrap();
        pipeline.set_state(gst::State::Null).unwrap();

        assert!(
            (560_000_000..700_000_000).contains(&offset),
            "offset was {offset}"
        );
        assert_eq!(inherited, offset);
        assert!(first.contains(gst::BufferFlags::RESYNC));
        assert!(!second.contains(gst::BufferFlags::RESYNC));
        assert_eq!(
            video_sink.property::<i64>("ts-offset"),
            audio_sink.property::<i64>("ts-offset")
        );
    }

    #[test]
    fn permanently_expired_media_keeps_playing_unsynchronized_instead_of_ending_the_stream() {
        // Posting a fatal error here tore down the entire viewer. Three
        // watchers lost a stream whose video had lost no packets, because only
        // the late audio branch had left the budget. Isolated stale buffers may
        // still be discarded; a sustained fault gives up the clock, not the
        // stream.
        gst::init().unwrap();
        let pipeline = gst::Pipeline::new();
        let timing = LivePlayout::new(&pipeline);
        let (source, sink, played) = input(&pipeline);
        sink.set_property("sync", true);
        timing.attach(&sink, true, "test").unwrap();
        pipeline.set_state(gst::State::Playing).unwrap();
        let now = pipeline.clock().unwrap().time();
        sink.set_base_time(now - gst::ClockTime::from_seconds(2));
        // appsrc asks for more data after pushing its queue downstream, even
        // when our sink probe drops that buffer.
        let (sent, needs_data) = std::sync::mpsc::sync_channel(2);
        source.connect("need-data", false, move |_| {
            let _ = sent.try_send(());
            None
        });
        push(&source, gst::ClockTime::ZERO, gst::ClockTime::ZERO);
        while timing.0.lock().unwrap().expired_since[1].is_none() {
            needs_data
                .recv_timeout(std::time::Duration::from_secs(1))
                .unwrap();
        }
        sink.set_base_time(now - gst::ClockTime::from_seconds(4));
        push(&source, ms(10), gst::ClockTime::ZERO);
        let rendered = played.recv_timeout(std::time::Duration::from_secs(1));
        let error = pipeline
            .bus()
            .unwrap()
            .timed_pop_filtered(gst::ClockTime::SECOND, &[gst::MessageType::Error]);
        let synchronized = sink.property::<bool>("sync");
        pipeline.set_state(gst::State::Null).unwrap();

        rendered.expect("expired audio stayed silently dropped");
        assert!(
            error.is_none(),
            "a late audio branch ended the whole stream"
        );
        assert!(!synchronized, "late media would still be discarded as late");
    }
}
