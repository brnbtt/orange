//! Running bounded `orange troubleshoot` checks and formatting a safe report.

use crate::{background, supervisor};
use anyhow::Context as _;
use serde::Deserialize;
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

#[allow(clippy::unnecessary_sort_by)]
mod history;

const CHILD_TIMEOUT: Duration = Duration::from_secs(20);
const JOIN_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_OUTPUT_BYTES: u64 = 64 * 1024;
const MAX_DETAIL_CHARS: usize = 240;
const CHECKS_REQUIRED: usize = 7;

const CHILD_FAILURE_DETAIL: &str =
    "Media diagnostic process failed to start or finish; repair or reinstall orange, then run Troubleshoot again.";
const OUTPUT_INVALID_DETAIL: &str =
    "Media diagnostic output was invalid; update or reinstall orange, then run Troubleshoot again.";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum CheckStatus {
    Pass,
    Fail,
    Inconclusive,
}

impl CheckStatus {
    pub(super) fn label(self) -> &'static str {
        match self {
            Self::Pass => "pass",
            Self::Fail => "fail",
            Self::Inconclusive => "inconclusive",
        }
    }

    fn from_wire(value: &str) -> Option<Self> {
        match value {
            "pass" => Some(Self::Pass),
            "fail" => Some(Self::Fail),
            "inconclusive" => Some(Self::Inconclusive),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum CheckId {
    Runtime,
    Capture,
    Encoder,
    Decoder,
    Audio,
    Signalling,
    Stun,
}

impl CheckId {
    const ORDER: [Self; CHECKS_REQUIRED] = [
        Self::Runtime,
        Self::Capture,
        Self::Encoder,
        Self::Decoder,
        Self::Audio,
        Self::Signalling,
        Self::Stun,
    ];

    fn wire(self) -> &'static str {
        match self {
            Self::Runtime => "runtime",
            Self::Capture => "capture",
            Self::Encoder => "encoder",
            Self::Decoder => "decoder",
            Self::Audio => "audio",
            Self::Signalling => "signalling",
            Self::Stun => "stun",
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Runtime => "Runtime",
            Self::Capture => "Capture",
            Self::Encoder => "Encoder",
            Self::Decoder => "Decoder",
            Self::Audio => "Audio",
            Self::Signalling => "Signalling",
            Self::Stun => "STUN",
        }
    }

    fn from_wire(value: &str) -> Option<Self> {
        match value {
            "runtime" => Some(Self::Runtime),
            "capture" => Some(Self::Capture),
            "encoder" => Some(Self::Encoder),
            "decoder" => Some(Self::Decoder),
            "audio" => Some(Self::Audio),
            "signalling" => Some(Self::Signalling),
            "stun" => Some(Self::Stun),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct CheckResult {
    pub(super) label: &'static str,
    pub(super) id: &'static str,
    pub(super) status: CheckStatus,
    pub(super) detail: String,
}

impl CheckResult {
    fn failure(detail: &'static str) -> Vec<Self> {
        CheckId::ORDER
            .iter()
            .map(|id| Self {
                label: id.label(),
                id: id.wire(),
                status: CheckStatus::Inconclusive,
                detail: detail.to_string(),
            })
            .collect()
    }
}

#[derive(Debug, Clone)]
struct CompletedRun {
    checks: Vec<CheckResult>,
    history: Vec<String>,
    generated_at_unix_ms: u128,
}

impl CompletedRun {
    fn summary(&self) -> &'static str {
        if self
            .checks
            .iter()
            .all(|check| check.status == CheckStatus::Pass)
        {
            "Basic checks passed"
        } else {
            "Checks need attention"
        }
    }
}

struct TroubleshootJob {
    cancel: Arc<AtomicBool>,
    cancelled_at: Option<Instant>,
    receiver: mpsc::Receiver<CompletedRun>,
    worker: Option<JoinHandle<()>>,
}

impl TroubleshootJob {
    fn cancel(&mut self) {
        self.cancel.store(true, Ordering::Release);
        self.cancelled_at.get_or_insert_with(Instant::now);
    }

    fn is_finished(&self) -> bool {
        let finished = self.worker.as_ref().is_none_or(JoinHandle::is_finished);
        if !finished {
            background::check_cancel_deadline(
                self.cancelled_at,
                JOIN_TIMEOUT,
                "troubleshoot worker",
            );
        }
        finished
    }

    fn join(&mut self) {
        if let Some(worker) = self.worker.take() {
            background::join_background_worker(worker, JOIN_TIMEOUT, "troubleshoot worker");
        }
    }
}

impl Drop for TroubleshootJob {
    fn drop(&mut self) {
        self.cancel();
        self.join();
    }
}

#[derive(Default)]
pub(super) struct TroubleshootState {
    job: Option<TroubleshootJob>,
    completed: Option<CompletedRun>,
    generation: u64,
}

impl TroubleshootState {
    pub(super) fn start(&mut self, server: &str, diagnostics: Option<PathBuf>) -> bool {
        if self.job.is_some() {
            return false;
        }
        let mut command = match supervisor::orange_command() {
            Ok(command) => command,
            Err(_) => {
                self.completed = Some(failed_run(CHILD_FAILURE_DETAIL));
                self.generation = self.generation.wrapping_add(1);
                return false;
            }
        };
        command.arg("troubleshoot").args(["--server", server]);
        self.start_with_command(command, diagnostics)
    }

    fn start_with_command(&mut self, command: Command, diagnostics: Option<PathBuf>) -> bool {
        if self.job.is_some() {
            return false;
        }
        self.completed = None;
        let cancel = Arc::new(AtomicBool::new(false));
        let worker_cancel = Arc::clone(&cancel);
        let (sender, receiver) = mpsc::channel();
        let worker = std::thread::spawn(move || {
            let checks = run_checks(command, &worker_cancel).unwrap_or_else(CheckResult::failure);
            if worker_cancel.load(Ordering::Acquire) {
                return;
            }
            let history = diagnostics
                .as_deref()
                .map(|path| history::summarize(path, &worker_cancel))
                .unwrap_or_else(|| {
                    vec!["No recent local diagnostics were found for this install.".to_string()]
                });
            if worker_cancel.load(Ordering::Acquire) {
                return;
            }
            let _ = sender.send(CompletedRun {
                checks,
                history,
                generated_at_unix_ms: SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .map(|duration| duration.as_millis())
                    .unwrap_or_default(),
            });
        });
        self.job = Some(TroubleshootJob {
            cancel,
            cancelled_at: None,
            receiver,
            worker: Some(worker),
        });
        self.generation = self.generation.wrapping_add(1);
        true
    }

    pub(super) fn poll(&mut self) -> bool {
        let Some(job) = self.job.as_mut() else {
            return false;
        };
        if !job.is_finished() {
            return false;
        }
        job.join();
        let cancelled = job.cancel.load(Ordering::Acquire);
        let completed = if cancelled {
            None
        } else {
            job.receiver.try_recv().ok()
        };
        self.job = None;
        self.completed =
            completed.or_else(|| (!cancelled).then(|| failed_run(CHILD_FAILURE_DETAIL)));
        self.generation = self.generation.wrapping_add(1);
        true
    }

    pub(super) fn cancel(&mut self) {
        if let Some(job) = self.job.as_mut() {
            job.cancel();
        }
    }

    pub(super) fn clear(&mut self) {
        self.cancel();
        self.completed = None;
        self.generation = self.generation.wrapping_add(1);
    }

    pub(super) fn is_running(&self) -> bool {
        self.job.is_some()
    }

    pub(super) fn has_result(&self) -> bool {
        self.completed.is_some()
    }

    pub(super) fn summary(&self) -> Option<&'static str> {
        self.completed.as_ref().map(CompletedRun::summary)
    }

    pub(super) fn checks(&self) -> &[CheckResult] {
        self.completed
            .as_ref()
            .map(|run| run.checks.as_slice())
            .unwrap_or(&[])
    }

    pub(super) fn history(&self) -> &[String] {
        self.completed
            .as_ref()
            .map(|run| run.history.as_slice())
            .unwrap_or(&[])
    }

    pub(super) fn generation(&self) -> u64 {
        self.generation
    }

    pub(super) fn report_text(&self, version: &str, build: &str) -> Option<String> {
        let run = self.completed.as_ref()?;
        let mut lines = vec![
            "Orange troubleshoot report".to_string(),
            format!("Generated: {}", run.generated_at_unix_ms),
            format!("Version: {version}"),
            format!("Build: {build}"),
            String::new(),
            format!("Summary: {}", run.summary()),
            String::new(),
            "Current checks".to_string(),
        ];
        for check in &run.checks {
            lines.push(format!(
                "- {} ({}) [{}] {}",
                check.label,
                check.id,
                check.status.label(),
                check.detail
            ));
        }
        lines.push(String::new());
        lines.push("Recent diagnostics".to_string());
        if run.history.is_empty() {
            lines.push("- No recent local diagnostics were found for this install.".to_string());
        } else {
            lines.extend(run.history.iter().map(|line| format!("- {line}")));
        }
        lines.push(String::new());
        lines.push(
            "Limitations: these checks do not validate real capture content, physical playback output, or connection to your intended friend.".to_string(),
        );
        lines.push(
            "Orange currently has no TURN fallback; some networks cannot connect directly."
                .to_string(),
        );
        Some(lines.join("\n"))
    }
}

fn failed_run(detail: &'static str) -> CompletedRun {
    CompletedRun {
        checks: CheckResult::failure(detail),
        history: Vec::new(),
        generated_at_unix_ms: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_millis())
            .unwrap_or_default(),
    }
}

fn run_checks(command: Command, cancel: &AtomicBool) -> Result<Vec<CheckResult>, &'static str> {
    let output = supervisor::run_bounded_command(
        command,
        cancel,
        CHILD_TIMEOUT,
        MAX_OUTPUT_BYTES,
        "troubleshoot command",
    )
    .map_err(|_| CHILD_FAILURE_DETAIL)?;
    if !output.status.success() {
        return Err(CHILD_FAILURE_DETAIL);
    }
    parse_checks(&output.stdout).map_err(|_| OUTPUT_INVALID_DETAIL)
}

