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
static DIAGNOSTIC_CONTEXT: OnceLock<DiagnosticContext> = OnceLock::new();
const MAX_DIAGNOSTIC_BYTES: u64 = 64 * 1024 * 1024;

#[cfg(test)]
thread_local! {
    static TEST_DIAGNOSTIC_SINK: std::cell::RefCell<Option<Vec<serde_json::Value>>> =
        const { std::cell::RefCell::new(None) };
}

#[derive(Default)]
struct DiagnosticMetadata {
    build: Option<String>,
    run: Option<String>,
    device: Option<String>,
    profile: Option<String>,
}

impl DiagnosticMetadata {
    fn from_lookup(mut lookup: impl FnMut(&str) -> Option<String>) -> Self {
        Self {
            build: lookup("ORANGE_BUILD_ID"),
            run: lookup("ORANGE_RUN_ID"),
            device: lookup("ORANGE_DEVICE_ID"),
            profile: lookup("ORANGE_TEST_PROFILE"),
        }
    }
}

struct DiagnosticContext {
    started: Instant,
    metadata: DiagnosticMetadata,
}

#[derive(Serialize)]
struct DiagnosticRecord<'a, T> {
    at_unix_ms: u128,
    elapsed_ms: u64,
    event: &'a str,
    role: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    build: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    run: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    device: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    profile: Option<&'a str>,
    payload: T,
}

fn diagnostic_json(
    metadata: &DiagnosticMetadata,
    elapsed_ms: u64,
    at_unix_ms: u128,
    event: &str,
    role: &str,
    payload: impl Serialize,
) -> Result<String, serde_json::Error> {
    serde_json::to_string(&DiagnosticRecord {
        at_unix_ms,
        elapsed_ms,
        event,
        role,
        build: metadata.build.as_deref(),
        run: metadata.run.as_deref(),
        device: metadata.device.as_deref(),
        profile: metadata.profile.as_deref(),
        payload,
    })
}

pub(crate) trait OperationOutcome {
    fn succeeded(&self) -> bool;
}

impl<T, E> OperationOutcome for Result<T, E> {
    fn succeeded(&self) -> bool {
        self.is_ok()
    }
}

impl OperationOutcome for () {
    fn succeeded(&self) -> bool {
        true
    }
}

#[derive(Clone, Copy, Serialize)]
pub(crate) struct Operation<'a> {
    operation: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    element: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    factory: Option<&'a str>,
}

impl<'a> Operation<'a> {
    pub(crate) const fn named(operation: &'a str) -> Self {
        Self {
            operation,
            element: None,
            factory: None,
        }
    }

    pub(crate) const fn element(operation: &'a str, element: &'a str, factory: &'a str) -> Self {
        Self {
            operation,
            element: Some(element),
            factory: Some(factory),
        }
    }
}

#[derive(Serialize)]
struct OperationPayload<'a> {
    #[serde(flatten)]
    operation: Operation<'a>,
    #[serde(skip_serializing_if = "Option::is_none")]
    duration_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    success: Option<bool>,
}

