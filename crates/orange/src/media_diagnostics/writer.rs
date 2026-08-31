use serde::Serialize;
use std::fs::OpenOptions;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{sync_channel, Receiver, SyncSender};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread::JoinHandle;
use std::time::Instant;
use std::time::{SystemTime, UNIX_EPOCH};

static DIAGNOSTIC_STATE: OnceLock<Arc<DiagnosticState>> = OnceLock::new();
const DIAGNOSTIC_QUEUE_CAPACITY: usize = 128;
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

pub(super) struct DiagnosticSink {
    context: DiagnosticContext,
    sender: Mutex<Option<SyncSender<String>>>,
}

struct DiagnosticState {
    runtime: Mutex<DiagnosticRuntime>,
}

enum DiagnosticRuntime {
    Pending {
        directory: PathBuf,
        metadata: DiagnosticMetadata,
    },
    Running {
        sink: Arc<DiagnosticSink>,
        worker: JoinHandle<()>,
    },
    Stopped,
}

pub(crate) struct DiagnosticWriter {
    state: Option<Arc<DiagnosticState>>,
    sink: Option<Arc<DiagnosticSink>>,
    worker: Option<JoinHandle<()>>,
}

impl DiagnosticWriter {
    pub(crate) fn new() -> Self {
        Self::new_with_destination(
            std::env::var("ORANGE_MEDIA_DIAGNOSTICS")
                .ok()
                .map(PathBuf::from),
        )
    }

    fn new_with_destination(destination: Option<PathBuf>) -> Self {
        Self::new_with_destination_in(destination, &DIAGNOSTIC_STATE)
    }

    fn new_with_destination_in(
        destination: Option<PathBuf>,
        process_state: &OnceLock<Arc<DiagnosticState>>,
    ) -> Self {
        let Some(directory) = destination else {
            return Self::disabled();
        };
        let state = Arc::new(DiagnosticState {
            runtime: Mutex::new(DiagnosticRuntime::Pending {
                directory,
                metadata: DiagnosticMetadata::from_lookup(|name| std::env::var(name).ok()),
            }),
        });
        if process_state.set(state.clone()).is_err() {
            return Self::disabled();
        }
        Self {
            state: Some(state),
            sink: None,
            worker: None,
        }
    }

    #[cfg(test)]
    fn start(writer: impl Write + Send + 'static, max_bytes: u64) -> Self {
        let (sink, worker) =
            Self::start_parts(writer, max_bytes, DiagnosticMetadata::default(), None);
        Self {
            state: None,
            sink: Some(sink),
            worker: Some(worker),
        }
    }

    fn start_parts(
        writer: impl Write + Send + 'static,
        max_bytes: u64,
        metadata: DiagnosticMetadata,
        path: Option<PathBuf>,
    ) -> (Arc<DiagnosticSink>, JoinHandle<()>) {
        let (sender, receiver) = sync_channel(DIAGNOSTIC_QUEUE_CAPACITY);
        let sink = Arc::new(DiagnosticSink {
            context: DiagnosticContext {
                started: Instant::now(),
                metadata,
            },
            sender: Mutex::new(Some(sender)),
        });
        let worker = std::thread::spawn(move || {
            let mut writer = writer;
            if let Err(error) = drain_diagnostics(receiver, &mut writer, max_bytes) {
                if let Some(path) = path {
                    eprintln!(
                        "[media-diagnostics] could not write {}: {error}",
                        path.display()
                    );
                } else {
                    eprintln!("[media-diagnostics] could not write diagnostics: {error}");
                }
            }
        });
        (sink, worker)
    }

    const fn disabled() -> Self {
        Self {
            state: None,
            sink: None,
            worker: None,
        }
    }

    fn shutdown(&mut self) {
        if let Some(state) = self.state.take() {
            state.shutdown();
        }
        if let Some(sink) = &self.sink {
            sink.sender
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .take();
        }
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
        self.sink.take();
    }
}