#[derive(Deserialize)]
struct WireReport {
    schema: u64,
    checks: Vec<WireCheck>,
}

#[derive(Deserialize)]
struct WireCheck {
    id: String,
    status: String,
    detail: String,
}

fn parse_checks(bytes: &[u8]) -> anyhow::Result<Vec<CheckResult>> {
    let text = std::str::from_utf8(bytes)?;
    let json_line = text
        .lines()
        .rev()
        .find(|line| line.trim_start().starts_with('{'))
        .context("no troubleshoot json object")?;
    let report: WireReport = serde_json::from_str(json_line)?;
    anyhow::ensure!(report.schema == 1, "unsupported troubleshoot schema");
    anyhow::ensure!(
        report.checks.len() == CHECKS_REQUIRED,
        "unexpected check count"
    );

    let mut by_id = std::collections::HashMap::new();
    for check in report.checks {
        let id = CheckId::from_wire(&check.id).context("unknown check id")?;
        let status = CheckStatus::from_wire(&check.status).context("unknown check status")?;
        anyhow::ensure!(!by_id.contains_key(&id), "duplicate check id");
        by_id.insert(
            id,
            CheckResult {
                label: id.label(),
                id: id.wire(),
                status,
                detail: sanitize_detail(&check.detail).context("invalid detail")?,
            },
        );
    }
    anyhow::ensure!(by_id.len() == CHECKS_REQUIRED, "missing check id");
    Ok(CheckId::ORDER
        .iter()
        .filter_map(|id| by_id.remove(id))
        .collect())
}

