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
mod logs;
mod upload;

const CHILD_TIMEOUT: Duration = Duration::from_secs(20);
const CHECK_JOIN_TIMEOUT: Duration = Duration::from_secs(5);
const UPLOAD_JOIN_TIMEOUT: Duration = Duration::from_secs(20);
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
            Self::Runtime => "Orange setup",
            Self::Capture => "Screen sharing",
            Self::Encoder => "Sending video",
            Self::Decoder => "Playing video",
            Self::Audio => "Sound",
            Self::Signalling => "Orange connection",
            Self::Stun => "Network check",
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
            "Everything checked looks good."
        } else if self
            .checks
            .iter()
            .any(|check| check.status == CheckStatus::Fail)
        {
            "Some checks need attention."
        } else {
            "Some checks could not be completed."
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
                CHECK_JOIN_TIMEOUT,
                "troubleshoot worker",
            );
        }
        finished
    }

    fn join(&mut self) {
        if let Some(worker) = self.worker.take() {
            background::join_background_worker(worker, CHECK_JOIN_TIMEOUT, "troubleshoot worker");
        }
    }
}

struct UploadJob {
    cancel: Arc<AtomicBool>,
    cancelled_at: Option<Instant>,
    receiver: mpsc::Receiver<Result<String, upload::UploadError>>,
    worker: Option<JoinHandle<()>>,
    report_revision: u64,
}

impl UploadJob {
    fn cancel(&mut self) {
        self.cancel.store(true, Ordering::Release);
        self.cancelled_at.get_or_insert_with(Instant::now);
    }

    fn is_finished(&self) -> bool {
        let finished = self.worker.as_ref().is_none_or(JoinHandle::is_finished);
        if !finished {
            background::check_cancel_deadline(
                self.cancelled_at,
                UPLOAD_JOIN_TIMEOUT,
                "troubleshoot upload worker",
            );
        }
        finished
    }

    fn join(&mut self) {
        if let Some(worker) = self.worker.take() {
            background::join_background_worker(
                worker,
                UPLOAD_JOIN_TIMEOUT,
                "troubleshoot upload worker",
            );
        }
    }
}

