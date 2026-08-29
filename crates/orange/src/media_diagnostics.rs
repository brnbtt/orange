use gst::prelude::*;
use gstreamer as gst;
use gstreamer_webrtc as gst_webrtc;
use serde::Serialize;
use std::fs::OpenOptions;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{sync_channel, Receiver, SyncSender, TrySendError};
use std::sync::Arc;
use std::sync::OnceLock;
use std::time::{Duration, Instant};
use std::time::{SystemTime, UNIX_EPOCH};

static DIAGNOSTIC_SINK: OnceLock<Option<SyncSender<DiagnosticCommand>>> = OnceLock::new();
const MAX_DIAGNOSTIC_BYTES: u64 = 64 * 1024 * 1024;

enum DiagnosticCommand {
    Line(String),
    Flush(SyncSender<()>),
}

pub(crate) struct DiagnosticsHandle {
    active: Arc<AtomicBool>,
}

impl Drop for DiagnosticsHandle {
    fn drop(&mut self) {
        self.active.store(false, Ordering::Release);
    }
}

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

#[derive(Debug, Default, Serialize)]
pub(crate) struct WebRtcReport {
    inbound_video: Option<InboundVideoStats>,
    outbound_video: Option<OutboundVideoStats>,
}

#[derive(Debug, Default, Serialize)]
struct InboundVideoStats {
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
}

#[derive(Debug, Default, Serialize)]
struct OutboundVideoStats {
    packets_sent: u64,
    payload_bytes_sent: u64,
    nack_received: u64,
    pli_received: u64,
    fir_received: u64,
}

pub(crate) fn parse_webrtc_stats(stats: &gst::StructureRef) -> WebRtcReport {
    let mut report = WebRtcReport::default();
    for (_, value) in stats.iter() {
        let Ok(sample) = value.get::<gst::Structure>() else {
            continue;
        };
        if sample.get::<String>("kind").as_deref() != Ok("video") {
            continue;
        }
        let Ok(kind) = sample.get::<gst_webrtc::WebRTCStatsType>("type") else {
            continue;
        };
        match kind {
            gst_webrtc::WebRTCStatsType::InboundRtp => {
                let inbound = report
                    .inbound_video
                    .get_or_insert_with(InboundVideoStats::default);
                inbound.packets_received += get_u64(&sample, "packets-received");
                inbound.payload_bytes_received += get_u64(&sample, "bytes-received");
                inbound.packets_lost += get_i64(&sample, "packets-lost");
                inbound.packets_repaired += get_u64(&sample, "packets-repaired");
                inbound.packets_discarded += get_u64(&sample, "packets-discarded");
                inbound.packets_duplicated += get_u64(&sample, "packets-duplicated");
                inbound.jitter_ms = get_f64(&sample, "jitter") * 1_000.0;
                inbound.nack_sent += get_u64(&sample, "nack-count");
                inbound.pli_sent += get_u64(&sample, "pli-count");
                inbound.fir_sent += get_u64(&sample, "fir-count");
                if let Ok(jitter) = sample.get::<gst::Structure>("gst-rtpjitterbuffer-stats") {
                    inbound.rtx_requested += get_u64(&jitter, "rtx-count");
                    inbound.rtx_succeeded += get_u64(&jitter, "rtx-success-count");
                    inbound.packets_late += get_u64(&jitter, "num-late");
                }
            }
            gst_webrtc::WebRTCStatsType::OutboundRtp => {
                let outbound = report
                    .outbound_video
                    .get_or_insert_with(OutboundVideoStats::default);
                outbound.packets_sent += get_u64(&sample, "packets-sent");
                outbound.payload_bytes_sent += get_u64(&sample, "bytes-sent");
                outbound.nack_received += get_u64(&sample, "nack-count");
                outbound.pli_received += get_u64(&sample, "pli-count");
                outbound.fir_received += get_u64(&sample, "fir-count");
            }
            _ => {}
        }
    }
    report
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

fn diagnostic_file_path(directory: &Path, pid: u32) -> PathBuf {
    directory.join(format!("orange-media-{pid}.jsonl"))
}

fn drain_diagnostics(
    receiver: Receiver<DiagnosticCommand>,
    writer: &mut impl Write,
    max_bytes: u64,
) -> std::io::Result<()> {
    let mut written = 0u64;
    let mut capped = false;
    while let Ok(command) = receiver.recv() {
        match command {
            DiagnosticCommand::Line(line) if !capped => {
                let line_size = line.len() as u64 + 1;
                if written.saturating_add(line_size) > max_bytes {
                    capped = true;
                    continue;
                }
                writeln!(writer, "{line}")?;
                writer.flush()?;
                written += line_size;
            }
            DiagnosticCommand::Line(_) => {}
            DiagnosticCommand::Flush(acknowledge) => {
                writer.flush()?;
                let _ = acknowledge.try_send(());
            }
        }
    }
    Ok(())
}

fn diagnostic_sink() -> Option<&'static SyncSender<DiagnosticCommand>> {
    DIAGNOSTIC_SINK
        .get_or_init(|| {
            let destination = std::env::var("ORANGE_MEDIA_DIAGNOSTICS").ok()?;
            let (sender, receiver) = sync_channel::<DiagnosticCommand>(128);
            let directory = PathBuf::from(destination);
            if let Err(error) = std::fs::create_dir_all(&directory) {
                eprintln!("[media-diagnostics] could not create log directory: {error}");
                return None;
            }
            let path = diagnostic_file_path(&directory, std::process::id());
            let file = match OpenOptions::new().create(true).append(true).open(&path) {
                Ok(file) => file,
                Err(error) => {
                    eprintln!(
                        "[media-diagnostics] could not open {}: {error}",
                        path.display()
                    );
                    return None;
                }
            };
            let remaining = MAX_DIAGNOSTIC_BYTES
                .saturating_sub(file.metadata().map(|metadata| metadata.len()).unwrap_or(0));
            std::thread::spawn(move || {
                let mut writer = BufWriter::new(file);
                if let Err(error) = drain_diagnostics(receiver, &mut writer, remaining) {
                    eprintln!(
                        "[media-diagnostics] could not write {}: {error}",
                        path.display()
                    );
                }
            });
            Some(sender)
        })
        .as_ref()
}

