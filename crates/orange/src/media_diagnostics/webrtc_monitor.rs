use gst::prelude::*;
use gstreamer as gst;
use gstreamer_webrtc as gst_webrtc;
use serde::Serialize;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{sync_channel, SyncSender};
use std::sync::Arc;
use std::time::Duration;

use crate::network_diagnostics::selected_ice_route;

use super::progress::MediaProgress;
use super::writer::{diagnostic_sink, emit_diagnostic};

pub(crate) struct DiagnosticsHandle {
    active: Arc<AtomicBool>,
    stop: SyncSender<()>,
    worker: Option<std::thread::JoinHandle<()>>,
}

impl Drop for DiagnosticsHandle {
    fn drop(&mut self) {
        self.active.store(false, Ordering::Release);
        let _ = self.stop.try_send(());
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

#[derive(Debug, Default, Serialize)]
struct WebRtcReport {
    inbound_video: Option<InboundRtpStats>,
    outbound_video: Option<OutboundRtpStats>,
    inbound_audio: Option<InboundRtpStats>,
    outbound_audio: Option<OutboundRtpStats>,
    /// RTP streams whose media type could not be established.
    ///
    /// `inbound_audio` was null in every sample of a session whose audio was
    /// demonstrably flowing, which made an audio transport fault undiagnosable
    /// from these logs. Rather than assume why, count the streams that fall
    /// through so the next log says whether classification is still the
    /// problem.
    #[serde(skip_serializing_if = "is_zero")]
    unclassified_streams: u64,
}

fn is_zero(count: &u64) -> bool {
    *count == 0
}

#[derive(Debug, Default, Serialize)]
struct InboundRtpStats {
    packets_received: u64,
    payload_bytes_received: u64,
    packets_lost: i64,
    packets_repaired: u64,
    packets_discarded: u64,
    packets_duplicated: u64,
    jitter_ms: f64,
    nack_sent: u64,
    pli_sent: u64,
    fir_sent: u64,
    rtx_requested: u64,
    rtx_succeeded: u64,
    packets_late: u64,
    jitterbuffer_packets_pushed: u64,
    jitterbuffer_packets_lost: u64,
    jitterbuffer_packets_duplicated: u64,
    jitterbuffer_avg_jitter_ms: f64,
    jitterbuffer_rtx_per_packet: f64,
    jitterbuffer_rtx_rtt_ms: f64,
}

#[derive(Debug, Default, Serialize)]
struct OutboundRtpStats {
    packets_sent: u64,
    payload_bytes_sent: u64,
    nack_received: u64,
    pli_received: u64,
    fir_received: u64,
}

fn parse_webrtc_stats(stats: &gst::StructureRef) -> WebRtcReport {
    let mut report = WebRtcReport::default();
    for (_, value) in stats.iter() {
        let Ok(sample) = value.get::<gst::Structure>() else {
            continue;
        };
        let Ok(kind) = sample.get::<gst_webrtc::WebRTCStatsType>("type") else {
            continue;
        };
        if !matches!(
            kind,
            gst_webrtc::WebRTCStatsType::InboundRtp | gst_webrtc::WebRTCStatsType::OutboundRtp
        ) {
            continue;
        }
        let Some(media) = media_kind(&sample, stats) else {
            report.unclassified_streams += 1;
            continue;
        };
        match kind {
            gst_webrtc::WebRTCStatsType::InboundRtp => {
                let inbound = match media.as_str() {
                    "video" => &mut report.inbound_video,
                    "audio" => &mut report.inbound_audio,
                    _ => {
                        report.unclassified_streams += 1;
                        continue;
                    }
                }
                .get_or_insert_with(InboundRtpStats::default);
                accumulate_inbound(inbound, &sample);
            }
            gst_webrtc::WebRTCStatsType::OutboundRtp => {
                let outbound = match media.as_str() {
                    "video" => &mut report.outbound_video,
                    "audio" => &mut report.outbound_audio,
                    _ => {
                        report.unclassified_streams += 1;
                        continue;
                    }
                }
                .get_or_insert_with(OutboundRtpStats::default);
                accumulate_outbound(outbound, &sample);
            }
            _ => {}
        }
    }
    report
}

/// Establish whether an RTP stream stat describes audio or video.
///
/// `kind` is what webrtcbin is supposed to carry, and it is what video arrives
/// with. Audio reached the viewer perfectly well in a session where every
/// sample still reported no inbound audio, so `kind` cannot be the only route:
/// fall back to the codec the stream references, whose mime type names the
/// media directly.
fn media_kind(sample: &gst::StructureRef, stats: &gst::StructureRef) -> Option<String> {
    if let Ok(kind) = sample.get::<String>("kind") {
        return Some(kind);
    }
    let codec_id = sample.get::<String>("codec-id").ok()?;
    let codec = stats.get::<gst::Structure>(&codec_id).ok()?;
    let mime = codec.get::<String>("mime-type").ok()?;
    Some(mime.split('/').next()?.to_ascii_lowercase())
}

fn accumulate_inbound(inbound: &mut InboundRtpStats, sample: &gst::StructureRef) {
    inbound.packets_received += get_u64(sample, "packets-received");
    inbound.payload_bytes_received += get_u64(sample, "bytes-received");
    inbound.packets_lost += get_i64(sample, "packets-lost");
    inbound.packets_repaired += get_u64(sample, "packets-repaired");
    inbound.packets_discarded += get_u64(sample, "packets-discarded");
    inbound.packets_duplicated += get_u64(sample, "packets-duplicated");
    inbound.jitter_ms = get_f64(sample, "jitter") * 1_000.0;
    inbound.nack_sent += get_u64(sample, "nack-count");
    inbound.pli_sent += get_u64(sample, "pli-count");
    inbound.fir_sent += get_u64(sample, "fir-count");
    if let Ok(jitter) = sample.get::<gst::Structure>("gst-rtpjitterbuffer-stats") {
        inbound.jitterbuffer_packets_pushed += get_u64(&jitter, "num-pushed");
        inbound.jitterbuffer_packets_lost += get_u64(&jitter, "num-lost");
        inbound.jitterbuffer_packets_duplicated += get_u64(&jitter, "num-duplicates");
        inbound.jitterbuffer_avg_jitter_ms = get_u64(&jitter, "avg-jitter") as f64 / 1_000_000.0;
        inbound.rtx_requested += get_u64(&jitter, "rtx-count");
        inbound.rtx_succeeded += get_u64(&jitter, "rtx-success-count");
        inbound.jitterbuffer_rtx_per_packet = get_f64(&jitter, "rtx-per-packet");
        inbound.jitterbuffer_rtx_rtt_ms = get_u64(&jitter, "rtx-rtt") as f64 / 1_000_000.0;
        inbound.packets_late += get_u64(&jitter, "num-late");
    }
}

fn accumulate_outbound(outbound: &mut OutboundRtpStats, sample: &gst::StructureRef) {
    outbound.packets_sent += get_u64(sample, "packets-sent");
    outbound.payload_bytes_sent += get_u64(sample, "bytes-sent");
    outbound.nack_received += get_u64(sample, "nack-count");
    outbound.pli_received += get_u64(sample, "pli-count");
    outbound.fir_received += get_u64(sample, "fir-count");
}

fn get_u64(stats: &gst::StructureRef, field: &str) -> u64 {
    stats
        .get::<u64>(field)
        .or_else(|_| stats.get::<u32>(field).map(u64::from))
        .unwrap_or(0)
}

fn get_i64(stats: &gst::StructureRef, field: &str) -> i64 {
    stats
        .get::<i64>(field)
        .or_else(|_| stats.get::<i32>(field).map(i64::from))
        .unwrap_or(0)
}

fn get_f64(stats: &gst::StructureRef, field: &str) -> f64 {
    stats.get::<f64>(field).unwrap_or(0.0)
}

pub(crate) fn start_webrtc_diagnostics(
    bin: &gst::Element,
    label: String,
    progress: Option<Arc<MediaProgress>>,
    playback: Option<crate::window::PlaybackWindowHandle>,
) -> Option<DiagnosticsHandle> {
    diagnostic_sink()?;
    let bin = bin.downgrade();
    let in_flight = Arc::new(AtomicBool::new(false));
    let active = Arc::new(AtomicBool::new(true));
    let active_for_worker = active.clone();
    let (stop, stopped) = sync_channel(1);
    let worker = std::thread::spawn(move || {
        while let Err(std::sync::mpsc::RecvTimeoutError::Timeout) =
            stopped.recv_timeout(Duration::from_secs(2))
        {
            if !active_for_worker.load(Ordering::Acquire) {
                break;
            }
            let Some(bin) = bin.upgrade() else { break };

            if let Some(progress) = &progress {
                let snapshot = progress.snapshot();
                let ui_responsive = playback
                    .as_ref()
                    .map(crate::window::PlaybackWindowHandle::is_responsive);
                emit_diagnostic(
                    "media-progress",
                    &label,
                    serde_json::json!({
                        "ui_responsive": ui_responsive,
                        "progress": snapshot,
                    }),
                );
            }

            if in_flight.swap(true, Ordering::AcqRel) {
                continue;
            }
            let in_flight_for_reply = in_flight.clone();
            let active_for_reply = active_for_worker.clone();
            let label_for_reply = label.clone();
            let promise = gst::Promise::with_change_func(move |reply| {
                if active_for_reply.load(Ordering::Acquire) {
                    if let Ok(Some(stats)) = reply {
                        let report = parse_webrtc_stats(stats);
                        emit_diagnostic("webrtc-stats", &label_for_reply, report);
                        emit_diagnostic("ice-route", &label_for_reply, selected_ice_route(stats));
                    }
                }
                in_flight_for_reply.store(false, Ordering::Release);
            });
            bin.emit_by_name::<()>("get-stats", &[&None::<gst::Pad>, &promise]);
        }
    });
    Some(DiagnosticsHandle {
        active,
        stop,
        worker: Some(worker),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dropping_diagnostics_stops_its_worker() {
        let active = Arc::new(AtomicBool::new(true));
        let stop_observed = Arc::new(AtomicBool::new(false));
        let worker_completed = Arc::new(AtomicBool::new(false));
        let (stop, stopped) = sync_channel(1);
        let stop_observed_for_worker = stop_observed.clone();
        let worker_completed_for_worker = worker_completed.clone();
        let worker = std::thread::spawn(move || {
            if stopped.recv_timeout(Duration::from_secs(5)).is_ok() {
                stop_observed_for_worker.store(true, Ordering::Release);
            }
            worker_completed_for_worker.store(true, Ordering::Release);
        });
        let handle = DiagnosticsHandle {
            active: active.clone(),
            stop,
            worker: Some(worker),
        };

        drop(handle);

        assert!(!active.load(Ordering::Acquire));
        assert!(stop_observed.load(Ordering::Acquire));
        assert!(worker_completed.load(Ordering::Acquire));
    }

    #[test]
    fn webrtc_stats_extract_video_recovery_without_sensitive_values() {
        gst::init().unwrap();
        let jitter = gst::Structure::builder("application/x-rtp-jitterbuffer-stats")
            .field("num-pushed", 790u64)
            .field("num-lost", 7u64)
            .field("num-late", 3u64)
            .field("num-duplicates", 2u64)
            .field("avg-jitter", 2_500_000u64)
            .field("rtx-count", 11u64)
            .field("rtx-success-count", 9u64)
            .field("rtx-per-packet", 1.25f64)
            .field("rtx-rtt", 12_000_000u64)
            .build();
        let inbound = gst::Structure::builder("inbound-video")
            .field("type", gst_webrtc::WebRTCStatsType::InboundRtp)
            .field("id", "inbound-sensitive-id")
            .field("kind", "video")
            .field("packets-received", 800u64)
            .field("bytes-received", 900_000u64)
            .field("packets-lost", 7i64)
            .field("packets-repaired", 9u64)
            .field("nack-count", 11u32)
            .field("pli-count", 2u32)
            .field("fir-count", 1u32)
            .field("jitter", 0.012f64)
            .field("gst-rtpjitterbuffer-stats", jitter)
            .field("address", "203.0.113.10")
            .build();
        let outbound = gst::Structure::builder("outbound-video")
            .field("type", gst_webrtc::WebRTCStatsType::OutboundRtp)
            .field("kind", "video")
            .field("packets-sent", 850u64)
            .field("bytes-sent", 950_000u64)
            .field("nack-count", 11u32)
            .field("pli-count", 2u32)
            .field("fir-count", 1u32)
            .build();
        let audio_jitter = gst::Structure::builder("application/x-rtp-jitterbuffer-stats")
            .field("num-pushed", 990u64)
            .field("num-lost", 4u64)
            .field("num-late", 6u64)
            .field("avg-jitter", 4_000_000u64)
            .build();
        let inbound_audio = gst::Structure::builder("inbound-audio")
            .field("type", gst_webrtc::WebRTCStatsType::InboundRtp)
            .field("kind", "audio")
            .field("packets-received", 1_000u64)
            .field("bytes-received", 128_000u64)
            .field("packets-lost", 4i64)
            .field("jitter", 0.018f64)
            .field("gst-rtpjitterbuffer-stats", audio_jitter)
            .build();
        let stats = gst::Structure::builder("application/x-webrtc-stats")
            .field("inbound-sensitive-id", inbound)
            .field("outbound-sensitive-id", outbound)
            .field("inbound-audio-sensitive-id", inbound_audio)
            .build();

        let report = parse_webrtc_stats(&stats);
        let inbound = report.inbound_video.as_ref().unwrap();
        let outbound = report.outbound_video.as_ref().unwrap();
        assert_eq!(inbound.packets_received, 800);
        assert_eq!(inbound.packets_lost, 7);
        assert_eq!(inbound.nack_sent, 11);
        assert_eq!(inbound.rtx_requested, 11);
        assert_eq!(inbound.rtx_succeeded, 9);
        assert_eq!(inbound.jitterbuffer_packets_pushed, 790);
        assert_eq!(inbound.jitterbuffer_packets_lost, 7);
        assert_eq!(inbound.jitterbuffer_packets_duplicated, 2);
        assert_eq!(inbound.jitterbuffer_avg_jitter_ms, 2.5);
        assert_eq!(inbound.jitterbuffer_rtx_per_packet, 1.25);
        assert_eq!(inbound.jitterbuffer_rtx_rtt_ms, 12.0);
        assert_eq!(outbound.packets_sent, 850);
        assert_eq!(outbound.nack_received, 11);
        let audio = report.inbound_audio.as_ref().unwrap();
        assert_eq!(audio.packets_received, 1_000);
        assert_eq!(audio.packets_lost, 4);
        assert_eq!(audio.packets_late, 6);
        assert_eq!(audio.jitter_ms, 18.0);
        assert_eq!(audio.jitterbuffer_packets_pushed, 990);
        assert_eq!(audio.jitterbuffer_packets_lost, 4);

        let json = serde_json::to_string(&report).unwrap();
        assert!(!json.contains("inbound-sensitive-id"));
        assert!(!json.contains("203.0.113.10"));
        // Everything classified, so the escape hatch stays out of the payload.
        assert_eq!(report.unclassified_streams, 0);
        assert!(!json.contains("unclassified_streams"));
    }

    #[test]
    fn a_stream_without_a_kind_is_classified_by_its_codec() {
        // Audio was flowing perfectly in a session that reported no inbound
        // audio for its whole 29 minutes, so `kind` is not always present.
        gst::init().unwrap();
        let codec = gst::Structure::builder("codec-for-opus")
            .field("type", gst_webrtc::WebRTCStatsType::Codec)
            .field("mime-type", "audio/OPUS")
            .field("payload-type", 111u32)
            .build();
        let inbound = gst::Structure::builder("inbound-audio")
            .field("type", gst_webrtc::WebRTCStatsType::InboundRtp)
            .field("codec-id", "codec-for-opus")
            .field("packets-received", 1_000u64)
            .build();
        let stats = gst::Structure::builder("application/x-webrtc-stats")
            .field("codec-for-opus", codec)
            .field("inbound-audio-id", inbound)
            .build();

        let report = parse_webrtc_stats(&stats);

        assert_eq!(
            report.inbound_audio.as_ref().unwrap().packets_received,
            1_000
        );
        assert!(report.inbound_video.is_none());
        assert_eq!(report.unclassified_streams, 0);
    }

    #[test]
    fn a_stream_that_cannot_be_classified_is_counted_rather_than_dropped() {
        // Silently skipping is what made the null inbound audio impossible to
        // explain from a log. If the fallback misses too, the count says so.
        gst::init().unwrap();
        let inbound = gst::Structure::builder("inbound-mystery")
            .field("type", gst_webrtc::WebRTCStatsType::InboundRtp)
            .field("packets-received", 500u64)
            .build();
        let stats = gst::Structure::builder("application/x-webrtc-stats")
            .field("inbound-mystery-id", inbound)
            .build();

        let report = parse_webrtc_stats(&stats);

        assert!(report.inbound_audio.is_none());
        assert!(report.inbound_video.is_none());
        assert_eq!(report.unclassified_streams, 1);
        assert!(serde_json::to_string(&report)
            .unwrap()
            .contains("unclassified_streams"));
    }
}
