//! Hardware acceptance: observe a decoded video marker and capture the actual
//! sound-device output. Sink-input PTS and BaseSink rendered counts cannot
//! detect the silent late-sample path that motivated this regression test.

use super::*;
use std::sync::{
    atomic::{AtomicBool, AtomicU64, Ordering},
    mpsc,
};
use std::time::Duration;

struct StopPipeline(gst::Pipeline);
impl Drop for StopPipeline {
    fn drop(&mut self) {
        let _ = self.0.set_state(gst::State::Null);
    }
}

#[test]
#[ignore = "requires Windows audio loopback and a D3D11 hardware encoder; plays two quiet markers"]
fn delayed_audio_is_audible_and_matches_the_presented_video_marker() {
    // Both sinks sync=false makes the picture about 500ms early. Restoring
    // audio sync alone makes the sound disappear. The test must reject both.
    gst::init().unwrap();
    for fps in [60, 120] {
        delayed_marker_at_frame_rate(fps);
    }
}

fn delayed_marker_at_frame_rate(fps: u32) {
    let owner = crate::window::PlaybackWindow::spawn(
        "orange A/V timing acceptance",
        crate::window::PlaybackProfile::FriendViewer { cascade: 0 },
    )
    .unwrap();
    let pipeline = gst::Pipeline::new();
    let _stop = StopPipeline(pipeline.clone());
    let playout = LivePlayout::new(&pipeline);
    let video = gst::parse::bin_from_description(
        &format!("videotestsrc name=marker-video is-live=true num-buffers={} pattern=black \
         ! video/x-raw,format=BGRA,width=320,height=180,framerate={fps}/1 \
         ! d3d11upload ! d3d11convert \
         ! video/x-raw(memory:D3D11Memory),format=NV12 \
         ! mfh264enc bitrate=2000 low-latency=true ! h264parse \
         ! rtph264pay config-interval=-1 \
         ! capsfilter caps=\"application/x-rtp,media=video,encoding-name=H264,payload=96,clock-rate=90000\"", fps * 3), true,
    ).unwrap();
    let audio = gst::parse::bin_from_description(
        "appsrc name=marker-audio is-live=true format=time \
         caps=audio/x-raw,format=F32LE,layout=interleaved,rate=48000,channels=2 \
         ! audioconvert ! opusenc frame-size=10 ! rtpopuspay pt=111 \
         ! identity sync=true ts-offset=500000000",
        true,
    )
    .unwrap();
    pipeline.add_many([&video, &audio]).unwrap();
    let video_pad = video.static_pad("src").unwrap();
    video_pad.set_active(true).unwrap();
    video_pad
        .store_sticky_event(&gst::event::StreamStart::new("av-test"))
        .unwrap();
    video_pad
        .store_sticky_event(&gst::event::Caps::new(&video_pad.query_caps(None)))
        .unwrap();
    build_receive_branch(
        &pipeline,
        &video.static_pad("src").unwrap(),
        ReceiveOutput::Window(owner.handle()),
        None,
        &playout,
        "av-test",
    )
    .unwrap();
    build_audio_branch(
        &pipeline,
        &audio.static_pad("src").unwrap(),
        None,
        None,
        &playout,
        "av-test",
    )
    .unwrap();

    let video_sink = pipeline
        .children()
        .into_iter()
        .find(|element| {
            element
                .factory()
                .is_some_and(|factory| factory.name() == "d3d11videosink")
        })
        .unwrap();
    video_sink.set_property("emit-present", true);
    let marker_pts = Arc::new(AtomicU64::new(u64::MAX));
    let pts_for_source = marker_pts.clone();
    video
        .by_name("marker-video")
        .unwrap()
        .static_pad("src")
        .unwrap()
        .add_probe(gst::PadProbeType::BUFFER, move |_, info| {
            if let Some(gst::PadProbeData::Buffer(buffer)) = &mut info.data {
                if let Some(pts) = buffer.pts().filter(|pts| pts.mseconds() >= 900) {
                    if pts_for_source
                        .compare_exchange(
                            u64::MAX,
                            pts.nseconds(),
                            Ordering::SeqCst,
                            Ordering::SeqCst,
                        )
                        .is_ok()
                    {
                        buffer
                            .make_mut()
                            .map_writable()
                            .unwrap()
                            .as_mut_slice()
                            .fill(255);
                    }
                }
            }
            gst::PadProbeReturn::Ok
        });
    let (presented, presentation) = mpsc::sync_channel(1);
    let (finished, steady_playback) = mpsc::sync_channel(1);
    let rendered_pts = Arc::new(std::sync::Mutex::new(Vec::new()));
    let pts_for_sink = rendered_pts.clone();
    let seen = AtomicBool::new(false);
    let weak = pipeline.downgrade();
    video_sink.connect("present", false, move |values| {
        let sink = values[0].get::<gst::Element>().unwrap();
        if let Some(sample) = sink.property::<Option<gst::Sample>>("last-sample") {
            if let Some(pts) = sample
                .buffer()
                .and_then(|buffer| buffer.pts())
                .and_then(|pts| {
                    sample
                        .segment()?
                        .downcast_ref::<gst::ClockTime>()?
                        .to_running_time(pts)
                })
            {
                pts_for_sink.lock().unwrap().push(pts.mseconds());
                if pts.mseconds() >= 1_800 {
                    let _ = finished.try_send(());
                }
                let marker = marker_pts.load(Ordering::SeqCst);
                if pts.nseconds().abs_diff(marker) < 1_000_000 && !seen.swap(true, Ordering::SeqCst)
                {
                    if let Some(now) = weak
                        .upgrade()
                        .and_then(|pipeline| pipeline.current_running_time())
                    {
                        let _ = presented.try_send(now);
                    }
                }
            }
        }
        None
    });

    let capture = gst::parse::launch(&format!(
        "wasapi2src loopback=true loopback-mode=include-process-tree loopback-target-pid={} \
         ! audioconvert ! audio/x-raw,format=F32LE,rate=48000,channels=2 \
         ! appsink name=heard emit-signals=true sync=false",
        std::process::id(),
    ))
    .unwrap()
    .downcast::<gst::Pipeline>()
    .unwrap();
    let _stop_capture = StopPipeline(capture.clone());
    let (heard, audible) = mpsc::sync_channel(1);
    let capture_stats = Arc::new(std::sync::Mutex::new((0usize, 0.0f32)));
    let stats_for_capture = capture_stats.clone();
    let heard_once = AtomicBool::new(false);
    let weak = pipeline.downgrade();
    capture
        .by_name("heard")
        .unwrap()
        .connect("new-sample", false, move |values| {
            let sink = values[0].get::<gst::Element>().unwrap();
            let sample = sink.emit_by_name::<gst::Sample>("pull-sample", &[]);
            let map = sample.buffer().unwrap().map_readable().unwrap();
            let peak = map
                .as_slice()
                .as_chunks::<4>()
                .0
                .iter()
                .map(|bytes| f32::from_le_bytes(*bytes).abs())
                .fold(0.0f32, f32::max);
            {
                let mut stats = stats_for_capture.lock().unwrap();
                stats.0 += 1;
                stats.1 = stats.1.max(peak);
            }
            let nonzero = peak > 0.001;
            if nonzero {
                if let Some(now) = weak
                    .upgrade()
                    .and_then(|pipeline| pipeline.current_running_time())
                {
                    if !heard_once.swap(true, Ordering::SeqCst) {
                        let _ = heard.try_send(now);
                    }
                }
            }
            Some(gst::FlowReturn::Ok.to_value())
        });

    // Use the same dynamic latency recalculation as host/watch. Identity's
    // artificial transport delay deliberately isn't advertised as latency.
    let weak = pipeline.downgrade();
    pipeline.bus().unwrap().set_sync_handler(move |_, message| {
        if matches!(message.view(), gst::MessageView::Latency(_)) {
            if let Some(pipeline) = weak.upgrade() {
                pipeline.call_async(|pipeline| {
                    let _ = pipeline.recalculate_latency();
                });
            }
        }
        gst::BusSyncReply::Pass
    });
    capture.set_state(gst::State::Playing).unwrap();
    pipeline.set_state(gst::State::Playing).unwrap();
    let source = audio.by_name("marker-audio").unwrap();
    for packet in 0..200u64 {
        let mut buffer = gst::Buffer::with_size(480 * 2 * 4).unwrap();
        let writable = buffer.get_mut().unwrap();
        writable.set_pts(gst::ClockTime::from_mseconds(packet * 10));
        writable.set_duration(gst::ClockTime::from_mseconds(10));
        let mut map = writable.map_writable().unwrap();
        for (index, bytes) in map
            .as_mut_slice()
            .as_chunks_mut::<4>()
            .0
            .iter_mut()
            .enumerate()
        {
            let sample = if (90..95).contains(&packet) {
                let phase =
                    (packet * 480 + index as u64 / 2) as f32 * 440.0 * std::f32::consts::TAU
                        / 48_000.0;
                phase.sin() * 0.03
            } else {
                0.0
            };
            *bytes = sample.to_le_bytes();
        }
        drop(map);
        assert_eq!(
            source.emit_by_name::<gst::FlowReturn>("push-buffer", &[&buffer]),
            gst::FlowReturn::Ok
        );
    }
    assert_eq!(
        source.emit_by_name::<gst::FlowReturn>("end-of-stream", &[]),
        gst::FlowReturn::Ok
    );
    let heard = audible.recv_timeout(Duration::from_secs(4));
    let shown = presentation.recv_timeout(Duration::from_secs(1));
    let offset_at_marker = video_sink.property::<i64>("ts-offset");
    let continued = steady_playback.recv_timeout(Duration::from_secs(2));
    for message in capture.bus().unwrap().iter() {
        if let gst::MessageView::Error(error) = message.view() {
            panic!(
                "output capture failed: {} ({:?})",
                error.error(),
                error.debug()
            );
        }
    }
    for message in pipeline.bus().unwrap().iter() {
        if let gst::MessageView::Error(error) = message.view() {
            panic!("playback failed: {} ({:?})", error.error(), error.debug());
        }
    }
    let heard = heard.expect("the sound device never played the audio marker");
    let shown = shown.expect("the decoded video marker was never presented");
    continued.expect("video stopped after synchronizing with audio");
    let steady_frames = rendered_pts
        .lock()
        .unwrap()
        .iter()
        .filter(|pts| (900..1_800).contains(*pts))
        .count();
    assert!(
        steady_frames >= (fps * 9 / 10 - 4) as usize,
        "only {steady_frames} frames presented in 900ms after recovery"
    );
    let offset_ms = heard.mseconds() as i64 - shown.mseconds() as i64;
    eprintln!("actual loopback audio={heard}, D3D video={shown}, difference={offset_ms}ms");
    let final_offset = video_sink.property::<i64>("ts-offset");
    assert!(
        final_offset - offset_at_marker < 100_000_000,
        "{fps}fps made the playout delay keep growing"
    );
    eprintln!(
        "steady frames={steady_frames}, captured buffers/peak={:?}",
        capture_stats.lock().unwrap()
    );
    assert!(
        offset_ms.abs() <= 100,
        "audio/video marker separation was {offset_ms}ms"
    );
}