impl DiagnosticState {
    fn sink(&self) -> Option<Arc<DiagnosticSink>> {
        let mut runtime = self
            .runtime
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        match &*runtime {
            DiagnosticRuntime::Running { sink, .. } => return Some(sink.clone()),
            DiagnosticRuntime::Stopped => return None,
            DiagnosticRuntime::Pending { .. } => {}
        }
        let DiagnosticRuntime::Pending {
            directory,
            metadata,
        } = std::mem::replace(&mut *runtime, DiagnosticRuntime::Stopped)
        else {
            unreachable!();
        };
        let (sink, worker) = start_diagnostic_file(directory, metadata)?;
        *runtime = DiagnosticRuntime::Running {
            sink: sink.clone(),
            worker,
        };
        Some(sink)
    }

    fn shutdown(&self) {
        let running = {
            let mut runtime = self
                .runtime
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            std::mem::replace(&mut *runtime, DiagnosticRuntime::Stopped)
        };
        if let DiagnosticRuntime::Running { sink, worker } = running {
            sink.sender
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .take();
            let _ = worker.join();
        }
    }
}

fn start_diagnostic_file(
    directory: PathBuf,
    metadata: DiagnosticMetadata,
) -> Option<(Arc<DiagnosticSink>, JoinHandle<()>)> {
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
    Some(DiagnosticWriter::start_parts(
        BufWriter::new(file),
        remaining,
        metadata,
        Some(path),
    ))
}

impl Drop for DiagnosticWriter {
    fn drop(&mut self) {
        self.shutdown();
    }
}

impl DiagnosticSink {
    pub(super) fn emit(&self, event: &str, role: &str, payload: impl Serialize) -> bool {
        let sender = self
            .sender
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        let Some(sender) = sender else {
            return false;
        };
        let at_unix_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis();
        let elapsed_ms = self
            .context
            .started
            .elapsed()
            .as_millis()
            .min(u128::from(u64::MAX)) as u64;
        let line = match diagnostic_json(
            &self.context.metadata,
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
                return false;
            }
        };
        sender.try_send(line).is_ok()
    }
}

fn diagnostic_file_path(directory: &Path, pid: u32) -> PathBuf {
    directory.join(format!("orange-media-{pid}.jsonl"))
}

fn drain_diagnostics(
    receiver: Receiver<String>,
    writer: &mut impl Write,
    max_bytes: u64,
) -> std::io::Result<()> {
    let mut written = 0u64;
    let mut capped = false;
    while let Ok(line) = receiver.recv() {
        if capped {
            continue;
        }
        let line_size = line.len() as u64 + 1;
        if written.saturating_add(line_size) > max_bytes {
            capped = true;
            continue;
        }
        writeln!(writer, "{line}")?;
        writer.flush()?;
        written += line_size;
    }
    writer.flush()
}

fn diagnostic_sink_from(state: &OnceLock<Arc<DiagnosticState>>) -> Option<Arc<DiagnosticSink>> {
    state.get()?.sink()
}