impl Drop for UploadJob {
    fn drop(&mut self) {
        self.cancel();
        self.join();
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(super) enum UploadUiState {
    #[default]
    Hidden,
    Idle,
    Sending,
    Cancelling,
    Sent,
    Retry,
    SignInRequired,
}

impl UploadUiState {
    pub(super) fn message(&self) -> Option<&'static str> {
        match self {
            Self::Hidden | Self::Idle => None,
            Self::Sending => Some("Sending…"),
            Self::Cancelling => Some("Stopping…"),
            Self::Sent => Some("Report sent. Thank you!"),
            Self::Retry => Some("Could not send the report. Please try again."),
            Self::SignInRequired => Some("Sign in to send a report."),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct FriendlyCheck {
    pub(super) label: &'static str,
    pub(super) status: CheckStatus,
    pub(super) state_text: &'static str,
    pub(super) action_text: &'static str,
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
    upload_job: Option<UploadJob>,
    completed: Option<CompletedRun>,
    upload_state: UploadUiState,
    receipt_id: Option<String>,
    report_revision: u64,
    generation: u64,
}

impl TroubleshootState {
    pub(super) fn start(&mut self, server: &str, diagnostics: Option<PathBuf>) -> bool {
        if self.job.is_some() || self.upload_job.is_some() {
            return false;
        }
        let mut command = match supervisor::orange_command() {
            Ok(command) => command,
            Err(_) => {
                self.upload_state = UploadUiState::Idle;
                self.receipt_id = None;
                self.report_revision = self.report_revision.wrapping_add(1);
                self.completed = Some(failed_run(CHILD_FAILURE_DETAIL));
                self.generation = self.generation.wrapping_add(1);
                return false;
            }
        };
        command.arg("troubleshoot").args(["--server", server]);
        self.start_with_command(command, diagnostics)
    }

    fn start_with_command(&mut self, command: Command, diagnostics: Option<PathBuf>) -> bool {
        if self.job.is_some() || self.upload_job.is_some() {
            return false;
        }
        self.completed = None;
        self.upload_state = UploadUiState::Hidden;
        self.receipt_id = None;
        self.report_revision = self.report_revision.wrapping_add(1);
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
        let mut changed = self.poll_upload();

        let Some(job) = self.job.as_mut() else {
            return changed;
        };
        if !job.is_finished() {
            return changed;
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
        if self.completed.is_some() {
            self.upload_state = UploadUiState::Idle;
            self.receipt_id = None;
        }
        self.report_revision = self.report_revision.wrapping_add(1);
        self.generation = self.generation.wrapping_add(1);
        changed = true;
        changed
    }

    fn poll_upload(&mut self) -> bool {
        let Some(job) = self.upload_job.as_mut() else {
            return false;
        };
        if !job.is_finished() {
            return false;
        }
        let report_revision = job.report_revision;
        job.join();
        let cancelled = job.cancel.load(Ordering::Acquire);
        let result = if cancelled {
            None
        } else {
            job.receiver.try_recv().ok()
        };
        self.upload_job = None;
        if cancelled {
            if report_revision == self.report_revision && self.completed.is_some() {
                self.upload_state = UploadUiState::Idle;
                self.receipt_id = None;
            }
            self.generation = self.generation.wrapping_add(1);
            return true;
        }
        if report_revision != self.report_revision {
            self.generation = self.generation.wrapping_add(1);
            return true;
        }
        self.upload_state = match result {
            Some(Ok(report_id)) => {
                self.receipt_id = Some(report_id);
                UploadUiState::Sent
            }
            Some(Err(upload::UploadError::SignedOut)) => UploadUiState::SignInRequired,
            Some(Err(_)) => UploadUiState::Retry,
            None => UploadUiState::Retry,
        };
        self.generation = self.generation.wrapping_add(1);
        true
    }

    pub(super) fn cancel(&mut self) {
        if let Some(job) = self.job.as_mut() {
            job.cancel();
        }
        let mut changed = false;
        if let Some(job) = self.upload_job.as_mut() {
            job.cancel();
            self.upload_state = UploadUiState::Cancelling;
            self.receipt_id = None;
            changed = true;
        }
        if changed {
            self.generation = self.generation.wrapping_add(1);
        }
    }

    pub(super) fn clear(&mut self) {
        self.cancel();
        self.completed = None;
        self.upload_state = UploadUiState::Hidden;
        self.receipt_id = None;
        self.report_revision = self.report_revision.wrapping_add(1);
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

    pub(super) fn headline(&self) -> &'static str {
        if self.job.is_some() {
            "Checking Orange and your connection…"
        } else if self.completed.is_some() {
            "Here’s what we found:"
        } else {
            "Having trouble sharing or watching? Let’s check."
        }
    }

    pub(super) fn friendly_checks(&self) -> Vec<FriendlyCheck> {
        self.checks()
            .iter()
            .map(|check| {
                let (state_text, action_text) = match check.status {
                    CheckStatus::Pass => ("Looks good", ""),
                    CheckStatus::Fail => ("Needs attention", fail_action(check.id)),
                    CheckStatus::Inconclusive => ("Could not check", inconclusive_action(check.id)),
                };
                FriendlyCheck {
                    label: check.label,
                    status: check.status,
                    state_text,
                    action_text,
                }
            })
            .collect()
    }

    pub(super) fn upload_state(&self) -> UploadUiState {
        if self.completed.is_none() {
            UploadUiState::Hidden
        } else {
            self.upload_state.clone()
        }
    }

    pub(super) fn is_uploading(&self) -> bool {
        self.upload_job.is_some()
    }

    pub(super) fn send_report(
        &mut self,
        server: &str,
        token: Option<&str>,
        diagnostics: Option<PathBuf>,
        version: &str,
        build: &str,
    ) -> bool {
        if self.upload_job.is_some()
            || self.completed.is_none()
            || self.upload_state == UploadUiState::Sent
        {
            return false;
        }
        let Some(token) = token.filter(|token| !token.trim().is_empty()) else {
            self.upload_state = UploadUiState::SignInRequired;
            self.generation = self.generation.wrapping_add(1);
            return false;
        };
        let Some(report) = self.report_text(version, build) else {
            return false;
        };

        let report_revision = self.report_revision;
        let server = server.to_string();
        let token = token.to_string();
        let cancel = Arc::new(AtomicBool::new(false));
        let worker_cancel = Arc::clone(&cancel);
        let (sender, receiver) = mpsc::channel();
        let worker = std::thread::spawn(move || {
            if worker_cancel.load(Ordering::Acquire) {
                return;
            }
            let mut report = report;
            let logs = diagnostics
                .as_deref()
                .map(|directory| logs::collect(directory, &worker_cancel))
                .unwrap_or_else(|| Ok(Vec::new()));
            let logs = match logs {
                Ok(logs) => logs,
                Err(error) => {
                    report.push_str("\n\nLog collection note: ");
                    report.push_str(&error);
                    Vec::new()
                }
            };
            if worker_cancel.load(Ordering::Acquire) {
                return;
            }
            let _ = sender.send(upload::send(&server, &token, &report, logs));
        });

        self.upload_job = Some(UploadJob {
            cancel,
            cancelled_at: None,
            receiver,
            worker: Some(worker),
            report_revision,
        });
        self.upload_state = UploadUiState::Sending;
        self.receipt_id = None;
        self.generation = self.generation.wrapping_add(1);
        true
    }

    pub(super) fn checks(&self) -> &[CheckResult] {
        self.completed
            .as_ref()
            .map(|run| run.checks.as_slice())
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
        ];
        if let Some(receipt_id) = &self.receipt_id {
            lines.push(format!("Support reference: {receipt_id}"));
        }
        lines.push(String::new());
        lines.push(format!("Summary: {}", run.summary()));
        lines.push(String::new());
        lines.push("Current checks".to_string());
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

fn fail_action(check_id: &str) -> &'static str {
    match check_id {
        "runtime" | "capture" | "audio" => "Restart or reinstall Orange, then try again.",
        "encoder" | "decoder" => "Try reinstalling Orange or updating your graphics driver.",
        "signalling" => "Check your internet connection, then try again.",
        "stun" => "Try again, or try another network.",
        _ => "Try again.",
    }
}

fn inconclusive_action(check_id: &str) -> &'static str {
    match check_id {
        "stun" => "Try again, or try another network.",
        _ => "Try this check again in a moment.",
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

    #[test]
    fn friendly_projection_keeps_the_seven_labels_and_hides_technical_details() {
        let checks = parse_checks(
            br#"{"schema":1,"checks":[{"id":"runtime","status":"pass","detail":"plugin build 123"},{"id":"capture","status":"fail","detail":"codec failed"},{"id":"encoder","status":"inconclusive","detail":"stun ice turn"},{"id":"decoder","status":"pass","detail":"history"},{"id":"audio","status":"fail","detail":"time"},{"id":"signalling","status":"inconclusive","detail":"build"},{"id":"stun","status":"pass","detail":"plugin"}]}"#,
        )
        .unwrap();
        let state = TroubleshootState {
            completed: Some(CompletedRun {
                checks,
                history: Vec::new(),
                generated_at_unix_ms: 1,
            }),
            ..Default::default()
        };
        let projected = state.friendly_checks();
        assert_eq!(
            projected
                .iter()
                .map(|check| check.label)
                .collect::<Vec<_>>(),
            vec![
                "Orange setup",
                "Screen sharing",
                "Sending video",
                "Playing video",
                "Sound",
                "Orange connection",
                "Network check",
            ]
        );
        assert_eq!(projected[0].state_text, "Looks good");
        assert_eq!(projected[1].state_text, "Needs attention");
        assert_eq!(projected[2].state_text, "Could not check");
        assert_eq!(
            projected[1].action_text,
            "Restart or reinstall Orange, then try again."
        );
        assert_eq!(
            projected[4].action_text,
            "Restart or reinstall Orange, then try again."
        );
        assert_eq!(
            projected[2].action_text,
            "Try this check again in a moment."
        );
        assert_eq!(
            projected[5].action_text,
            "Try this check again in a moment."
        );
        let ui_text = projected
            .iter()
            .flat_map(|check| [check.label, check.state_text, check.action_text])
            .collect::<Vec<_>>()
            .join(" ")
            .to_ascii_lowercase();
        for secret in [
            "plugin", "codec", "stun", "ice", "turn", "build", "time", "history",
        ] {
            assert!(!ui_text.contains(secret), "leaked {secret}: {ui_text}");
        }
    }

    #[test]
    fn failed_items_show_per_check_actions_without_fake_transport_claims() {
        let state = TroubleshootState {
            completed: Some(CompletedRun {
                checks: vec![
                    CheckResult {
                        label: "Orange setup",
                        id: "runtime",
                        status: CheckStatus::Fail,
                        detail: "x".into(),
                    },
                    CheckResult {
                        label: "Screen sharing",
                        id: "capture",
                        status: CheckStatus::Fail,
                        detail: "x".into(),
                    },
                    CheckResult {
                        label: "Sending video",
                        id: "encoder",
                        status: CheckStatus::Fail,
                        detail: "x".into(),
                    },
                    CheckResult {
                        label: "Playing video",
                        id: "decoder",
                        status: CheckStatus::Fail,
                        detail: "x".into(),
                    },
                    CheckResult {
                        label: "Sound",
                        id: "audio",
                        status: CheckStatus::Fail,
                        detail: "x".into(),
                    },
                    CheckResult {
                        label: "Orange connection",
                        id: "signalling",
                        status: CheckStatus::Fail,
                        detail: "x".into(),
                    },
                    CheckResult {
                        label: "Network check",
                        id: "stun",
                        status: CheckStatus::Fail,
                        detail: "x".into(),
                    },
                ],
                history: Vec::new(),
                generated_at_unix_ms: 1,
            }),
            ..Default::default()
        };
        let checks = state.friendly_checks();
        assert_eq!(
            checks[0].action_text,
            "Restart or reinstall Orange, then try again."
        );
        assert_eq!(
            checks[1].action_text,
            "Restart or reinstall Orange, then try again."
        );
        assert_eq!(
            checks[2].action_text,
            "Try reinstalling Orange or updating your graphics driver."
        );
        assert_eq!(
            checks[3].action_text,
            "Try reinstalling Orange or updating your graphics driver."
        );
        assert_eq!(
            checks[4].action_text,
            "Restart or reinstall Orange, then try again."
        );
        assert_eq!(
            checks[5].action_text,
            "Check your internet connection, then try again."
        );
        assert_eq!(checks[6].action_text, "Try again, or try another network.");
        let ui_text = checks
            .iter()
            .flat_map(|check| [check.label, check.state_text, check.action_text])
            .collect::<Vec<_>>()
            .join(" ")
            .to_ascii_lowercase();
        for claim in ["firewall", "drivercause", "measured"] {
            assert!(
                !ui_text.contains(claim),
                "unexpected claim {claim}: {ui_text}"
            );
        }
    }

    #[test]
    fn send_report_requires_sign_in_prevents_double_send_and_clear_suppresses_stale_results() {
        let ws = "ws://127.0.0.1:9/ws";
        let mut state = TroubleshootState::default();
        assert!(state.start_with_command(fixture_command("ok"), None));
        let deadline = Instant::now() + Duration::from_secs(3);
        while !state.has_result() {
            state.poll();
            assert!(Instant::now() < deadline, "checks did not finish");
            std::thread::sleep(Duration::from_millis(2));
        }

        assert!(!state.send_report(ws, None, None, "1.0.0", "build"));
        assert_eq!(state.upload_state(), UploadUiState::SignInRequired);
        assert!(!state.send_report(ws, Some("   "), None, "1.0.0", "build"));
        assert_eq!(state.upload_state(), UploadUiState::SignInRequired);

        assert!(state.send_report(ws, Some("token"), None, "1.0.0", "build"));
        assert!(state.is_uploading());
        assert!(!state.send_report(ws, Some("token"), None, "1.0.0", "build"));

        state.clear();
        assert_eq!(state.upload_state(), UploadUiState::Hidden);
        assert!(!state.has_result());

        let deadline = Instant::now() + Duration::from_secs(3);
        while state.is_uploading() {
            state.poll();
            assert!(Instant::now() < deadline, "upload worker was not reaped");
            std::thread::sleep(Duration::from_millis(2));
        }
        assert_eq!(state.upload_state(), UploadUiState::Hidden);
        assert!(!state.has_result());
    }

    #[test]
    fn a_sent_report_cannot_be_sent_again_until_a_new_run_and_receipt_enters_copied_report() {
        let server = crate::background::tests::HttpServer::new(vec![
            (
                201,
                br#"{"report_id":"0123456789abcdef0123456789abcdef"}"#.to_vec(),
            ),
            (
                201,
                br#"{"report_id":"fedcba9876543210fedcba9876543210"}"#.to_vec(),
            ),
        ]);
        let ws = server.url("/ws").replacen("http://", "ws://", 1);
        let mut state = TroubleshootState::default();
        assert!(state.start_with_command(fixture_command("ok"), None));
        let deadline = Instant::now() + Duration::from_secs(3);
        while !state.has_result() {
            state.poll();
            assert!(Instant::now() < deadline, "checks did not finish");
            std::thread::sleep(Duration::from_millis(2));
        }

        assert!(state.send_report(ws.as_str(), Some("token"), None, "1.0.0", "build"));
        let deadline = Instant::now() + Duration::from_secs(3);
        while state.is_uploading() {
            state.poll();
            assert!(Instant::now() < deadline, "upload did not finish");
            std::thread::sleep(Duration::from_millis(2));
        }
        assert_eq!(state.upload_state(), UploadUiState::Sent);
        assert!(!state.send_report(ws.as_str(), Some("token"), None, "1.0.0", "build"));

        let report = state.report_text("1.0.0", "build").unwrap();
        assert!(report.contains("Support reference: 0123456789abcdef0123456789abcdef"));

        assert!(state.start_with_command(fixture_command("ok"), None));
        let deadline = Instant::now() + Duration::from_secs(3);
        while !state.has_result() {
            state.poll();
            assert!(Instant::now() < deadline, "second checks did not finish");
            std::thread::sleep(Duration::from_millis(2));
        }
        let report = state.report_text("1.0.0", "build").unwrap();
        assert!(!report.contains("Support reference:"));

        assert!(state.send_report(ws.as_str(), Some("token"), None, "1.0.0", "build"));
        let deadline = Instant::now() + Duration::from_secs(3);
        while state.is_uploading() {
            state.poll();
            assert!(Instant::now() < deadline, "second upload did not finish");
            std::thread::sleep(Duration::from_millis(2));
        }
        assert_eq!(state.upload_state(), UploadUiState::Sent);

        assert_eq!(server.finish().len(), 2);
    }

    #[test]
    fn cancelling_a_blocked_local_upload_returns_immediately_and_reaps_without_sent_state() {
        use std::io::{BufRead, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let (entered_tx, entered_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let worker = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(3)))
                .unwrap();
            stream
                .set_write_timeout(Some(Duration::from_secs(3)))
                .unwrap();
            let mut reader = std::io::BufReader::new(stream);
            let mut request = String::new();
            loop {
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                request.push_str(&line);
                if line == "\r\n" {
                    break;
                }
            }
            let content_length = request
                .lines()
                .find_map(|line| {
                    line.split_once(':').and_then(|(name, value)| {
                        name.eq_ignore_ascii_case("content-length")
                            .then(|| value.trim().parse::<usize>().ok())
                            .flatten()
                    })
                })
                .unwrap_or(0);
            if content_length > 0 {
                let mut body = vec![0; content_length];
                std::io::Read::read_exact(&mut reader, &mut body).unwrap();
            }
            entered_tx.send(()).unwrap();
            let _ = release_rx.recv_timeout(Duration::from_secs(3));
            let stream = reader.get_mut();
            write!(
                stream,
                "HTTP/1.1 201 Created\r\nContent-Length: 48\r\nConnection: close\r\n\r\n{{\"report_id\":\"0123456789abcdef0123456789abcdef\"}}"
            )
            .unwrap();
            stream.flush().unwrap();
        });

        let ws = format!("ws://{address}/ws");
        let mut state = TroubleshootState::default();
        assert!(state.start_with_command(fixture_command("ok"), None));
        let deadline = Instant::now() + Duration::from_secs(3);
        while !state.has_result() {
            state.poll();
            assert!(Instant::now() < deadline, "checks did not finish");
            std::thread::sleep(Duration::from_millis(2));
        }

        assert!(state.send_report(&ws, Some("token"), None, "1.0.0", "build"));
        entered_rx.recv_timeout(Duration::from_secs(3)).unwrap();
        let cancel_started = Instant::now();
        state.cancel();
        assert!(cancel_started.elapsed() < Duration::from_millis(100));
        assert_eq!(state.upload_state(), UploadUiState::Cancelling);
        assert!(state.is_uploading());

        release_tx.send(()).unwrap();
        let deadline = Instant::now() + Duration::from_secs(3);
        while state.is_uploading() {
            state.poll();
            assert!(Instant::now() < deadline, "cancelled upload was not reaped");
            std::thread::sleep(Duration::from_millis(2));
        }

        assert_eq!(state.upload_state(), UploadUiState::Idle);
        assert!(state
            .report_text("1.0.0", "build")
            .is_some_and(|report| !report.contains("Support reference:")));
        worker.join().unwrap();
    }
}