pub(crate) fn diagnostics_enabled() -> bool {
    diagnostic_sink().is_some()
}

pub(crate) fn emit_diagnostic(event: &str, role: &str, payload: impl Serialize) {
    let Some(sink) = diagnostic_sink() else {
        return;
    };
    let at_unix_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    let line = serde_json::json!({
        "at_unix_ms": at_unix_ms,
        "event": event,
        "role": role,
        "payload": payload,
    })
    .to_string();
    let _ = sink.try_send(DiagnosticCommand::Line(line));
}

pub(crate) fn flush_diagnostics() {
    let Some(sink) = diagnostic_sink() else {
        return;
    };
    let _ = enqueue_flush(sink, Duration::from_millis(500));
}

fn enqueue_flush(sink: &SyncSender<DiagnosticCommand>, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    let (acknowledge, acknowledged) = sync_channel(1);
    let mut command = DiagnosticCommand::Flush(acknowledge);
    loop {
        match sink.try_send(command) {
            Ok(()) => break,
            Err(TrySendError::Full(returned)) if Instant::now() < deadline => {
                command = returned;
                std::thread::yield_now();
            }
            Err(_) => return false,
        }
    }
    acknowledged
        .recv_timeout(deadline.saturating_duration_since(Instant::now()))
        .is_ok()
}