fn measure_operation_with<R: OperationOutcome>(
    operation: Operation<'_>,
    action: impl FnOnce() -> R,
    mut emit: impl FnMut(&str, OperationPayload<'_>),
) -> R {
    emit(
        "operation-started",
        OperationPayload {
            operation,
            duration_ms: None,
            success: None,
        },
    );
    let started = Instant::now();
    let result = action();
    let duration_ms = started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64;
    let success = result.succeeded();
    emit(
        "operation-finished",
        OperationPayload {
            operation,
            duration_ms: Some(duration_ms),
            success: Some(success),
        },
    );
    result
}

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
    inbound_video: Option<InboundRtpStats>,
    outbound_video: Option<OutboundRtpStats>,
    inbound_audio: Option<InboundRtpStats>,
    outbound_audio: Option<OutboundRtpStats>,
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

pub(crate) fn parse_webrtc_stats(stats: &gst::StructureRef) -> WebRtcReport {
    let mut report = WebRtcReport::default();
    for (_, value) in stats.iter() {
        let Ok(sample) = value.get::<gst::Structure>() else {
            continue;
        };
        let Ok(media) = sample.get::<String>("kind") else {
            continue;
        };
        let Ok(kind) = sample.get::<gst_webrtc::WebRTCStatsType>("type") else {
            continue;
        };
        match kind {
            gst_webrtc::WebRTCStatsType::InboundRtp => {
                let inbound = match media.as_str() {
                    "video" => &mut report.inbound_video,
                    "audio" => &mut report.inbound_audio,
                    _ => continue,
                }
                .get_or_insert_with(InboundRtpStats::default);
                accumulate_inbound(inbound, &sample);
            }
            gst_webrtc::WebRTCStatsType::OutboundRtp => {
                let outbound = match media.as_str() {
                    "video" => &mut report.outbound_video,
                    "audio" => &mut report.outbound_audio,
                    _ => continue,
                }
                .get_or_insert_with(OutboundRtpStats::default);
                accumulate_outbound(outbound, &sample);
            }
            _ => {}
        }
    }
    report
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
            DIAGNOSTIC_CONTEXT.get_or_init(|| DiagnosticContext {
                started: Instant::now(),
                metadata: DiagnosticMetadata::from_lookup(|name| std::env::var(name).ok()),
            });
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
    #[cfg(test)]
    if emit_to_test_sink(event, role, &payload) {
        return;
    }
    let Some(sink) = diagnostic_sink() else {
        return;
    };
    emit_diagnostic_to(sink, event, role, payload);
}

#[cfg(test)]
fn emit_to_test_sink(event: &str, role: &str, payload: &impl Serialize) -> bool {
    if !TEST_DIAGNOSTIC_SINK.with(|sink| sink.borrow().is_some()) {
        return false;
    }
    let Ok(payload) = serde_json::to_value(payload) else {
        return true;
    };
    TEST_DIAGNOSTIC_SINK.with(|sink| {
        if let Some(records) = sink.borrow_mut().as_mut() {
            records.push(serde_json::json!({
                "event": event,
                "role": role,
                "payload": payload,
            }));
        }
    });
    true
}

#[cfg(test)]
pub(crate) fn capture_diagnostics(action: impl FnOnce()) -> Vec<serde_json::Value> {
    TEST_DIAGNOSTIC_SINK.with(|sink| {
        let mut sink = sink.borrow_mut();
        assert!(sink.is_none(), "diagnostic capture is already active");
        *sink = Some(Vec::new());
    });
    action();
    TEST_DIAGNOSTIC_SINK.with(|sink| {
        sink.borrow_mut()
            .take()
            .expect("diagnostic capture was active")
    })
}

fn emit_diagnostic_to(
    sink: &SyncSender<DiagnosticCommand>,
    event: &str,
    role: &str,
    payload: impl Serialize,
) {
    let Some(context) = DIAGNOSTIC_CONTEXT.get() else {
        let _ = writeln!(
            std::io::stderr().lock(),
            "[media-diagnostics] diagnostic context unavailable"
        );
        return;
    };
    let at_unix_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    let elapsed_ms = context
        .started
        .elapsed()
        .as_millis()
        .min(u128::from(u64::MAX)) as u64;
    let line = match diagnostic_json(
        &context.metadata,
        elapsed_ms,
        at_unix_ms,
        event,
        role,
        payload,
    ) {
        Ok(line) => line,
        Err(_) => {
            let _ = writeln!(
                std::io::stderr().lock(),
                "[media-diagnostics] could not serialize event {event:?} for role {role:?}"
            );
            return;
        }
    };
    let _ = sink.try_send(DiagnosticCommand::Line(line));
}

pub(crate) fn measure_operation<R: OperationOutcome>(
    role: &str,
    operation: Operation<'_>,
    action: impl FnOnce() -> R,
) -> R {
    let Some(sink) = diagnostic_sink() else {
        return action();
    };
    measure_operation_with(operation, action, |event, payload| {
        emit_diagnostic_to(sink, event, role, payload);
    })
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
    use serde::Serializer;
    use std::cell::RefCell;
    use std::path::Path;

    struct FailingSerialize;

    impl Serialize for FailingSerialize {
        fn serialize<S>(&self, _serializer: S) -> Result<S::Ok, S::Error>
        where
            S: Serializer,
        {
            Err(serde::ser::Error::custom("failure"))
        }
    }

    struct ReentrantSerialize;

    impl Serialize for ReentrantSerialize {
        fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
        where
            S: Serializer,
        {
            emit_diagnostic(
                "nested-event",
                "nested-role",
                serde_json::json!({ "value": 42 }),
            );
            serializer.serialize_str("outer-value")
        }
    }

    #[test]
    fn diagnostic_json_returns_serialization_error() {
        assert!(diagnostic_json(
            &DiagnosticMetadata::default(),
            17,
            1_725_000_000_000,
            "test-event",
            "watch",
            FailingSerialize,
        )
        .is_err());
    }

    #[test]
    fn test_sink_handles_serialization_error_without_capturing_a_record() {
        let records = capture_diagnostics(|| {
            assert!(emit_to_test_sink("test-event", "watch", &FailingSerialize));
        });

        assert!(records.is_empty());
    }

    #[test]
    fn test_sink_allows_diagnostics_during_payload_serialization() {
        let records = capture_diagnostics(|| {
            emit_diagnostic("outer-event", "outer-role", ReentrantSerialize);
        });

        assert_eq!(
            serde_json::Value::Array(records),
            serde_json::json!([
                {
                    "event": "nested-event",
                    "role": "nested-role",
                    "payload": { "value": 42 },
                },
                {
                    "event": "outer-event",
                    "role": "outer-role",
                    "payload": "outer-value",
                },
            ])
        );
    }

    #[test]
    fn diagnostic_metadata_is_omitted_when_allowlisted_environment_is_absent() {
        let metadata = DiagnosticMetadata::from_lookup(|_| None);

        let json = diagnostic_json(
            &metadata,
            17,
            1_725_000_000_000,
            "test-event",
            "watch",
            serde_json::json!({ "value": 42 }),
        )
        .unwrap();

        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&json).unwrap(),
            serde_json::json!({
                "at_unix_ms": 1_725_000_000_000u64,
                "elapsed_ms": 17,
                "event": "test-event",
                "role": "watch",
                "payload": { "value": 42 },
            })
        );
    }

    #[test]
    fn diagnostic_metadata_is_included_as_top_level_strings() {
        let metadata = DiagnosticMetadata::from_lookup(|name| match name {
            "ORANGE_BUILD_ID" => Some("0123456789abcdef".to_string()),
            "ORANGE_RUN_ID" => Some("run-123".to_string()),
            "ORANGE_DEVICE_ID" => Some("device-456".to_string()),
            "ORANGE_TEST_PROFILE" => Some("hardware-bounded-jitter".to_string()),
            _ => None,
        });

        let json = diagnostic_json(
            &metadata,
            29,
            1_725_000_000_001,
            "test-event",
            "watch",
            serde_json::json!({}),
        )
        .unwrap();

        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&json).unwrap(),
            serde_json::json!({
                "at_unix_ms": 1_725_000_000_001u64,
                "elapsed_ms": 29,
                "event": "test-event",
                "role": "watch",
                "build": "0123456789abcdef",
                "run": "run-123",
                "device": "device-456",
                "profile": "hardware-bounded-jitter",
                "payload": {},
            })
        );
    }

    #[test]
    fn diagnostic_metadata_reads_only_the_explicit_allowlist() {
        let requested = RefCell::new(Vec::new());

        let _metadata = DiagnosticMetadata::from_lookup(|name| {
            requested.borrow_mut().push(name.to_string());
            None
        });

        assert_eq!(
            requested.into_inner(),
            [
                "ORANGE_BUILD_ID",
                "ORANGE_RUN_ID",
                "ORANGE_DEVICE_ID",
                "ORANGE_TEST_PROFILE",
            ]
        );
    }

    #[test]
    fn measured_operation_preserves_success_value_and_reports_success() {
        let mut events = Vec::new();

        let result = measure_operation_with(
            Operation::named("incoming-video-pad-link"),
            || Ok::<_, &'static str>(String::from("unchanged")),
            |event, payload| {
                events.push((event.to_string(), serde_json::to_value(payload).unwrap()));
            },
        );

        assert_eq!(result, Ok(String::from("unchanged")));
        assert_eq!(events[0].0, "operation-started");
        assert_eq!(
            events[0].1,
            serde_json::json!({ "operation": "incoming-video-pad-link" })
        );
        assert_eq!(events[1].0, "operation-finished");
        assert_eq!(events[1].1["operation"], "incoming-video-pad-link");
        assert_eq!(events[1].1["success"], true);
        assert!(events[1].1["duration_ms"].is_u64());
    }

    #[test]
    fn measured_operation_preserves_error_and_reports_failure() {
        let mut events = Vec::new();

        let result = measure_operation_with(
            Operation::named("video-decoder-create-d3d11av1dec"),
            || Err::<String, _>("decoder unavailable"),
            |event, payload| {
                events.push((event.to_string(), serde_json::to_value(payload).unwrap()));
            },
        );

        assert_eq!(result, Err("decoder unavailable"));
        assert_eq!(events[0].0, "operation-started");
        assert_eq!(events[1].0, "operation-finished");
        assert_eq!(events[1].1["success"], false);
    }

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
    }
}