fn sanitize_detail(detail: &str) -> Option<String> {
    let mut sanitized = String::new();
    for ch in detail.chars() {
        if sanitized.chars().count() >= MAX_DETAIL_CHARS {
            break;
        }
        if ch.is_control() {
            if ch.is_whitespace() {
                sanitized.push(' ');
            }
            continue;
        }
        sanitized.push(ch);
    }
    let sanitized = sanitized.split_whitespace().collect::<Vec<_>>().join(" ");
    if sanitized.is_empty() {
        return None;
    }
    Some(sanitized)
}

#[cfg(test)]
mod tests {
    use super::*;

    const CHILD_MODE: &str = "ORANGE_TEST_TROUBLESHOOT_CHILD";

    fn fixture_command(mode: &str) -> Command {
        let mut command = Command::new(std::env::current_exe().unwrap());
        command.args([
            "--exact",
            "troubleshoot::tests::troubleshoot_child_fixture",
            "--nocapture",
        ]);
        command.env(CHILD_MODE, mode);
        command
    }

    fn valid_report_json() -> &'static str {
        r#"{"schema":1,"checks":[{"id":"runtime","status":"pass","detail":"Runtime available"},{"id":"capture","status":"pass","detail":"Capture API available"},{"id":"encoder","status":"pass","detail":"Automatic encoder selected"},{"id":"decoder","status":"pass","detail":"Decoder factory available"},{"id":"audio","status":"pass","detail":"Audio output sink available"},{"id":"signalling","status":"pass","detail":"Signalling endpoint reachable"},{"id":"stun","status":"pass","detail":"STUN UDP response received"}]}"#
    }

    #[test]
    fn troubleshoot_child_fixture() {
        let Ok(mode) = std::env::var(CHILD_MODE) else {
            return;
        };
        match mode.as_str() {
            "ok" => {
                println!();
                println!("{}", valid_report_json());
            }
            "malformed" => println!("\n{{\"schema\":1,\"checks\":[}}"),
            "duplicate" => println!(
                "\n{}",
                valid_report_json().replace("\"capture\"", "\"runtime\"")
            ),
            "wait" => {
                if let Ok(ready) = std::env::var("ORANGE_TEST_TROUBLESHOOT_READY") {
                    std::fs::write(ready, "ready").unwrap();
                }
                std::thread::sleep(Duration::from_secs(30));
            }
            _ => panic!("unknown troubleshoot fixture mode"),
        }
    }

    #[test]
    fn malformed_and_duplicate_reports_are_rejected() {
        for mode in ["malformed", "duplicate"] {
            let error = run_checks(fixture_command(mode), &AtomicBool::new(false)).unwrap_err();
            assert_eq!(error, OUTPUT_INVALID_DETAIL);
        }
    }

    #[test]
    fn incomplete_or_unknown_check_contracts_cannot_be_reported_as_passed() {
        // An empty array would otherwise make an all-pass summary vacuously
        // true; old/new incompatible binaries must not claim a successful run.
        for json in [
            r#"{"schema":1,"checks":[]}"#.to_string(),
            valid_report_json().replace("\"schema\":1", "\"schema\":2"),
            valid_report_json().replace("\"pass\"", "\"unknown\""),
            valid_report_json().replace("\"runtime\"", "\"unknown\""),
            valid_report_json().replace("Runtime available", ""),
        ] {
            assert!(parse_checks(json.as_bytes()).is_err());
        }
    }

    #[test]
    fn report_details_are_bounded_unicode_text_without_control_characters() {
        // The CLI contract is bounded plain text. A malformed child must not
        // inject terminal controls or overflow the native Settings report.
        let mut wire: serde_json::Value = serde_json::from_str(valid_report_json()).unwrap();
        wire["checks"][0]["detail"] = "Runtime\n\tavailable\u{0}".into();
        wire["checks"][1]["detail"] = "é".repeat(300).into();
        let checks = parse_checks(&serde_json::to_vec(&wire).unwrap()).unwrap();
        assert_eq!(checks[0].detail, "Runtime available");
        assert_eq!(checks[1].detail.chars().count(), 240);
        assert!(!checks
            .iter()
            .any(|check| check.detail.chars().any(char::is_control)));

        wire["checks"][0]["detail"] = "\u{0}\n\t".into();
        assert!(parse_checks(&serde_json::to_vec(&wire).unwrap()).is_err());
    }

    #[test]
    fn a_timed_out_child_returns_the_fixed_failure_copy() {
        let cancel = AtomicBool::new(false);
        let checks = supervisor::run_bounded_command(
            fixture_command("wait"),
            &cancel,
            Duration::from_millis(50),
            MAX_OUTPUT_BYTES,
            "fixture command",
        )
        .unwrap_err()
        .to_string();
        assert!(checks.contains("timed out"), "{checks}");
    }

    #[test]
    fn cancelling_kills_a_running_child_before_the_outer_deadline() {
        let directory = tempfile::tempdir().unwrap();
        let ready = directory.path().join("ready");
        let cancel = AtomicBool::new(false);
        std::thread::scope(|scope| {
            let observer = scope.spawn(|| {
                let deadline = Instant::now() + Duration::from_secs(3);
                while !ready.exists() {
                    assert!(Instant::now() < deadline, "child never started");
                    std::thread::sleep(Duration::from_millis(1));
                }
                cancel.store(true, Ordering::Release);
            });
            let mut command = fixture_command("wait");
            command.env("ORANGE_TEST_TROUBLESHOOT_READY", &ready);
            let error = run_checks(command, &cancel).unwrap_err();
            assert_eq!(error, CHILD_FAILURE_DETAIL);
            observer.join().unwrap();
        });
    }

    #[test]
    fn troubleshoot_state_refuses_duplicate_starts_while_running() {
        // Signout/account transitions clear the current report first. A second
        // click while the cancelled worker is still reaping must not start a
        // replacement command or overwrite the now-empty state.
        let mut state = TroubleshootState::default();
        assert!(state.start_with_command(fixture_command("wait"), None));
        state.completed = Some(failed_run(OUTPUT_INVALID_DETAIL));
        state.clear();
        assert!(!state.has_result());
        assert!(!state.start_with_command(fixture_command("ok"), None));
        let deadline = Instant::now() + Duration::from_secs(3);
        while state.is_running() {
            state.poll();
            assert!(
                Instant::now() < deadline,
                "troubleshoot worker did not stop"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn clear_is_non_blocking_and_poll_reaps_the_cancelled_worker() {
        // `clear` runs on the UI thread during signout/account changes. It must
        // not wait for a slow child; poll performs the eventual join/reap.
        let directory = tempfile::tempdir().unwrap();
        let ready = directory.path().join("ready");
        let mut command = fixture_command("wait");
        command.env("ORANGE_TEST_TROUBLESHOOT_READY", &ready);
        let mut state = TroubleshootState::default();
        assert!(state.start_with_command(command, None));

        let started_deadline = Instant::now() + Duration::from_secs(3);
        while !ready.exists() {
            assert!(Instant::now() < started_deadline, "child never started");
            std::thread::sleep(Duration::from_millis(1));
        }

        let clear_started = Instant::now();
        state.clear();
        assert!(clear_started.elapsed() < Duration::from_millis(100));
        assert!(state.is_running());
        assert!(!state.has_result());

        let reap_deadline = Instant::now() + Duration::from_secs(3);
        while state.is_running() {
            state.poll();
            assert!(
                Instant::now() < reap_deadline,
                "cancelled troubleshoot worker was not reaped"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(!state.has_result());
    }
}