pub(crate) fn start_webrtc_diagnostics(
    bin: &gst::Element,
    label: String,
    progress: Option<Arc<MediaProgress>>,
    playback: Option<crate::window::PlaybackWindow>,
) -> Option<DiagnosticsHandle> {
    diagnostic_sink()?;
    let bin = bin.downgrade();
    let in_flight = Arc::new(AtomicBool::new(false));
    let active = Arc::new(AtomicBool::new(true));
    let active_for_worker = active.clone();
    std::thread::spawn(move || loop {
        std::thread::sleep(Duration::from_secs(2));
        if !active_for_worker.load(Ordering::Acquire) {
            break;
        }
        let Some(bin) = bin.upgrade() else { break };

        if let Some(progress) = &progress {
            let snapshot = progress.snapshot();
            let ui_responsive = playback
                .as_ref()
                .map(crate::window::PlaybackWindow::is_responsive);
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
                }
            }
            in_flight_for_reply.store(false, Ordering::Release);
        });
        bin.emit_by_name::<()>("get-stats", &[&None::<gst::Pad>, &promise]);
    });
    Some(DiagnosticsHandle { active })
}

#[cfg(test)]
mod tests {
    use super::*;
    use gstreamer as gst;
    use gstreamer_webrtc as gst_webrtc;
    use std::path::Path;

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

    #[test]
    fn diagnostic_files_are_scoped_to_one_process() {
        assert_eq!(
            diagnostic_file_path(Path::new(r"C:\logs"), 42),
            Path::new(r"C:\logs\orange-media-42.jsonl")
        );
    }

    #[test]
    fn dropping_diagnostics_stops_its_worker() {
        let active = Arc::new(AtomicBool::new(true));
        let handle = DiagnosticsHandle {
            active: active.clone(),
        };

        drop(handle);

        assert!(!active.load(Ordering::Acquire));
    }

    #[test]
    fn diagnostic_flush_acknowledges_after_queued_lines_are_written() {
        let (commands, receiver) = sync_channel(4);
        let (acknowledge, acknowledged) = sync_channel(1);
        commands
            .send(DiagnosticCommand::Line("first".into()))
            .unwrap();
        commands
            .send(DiagnosticCommand::Flush(acknowledge))
            .unwrap();
        drop(commands);
        let mut output = Vec::new();

        drain_diagnostics(receiver, &mut output, 1024).unwrap();

        assert!(acknowledged.try_recv().is_ok());
        assert_eq!(String::from_utf8(output).unwrap(), "first\n");
    }

    #[test]
    fn flush_waits_bounded_for_queue_capacity() {
        let (commands, receiver) = sync_channel(1);
        commands
            .send(DiagnosticCommand::Line("pending".into()))
            .unwrap();
        let commands_for_flush = commands.clone();
        let flushing =
            std::thread::spawn(move || enqueue_flush(&commands_for_flush, Duration::from_secs(1)));

        assert!(matches!(
            receiver.recv().unwrap(),
            DiagnosticCommand::Line(_)
        ));
        let DiagnosticCommand::Flush(acknowledge) = receiver.recv().unwrap() else {
            panic!("flush command was not queued");
        };
        acknowledge.send(()).unwrap();

        assert!(flushing.join().unwrap());
    }

    #[test]
    fn webrtc_stats_extract_video_recovery_without_sensitive_values() {
        gst::init().unwrap();
        let jitter = gst::Structure::builder("application/x-rtp-jitterbuffer-stats")
            .field("num-lost", 7u64)
            .field("num-late", 3u64)
            .field("rtx-count", 11u64)
            .field("rtx-success-count", 9u64)
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
        let stats = gst::Structure::builder("application/x-webrtc-stats")
            .field("inbound-sensitive-id", inbound)
            .field("outbound-sensitive-id", outbound)
            .build();

        let report = parse_webrtc_stats(&stats);
        let inbound = report.inbound_video.as_ref().unwrap();
        let outbound = report.outbound_video.as_ref().unwrap();
        assert_eq!(inbound.packets_received, 800);
        assert_eq!(inbound.packets_lost, 7);
        assert_eq!(inbound.nack_sent, 11);
        assert_eq!(inbound.rtx_requested, 11);
        assert_eq!(inbound.rtx_succeeded, 9);
        assert_eq!(outbound.packets_sent, 850);
        assert_eq!(outbound.nack_received, 11);

        let json = serde_json::to_string(&report).unwrap();
        assert!(!json.contains("inbound-sensitive-id"));
        assert!(!json.contains("203.0.113.10"));
    }
}
