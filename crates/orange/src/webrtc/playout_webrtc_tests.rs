//! Hardware acceptance across genuine WebRTC peers. Content, not sender PTS,
//! identifies the marker because the RTP receiver reconstructs its own timeline.

use super::*;
use std::sync::{
    atomic::{AtomicBool, AtomicU64, Ordering},
    mpsc,
};
use std::time::{Duration, Instant};

struct StopPipeline(gst::Pipeline);
impl Drop for StopPipeline {
    fn drop(&mut self) {
        let _ = self.0.set_state(gst::State::Null);
    }
}

fn observe_white_marker(sink: &gst::Element, shown: mpsc::SyncSender<Duration>, epoch: Instant) {
    sink.set_property("emit-present", true);
    let seen = AtomicBool::new(false);
    sink.connect("present", false, move |values| {
        if seen.load(Ordering::Relaxed) {
            return None;
        }
        let now = epoch.elapsed();
        let sink = values[0].get::<gst::Element>().unwrap();
        let sample = sink.property::<Option<gst::Sample>>("last-sample")?;
        let info = gstreamer_video::VideoInfo::from_caps(sample.caps()?).ok()?;
        // This test-only readable map downloads the decoded GPU surface. No
        // production receive element or zero-copy negotiation is replaced.
        let frame =
            gstreamer_video::VideoFrameRef::from_buffer_ref_readable(sample.buffer()?, &info)
                .ok()?;
        assert_eq!(info.format(), gstreamer_video::VideoFormat::Nv12);
        if frame
            .plane_data(0)
            .ok()?
            .get(..64)
            .is_some_and(|luma| luma.iter().all(|value| *value > 200))
            && !seen.swap(true, Ordering::Relaxed)
        {
            let _ = shown.try_send(now);
        }
        None
    });
}

