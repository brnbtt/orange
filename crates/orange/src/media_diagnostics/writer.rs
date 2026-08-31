use serde::Serialize;
use std::fs::OpenOptions;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{sync_channel, Receiver, SyncSender, TrySendError};
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

pub(super) struct DiagnosticContext {
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

pub(super) enum DiagnosticCommand {
    Line(String),
    Flush(SyncSender<()>),
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

pub(super) fn diagnostic_sink() -> Option<&'static SyncSender<DiagnosticCommand>> {
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

pub(super) fn emit_diagnostic_to(
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Serializer;
    use std::cell::RefCell;

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
    fn diagnostic_files_are_scoped_to_one_process() {
        assert_eq!(
            diagnostic_file_path(Path::new(r"C:\logs"), 42),
            Path::new(r"C:\logs\orange-media-42.jsonl")
        );
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
}