pub(super) fn diagnostic_sink() -> Option<Arc<DiagnosticSink>> {
    diagnostic_sink_from(&DIAGNOSTIC_STATE)
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
    let _ = sink.emit(event, role, payload);
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Serializer;
    use std::cell::RefCell;
    use std::io;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex, TryLockError};
    use std::time::{Duration, Instant};

    #[derive(Clone)]
    struct ReleaseGuard(Arc<Mutex<Option<SyncSender<()>>>>);

    impl ReleaseGuard {
        fn new(sender: SyncSender<()>) -> Self {
            Self(Arc::new(Mutex::new(Some(sender))))
        }

        fn release(&self) {
            let sender = self
                .0
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .take();
            if let Some(sender) = sender {
                let _ = sender.send(());
            }
        }
    }

    impl Drop for ReleaseGuard {
        fn drop(&mut self) {
            self.release();
        }
    }

    struct WriterReleaseGuard {
        // Field order releases a blocked writer before its owner joins on unwind.
        _release: ReleaseGuard,
        writer: DiagnosticWriter,
    }

    impl WriterReleaseGuard {
        fn shutdown(mut self) {
            self.writer.shutdown();
        }
    }

    fn wait_for_sender_close(sink: &DiagnosticSink) -> bool {
        let deadline = Instant::now() + Duration::from_secs(1);
        loop {
            match sink.sender.try_lock() {
                Ok(sender) if sender.is_none() => return true,
                Err(TryLockError::Poisoned(poisoned)) => {
                    if poisoned.into_inner().is_none() {
                        return true;
                    }
                }
                Ok(_) | Err(TryLockError::WouldBlock) => {}
            }
            if Instant::now() >= deadline {
                return false;
            }
            std::thread::yield_now();
        }
    }

    fn wait_for_worker_finish(writer: &DiagnosticWriter) -> bool {
        let deadline = Instant::now() + Duration::from_secs(1);
        loop {
            if writer.worker.as_ref().is_none_or(JoinHandle::is_finished) {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            std::thread::yield_now();
        }
    }

    #[derive(Clone)]
    struct SharedWriter {
        output: Arc<Mutex<Vec<u8>>>,
        dropped: Arc<AtomicUsize>,
    }

    impl Write for SharedWriter {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.output.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl Drop for SharedWriter {
        fn drop(&mut self) {
            self.dropped.fetch_add(1, Ordering::Release);
        }
    }

    struct FlushCountingWriter(Arc<AtomicUsize>);

    impl Write for FlushCountingWriter {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            Ok(bytes.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            self.0.fetch_add(1, Ordering::Relaxed);
            Ok(())
        }
    }

    #[derive(Clone, Copy)]
    enum IoFailure {
        Write,
        Flush,
    }

    struct FailingWriter {
        failure: IoFailure,
        failed: SyncSender<()>,
        dropped: Arc<AtomicUsize>,
    }

    impl Write for FailingWriter {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            if matches!(self.failure, IoFailure::Write) {
                let _ = self.failed.try_send(());
                return Err(io::Error::other("write failed"));
            }
            Ok(bytes.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            if matches!(self.failure, IoFailure::Flush) {
                let _ = self.failed.try_send(());
                return Err(io::Error::other("flush failed"));
            }
            Ok(())
        }
    }

    impl Drop for FailingWriter {
        fn drop(&mut self) {
            self.dropped.fetch_add(1, Ordering::Release);
        }
    }

    struct BlockingWriter {
        output: Arc<Mutex<Vec<u8>>>,
        entered: SyncSender<()>,
        release: Receiver<()>,
        blocked: bool,
    }

    impl Write for BlockingWriter {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            if !self.blocked {
                self.blocked = true;
                self.entered.send(()).unwrap();
                self.release.recv().unwrap();
            }
            self.output.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    struct BlockingSerialize {
        entered: SyncSender<()>,
        release: Receiver<()>,
    }

    impl Serialize for BlockingSerialize {
        fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
        where
            S: Serializer,
        {
            self.entered.send(()).unwrap();
            self.release.recv().unwrap();
            serializer.serialize_str("accepted-in-flight")
        }
    }

    struct ProductionReentrantSerialize {
        sink: Arc<DiagnosticSink>,
    }

    impl Serialize for ProductionReentrantSerialize {
        fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
        where
            S: Serializer,
        {
            assert!(self.sink.emit(
                "nested-event",
                "nested-role",
                serde_json::json!({ "value": 42 }),
            ));
            serializer.serialize_str("outer-value")
        }
    }

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

    fn local_writer() -> (DiagnosticWriter, Arc<Mutex<Vec<u8>>>, Arc<AtomicUsize>) {
        let output = Arc::new(Mutex::new(Vec::new()));
        let dropped = Arc::new(AtomicUsize::new(0));
        let writer = DiagnosticWriter::start(
            SharedWriter {
                output: output.clone(),
                dropped: dropped.clone(),
            },
            1024 * 1024,
        );
        (writer, output, dropped)
    }

    fn assert_io_failure_disconnects_sink(failure: IoFailure) {
        let (failed, failure_observed) = sync_channel(1);
        let dropped = Arc::new(AtomicUsize::new(0));
        let writer = DiagnosticWriter::start(
            FailingWriter {
                failure,
                failed,
                dropped: dropped.clone(),
            },
            1024,
        );
        let sink = writer.sink.as_ref().unwrap().clone();
        assert!(sink.emit("accepted", "test", serde_json::json!({})));
        failure_observed
            .recv_timeout(Duration::from_secs(1))
            .unwrap();
        assert!(wait_for_worker_finish(&writer));
        assert!(!sink.emit("after-failure", "test", serde_json::json!({})));

        let (finished, shutdown_finished) = sync_channel(1);
        let shutdown = std::thread::spawn(move || {
            let mut writer = writer;
            writer.shutdown();
            writer.shutdown();
            let _ = finished.send(());
        });
        shutdown_finished
            .recv_timeout(Duration::from_secs(1))
            .unwrap();
        shutdown.join().unwrap();
        assert_eq!(dropped.load(Ordering::Acquire), 1);
    }

    #[test]
    fn diagnostic_writer_shutdown_drains_accepted_lines_and_joins_once() {
        let (mut writer, output, dropped) = local_writer();
        let sink = writer.sink.as_ref().unwrap().clone();
        assert!(sink.emit("first", "test", serde_json::json!({ "index": 1 })));
        assert!(sink.emit("second", "test", serde_json::json!({ "index": 2 })));

        writer.shutdown();

        assert_eq!(dropped.load(Ordering::Acquire), 1);
        let output = String::from_utf8(output.lock().unwrap().clone()).unwrap();
        let records = output
            .lines()
            .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
            .collect::<Vec<_>>();
        assert_eq!(records.len(), 2);
        assert_eq!(records[0]["event"], "first");
        assert_eq!(records[1]["event"], "second");

        writer.shutdown();
        assert_eq!(dropped.load(Ordering::Acquire), 1);
    }

    #[test]
    fn diagnostic_writer_shutdown_flushes_after_channel_disconnect() {
        let flushes = Arc::new(AtomicUsize::new(0));
        let mut writer = DiagnosticWriter::start(FlushCountingWriter(flushes.clone()), 1024);

        writer.shutdown();

        assert_eq!(flushes.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn diagnostic_writer_shutdown_drains_a_full_queue_without_a_command_slot() {
        let output = Arc::new(Mutex::new(Vec::new()));
        let (entered, blocked) = sync_channel(1);
        let (release, released) = sync_channel(1);
        let writer = DiagnosticWriter::start(
            BlockingWriter {
                output: output.clone(),
                entered,
                release: released,
                blocked: false,
            },
            1024 * 1024,
        );
        // Drop before `writer` on any assertion panic so its join cannot strand.
        let release = ReleaseGuard::new(release);
        let sink = writer.sink.as_ref().unwrap().clone();
        assert!(sink.emit("first", "test", serde_json::json!({})));
        blocked.recv_timeout(Duration::from_secs(1)).unwrap();
        for index in 0..128 {
            assert!(sink.emit("queued", "test", serde_json::json!({ "index": index })));
        }
        assert!(!sink.emit("dropped", "test", serde_json::json!({})));

        let shutdown_owner = WriterReleaseGuard {
            _release: release.clone(),
            writer,
        };
        let shutdown = std::thread::spawn(move || shutdown_owner.shutdown());
        assert!(wait_for_sender_close(&sink));
        assert!(!sink.emit("after-close", "test", serde_json::json!({})));
        release.release();
        shutdown.join().unwrap();

        let output = String::from_utf8(output.lock().unwrap().clone()).unwrap();
        assert_eq!(output.lines().count(), 129);
        assert!(!output.contains("\"event\":\"dropped\""));
        assert!(!output.contains("\"event\":\"after-close\""));
    }

    #[test]
    fn diagnostic_writer_shutdown_cuts_off_new_senders_but_drains_in_flight_sender() {
        let (writer, output, _) = local_writer();
        let sink = writer.sink.as_ref().unwrap().clone();
        let (entered, serializing) = sync_channel(1);
        let (release, released) = sync_channel(1);
        let release = ReleaseGuard::new(release);
        let sink_for_emit = sink.clone();
        let emitting = std::thread::spawn(move || {
            sink_for_emit.emit(
                "in-flight",
                "test",
                BlockingSerialize {
                    entered,
                    release: released,
                },
            )
        });
        serializing.recv_timeout(Duration::from_secs(1)).unwrap();

        let shutdown_owner = WriterReleaseGuard {
            _release: release.clone(),
            writer,
        };
        let shutdown = std::thread::spawn(move || shutdown_owner.shutdown());
        assert!(wait_for_sender_close(&sink));
        assert!(!sink.emit("too-late", "test", serde_json::json!({})));
        release.release();

        assert!(emitting.join().unwrap());
        shutdown.join().unwrap();
        let output = String::from_utf8(output.lock().unwrap().clone()).unwrap();
        assert!(output.contains("\"event\":\"in-flight\""));
        assert!(!output.contains("\"event\":\"too-late\""));
    }

    #[test]
    fn diagnostic_writer_production_sink_allows_nested_emission() {
        let (mut writer, output, _) = local_writer();
        let sink = writer.sink.as_ref().unwrap().clone();

        assert!(sink.emit(
            "outer-event",
            "outer-role",
            ProductionReentrantSerialize { sink: sink.clone() },
        ));
        writer.shutdown();

        let output = String::from_utf8(output.lock().unwrap().clone()).unwrap();
        let events = output
            .lines()
            .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap()["event"].clone())
            .collect::<Vec<_>>();
        assert_eq!(events, ["nested-event", "outer-event"]);
    }

    #[test]
    fn diagnostic_writer_recovers_a_poisoned_sender_lock() {
        let (mut writer, output, _) = local_writer();
        let sink = writer.sink.as_ref().unwrap().clone();
        let sink_for_poison = sink.clone();
        let _ = std::thread::spawn(move || {
            let _guard = sink_for_poison.sender.lock().unwrap();
            panic!("poison sender lock");
        })
        .join();

        assert!(sink.emit("after-poison", "test", serde_json::json!({})));
        writer.shutdown();

        let output = String::from_utf8(output.lock().unwrap().clone()).unwrap();
        assert!(output.contains("\"event\":\"after-poison\""));
    }

    #[test]
    fn diagnostic_writer_without_destination_is_disabled_and_shutdown_is_idempotent() {
        assert!(DIAGNOSTIC_STATE.get().is_none());
        let mut writer = DiagnosticWriter::new_with_destination(None);

        writer.shutdown();
        writer.shutdown();

        assert!(writer.sink.is_none());
        assert!(writer.worker.is_none());
        assert!(DIAGNOSTIC_STATE.get().is_none());
    }

    #[test]
    fn diagnostic_writer_starts_on_first_sink_access_and_joins_on_shutdown() {
        let root = tempfile::tempdir().unwrap();
        let destination = root.path().join("diagnostics");
        let path = diagnostic_file_path(&destination, std::process::id());
        let process_state = OnceLock::new();

        let mut writer =
            DiagnosticWriter::new_with_destination_in(Some(destination.clone()), &process_state);

        assert!(!destination.exists());
        assert!(!path.exists());
        assert!(writer.worker.is_none());
        assert!(matches!(
            *process_state.get().unwrap().runtime.lock().unwrap(),
            DiagnosticRuntime::Pending { .. }
        ));

        let sink = diagnostic_sink_from(&process_state).unwrap();
        assert!(path.exists());
        assert!(matches!(
            *process_state.get().unwrap().runtime.lock().unwrap(),
            DiagnosticRuntime::Running { .. }
        ));
        assert!(sink.emit("lazy-start", "test", serde_json::json!({})));

        writer.shutdown();

        assert!(writer.worker.is_none());
        assert!(matches!(
            *process_state.get().unwrap().runtime.lock().unwrap(),
            DiagnosticRuntime::Stopped
        ));
        assert!(diagnostic_sink_from(&process_state).is_none());
        assert!(std::fs::read_to_string(path)
            .unwrap()
            .contains("\"event\":\"lazy-start\""));
    }

    #[test]
    fn diagnostic_writer_write_failure_disconnects_and_shuts_down() {
        assert_io_failure_disconnects_sink(IoFailure::Write);
    }

    #[test]
    fn diagnostic_writer_flush_failure_disconnects_and_shuts_down() {
        assert_io_failure_disconnects_sink(IoFailure::Flush);
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
    fn diagnostic_writer_stops_writing_at_the_byte_cap() {
        let (sender, receiver) = sync_channel(2);
        sender.send("1234".to_string()).unwrap();
        sender.send("x".to_string()).unwrap();
        drop(sender);
        let mut output = Vec::new();

        drain_diagnostics(receiver, &mut output, 6).unwrap();

        assert_eq!(String::from_utf8(output).unwrap(), "1234\n");
    }
}