fn marker_separation(delay_ms: u64, stress: bool) -> i64 {
    let owner = crate::window::PlaybackWindow::spawn(
        "orange WebRTC A/V acceptance",
        crate::window::PlaybackProfile::FriendViewer { cascade: 0 },
    )
    .unwrap();
    let send_pipeline = gst::Pipeline::new();
    let receive_pipeline = gst::Pipeline::new();
    let _stop_send = StopPipeline(send_pipeline.clone());
    let _stop_receive = StopPipeline(receive_pipeline.clone());
    let epoch = Instant::now();
    let playout = LivePlayout::new(&receive_pipeline);
    let (codec, width, height, fps, bitrate, pattern) = if stress {
        ("h265", 1920, 1080, 60, 80_000, "snow")
    } else {
        ("h264", 320, 180, 30, 2_000, "black")
    };
    let encoding = codec.to_ascii_uppercase();
    let video = gst::parse::bin_from_description(
        &format!("videotestsrc name=marker-video is-live=true num-buffers={} pattern={pattern} \
         ! video/x-raw,format=BGRA,width={width},height={height},framerate={fps}/1 \
         ! d3d11upload ! d3d11convert \
         ! video/x-raw(memory:D3D11Memory),format=NV12 \
         ! mf{codec}enc bitrate={bitrate} low-latency=true ! {codec}parse name=encoded-video \
         ! rtp{codec}pay config-interval=-1 pt=96 \
         ! capsfilter caps=\"application/x-rtp,media=video,encoding-name={encoding},payload=96,clock-rate=90000\"", fps * 8), true,
    ).unwrap();
    let encoded_bytes = Arc::new(AtomicU64::new(0));
    let bytes_for_probe = encoded_bytes.clone();
    video
        .by_name("encoded-video")
        .unwrap()
        .static_pad("src")
        .unwrap()
        .add_probe(gst::PadProbeType::BUFFER, move |_, info| {
            if let Some(gst::PadProbeData::Buffer(buffer)) = &info.data {
                bytes_for_probe.fetch_add(buffer.size() as u64, Ordering::Relaxed);
            }
            gst::PadProbeReturn::Ok
        });
    let audio = gst::parse::bin_from_description(
        &format!(
            "appsrc name=marker-audio is-live=true format=time \
         caps=audio/x-raw,format=F32LE,layout=interleaved,rate=48000,channels=2 \
         ! audioconvert ! opusenc frame-size=10 ! rtpopuspay pt=111 \
         ! identity sync=true ts-offset={}",
            delay_ms * 1_000_000,
        ),
        true,
    )
    .unwrap();
    let sender = gst::ElementFactory::make("webrtcbin")
        .property_from_str("bundle-policy", "max-bundle")
        .build()
        .unwrap();
    let receiver = gst::ElementFactory::make("webrtcbin")
        .property_from_str("bundle-policy", "max-bundle")
        .build()
        .unwrap();
    configure_receive_transport(&receiver, true).unwrap();
    send_pipeline
        .add_many([video.upcast_ref(), audio.upcast_ref(), &sender])
        .unwrap();
    receive_pipeline.add(&receiver).unwrap();
    let _video_pad = link_loopback_sender(&sender, &video.static_pad("src").unwrap()).unwrap();
    let _audio_pad = link_loopback_sender(&sender, &audio.static_pad("src").unwrap()).unwrap();
    // Leave five seconds for negotiation and RTCP before changing content.
    video
        .by_name("marker-video")
        .unwrap()
        .static_pad("src")
        .unwrap()
        .add_probe(gst::PadProbeType::BUFFER, |_, info| {
            if let Some(gst::PadProbeData::Buffer(buffer)) = &mut info.data {
                if buffer.pts().is_some_and(|pts| pts.mseconds() >= 5_000) {
                    buffer
                        .make_mut()
                        .map_writable()
                        .unwrap()
                        .as_mut_slice()
                        .fill(255);
                }
            }
            gst::PadProbeReturn::Ok
        });
    let (shown, presentation) = mpsc::sync_channel(1);
    let weak = receive_pipeline.downgrade();
    let playback = owner.handle();
    receiver.connect_pad_added(move |_, pad| {
        let Some(pipeline) = weak.upgrade() else {
            return;
        };
        match encoding_name(pad).as_deref() {
            Some("OPUS") => {
                build_audio_branch(&pipeline, pad, None, None, &playout, "webrtc-av-test").unwrap();
            }
            Some("H264" | "H265") => {
                build_receive_branch(
                    &pipeline,
                    pad,
                    ReceiveOutput::Window(playback.clone()),
                    None,
                    &playout,
                    "webrtc-av-test",
                )
                .unwrap();
                let sink = pipeline
                    .children()
                    .into_iter()
                    .find(|element| {
                        element
                            .factory()
                            .is_some_and(|factory| factory.name() == "d3d11videosink")
                    })
                    .unwrap();
                observe_white_marker(&sink, shown.clone(), epoch);
                playback.reveal();
            }
            _ => {}
        }
    });
    connect_signalling(&sender, &receiver);

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
    let capture_stats = Arc::new(Mutex::new((0usize, 0.0f32)));
    let stats = capture_stats.clone();
    let heard_once = AtomicBool::new(false);
    capture
        .by_name("heard")
        .unwrap()
        .connect("new-sample", false, move |values| {
            let now = epoch.elapsed();
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
                let mut stats = stats.lock().unwrap();
                stats.0 += 1;
                stats.1 = stats.1.max(peak);
            }
            // Silence carries roughly 0.00003 dither on this device.
            if peak > 0.001 && !heard_once.swap(true, Ordering::Relaxed) {
                let _ = heard.try_send(now);
            }
            Some(gst::FlowReturn::Ok.to_value())
        });
    for pipeline in [&send_pipeline, &receive_pipeline] {
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
    }
    capture.set_state(gst::State::Playing).unwrap();
    receive_pipeline.set_state(gst::State::Playing).unwrap();
    send_pipeline.set_state(gst::State::Playing).unwrap();
    let source = audio.by_name("marker-audio").unwrap();
    for packet in 0..800u64 {
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
            let sample = if (500..505).contains(&packet) {
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
    let heard = audible.recv_timeout(Duration::from_secs(10));
    let shown = presentation.recv_timeout(Duration::from_secs(1));
    eprintln!(
        "WebRTC delay={delay_ms}ms capture buffers/peak={:?}, audio={heard:?}, video={shown:?}",
        capture_stats.lock().unwrap()
    );
    for pipeline in [&capture, &send_pipeline, &receive_pipeline] {
        for message in pipeline.bus().unwrap().iter() {
            if let gst::MessageView::Error(error) = message.view() {
                panic!(
                    "pipeline {}: {} ({:?})",
                    pipeline.name(),
                    error.error(),
                    error.debug()
                );
            }
        }
    }
    let heard = heard.expect("the sound device never played the WebRTC audio marker");
    let shown = shown.expect("the decoded WebRTC white marker was never presented");
    let delta_ms = heard.as_millis() as i64 - shown.as_millis() as i64;
    let seconds = send_pipeline.current_running_time().unwrap().nseconds() as f64 / 1_000_000_000.0;
    let encoded_mbps = encoded_bytes.load(Ordering::Relaxed) as f64 * 8.0 / seconds / 1_000_000.0;
    eprintln!(
        "WebRTC {encoding} {width}x{height}@{fps}, requested {}Mbps, encoded {encoded_mbps:.1}Mbps, injected audio delay={delay_ms}ms, actual audio minus D3D video={delta_ms}ms", bitrate / 1000
    );
    if stress {
        assert!(
            encoded_mbps >= 20.0,
            "stress input did not produce substantial video traffic"
        );
    }
    delta_ms
}

#[test]
#[ignore = "requires Windows process audio loopback and D3D11 hardware; plays three quiet markers"]
fn webrtc_audio_is_audible_and_matches_the_presented_video_marker() {
    // sync=false allowed delayed audio to trail video by 500ms; enabling sync
    // without late-media recovery could discard the entire audible marker.
    gst::init().unwrap();
    for (delay_ms, stress) in [(0, false), (500, false), (500, true)] {
        let delta_ms = marker_separation(delay_ms, stress);
        assert!(
            delta_ms.abs() <= 100,
            "WebRTC audio/video separation was {delta_ms}ms with {delay_ms}ms injected audio delay"
        );
    }
}
