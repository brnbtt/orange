//! Windows Firewall inspection and targeted, user-requested repair.
//!
//! Read-only inspection (`inspect`) never mutates anything and needs no
//! elevation. It runs `crates/orange/src/firewall/inspect_repair.ps1` (baked
//! into the binary with `include_str!`, fed to `powershell.exe` over stdin, so
//! nothing is ever written to disk for a privileged process to load) and
//! classifies the result in pure, unit-tested Rust (`classify_facts`).
//!
//! The script reads facts primarily through the `HNetCfg.FwPolicy2` COM
//! object rather than the `NetSecurity` CIM cmdlets: a live probe against this
//! machine's real `Get-NetFirewallRule` confirmed the returned object has no
//! `Protocol` property at all (`Set-StrictMode -Version Latest` turns reading
//! one into a terminating error) and that `-All` is its own parameter set that
//! cannot be combined with `-Direction`/`-Enabled`/`-Action`. See the script's
//! header comment for the details and for the one thing COM cannot answer
//! (whether a rule is locally owned or Group-Policy-sourced), which still uses
//! CIM, correctly this time.
//!
//! Repair (`repair`) only changes rules for this exact executable path on
//! currently active profiles. It may disable (or profile-narrow) an enabled,
//! unrestricted, locally-owned inbound Block rule, then add idempotent
//! inbound TCP/UDP Allow rules for that same path. It never touches a global
//! default policy, a Group Policy-sourced rule, another application's rule,
//! a port/address/service/interface/package/user-restricted rule, or an
//! inactive network profile.
//! See `classify_facts` for the full decision table and `inspect_repair.ps1`
//! for the mutation itself, which re-derives eligibility immediately before
//! each mutating call rather than trusting facts collected moments earlier.
//!
//! Windows requires administrator permission to change firewall rules, so an
//! unelevated `repair` re-launches this same executable elevated
//! (`ShellExecuteExW` with the `runas` verb) and waits for it. The elevated
//! helper holds one process HANDLE to the unelevated requester (immune to PID
//! reuse, unlike re-opening by PID) for its whole lifetime and refuses to
//! mutate if that requester is no longer alive, both before its own read-only
//! inspection and again immediately before launching the mutating script
//! call. The script itself also re-checks the same PID immediately before
//! each individual mutating call, as a second, PID-based line of defence for
//! the duration the script runs after Rust's own check. None of this can
//! undo a mutation that already completed by the time a cancellation is
//! observed; this only bounds how late a cancellation can still prevent one.
//!
//! Every PowerShell child is bounded: an owned Job Object with
//! `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` is assigned to the process immediately
//! after spawn - before any script input is provided - and closing our one
//! handle to it (on success, on timeout, or on an early return/panic through
//! `JobGuard`'s `Drop`) terminates the PowerShell process and anything it
//! spawned. stdin is written from an owned writer thread (so a child that
//! never reads it cannot hold the wall-clock budget hostage on a blocked
//! write), stdout/stderr are each capped at 16 KiB with truncation tracked
//! rather than silently discarded, and the whole run is capped at 15 seconds.
//! A result is only ever trusted (parsed as facts) when the child exited on
//! its own with a recognised exit code and neither stream was truncated.

use serde::Deserialize;
use std::collections::HashSet;
use std::ffi::c_void;
use std::io::{Read, Write};
use std::os::windows::io::AsRawHandle;
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use windows::core::{HSTRING, PCWSTR};
use windows::Win32::Foundation::{
    CloseHandle, ERROR_CANCELLED, FILETIME, HANDLE, HINSTANCE, HWND, WAIT_TIMEOUT,
};
use windows::Win32::Globalization::{CompareStringOrdinal, CSTR_EQUAL};
use windows::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
    SetInformationJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
    JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
};
use windows::Win32::System::Registry::HKEY;
use windows::Win32::System::SystemInformation::GetSystemDirectoryW;
use windows::Win32::System::Threading::{
    GetCurrentProcess, GetExitCodeProcess, GetProcessTimes, OpenProcess, WaitForSingleObject,
    PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE,
};
use windows::Win32::UI::Shell::{ShellExecuteExW, SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW};
use windows::Win32::UI::WindowsAndMessaging::SW_HIDE;

const DETAIL_LIMIT: usize = 240;
const SCRIPT_TIMEOUT: Duration = Duration::from_secs(15);
const OUTPUT_LIMIT: usize = 16 * 1024;
const PIPE_JOIN_TIMEOUT: Duration = Duration::from_secs(2);
/// Absolute-budget math, not an independent guess: the frontend's bounded
/// child supervisor kills the whole `repair-network` process at 90 seconds
/// (see `orange-client`'s troubleshoot child supervisor). An unelevated run
/// first spends up to `SCRIPT_TIMEOUT` (15s) on its own read-only `inspect()`
/// before ever launching the elevated helper, and the elevated helper itself
/// spends up to two script runs (its own read-only inspect, then the repair
/// script) inside the time this constant bounds. 15 + 55 leaves 20 seconds of
/// margin for process/UAC-broker startup rather than crowding the frontend's
/// own bound; a normal (non-abandoned) elevation finishes in a few seconds.
const ELEVATION_WAIT_MS: u32 = 55_000;

/// `orange repair-network` process exit codes. The CLI wiring that returns
/// these from `main` belongs to the parent change that adds the subcommand;
/// this module only defines and produces them.
pub(crate) const REPAIR_CHANGED_OR_VERIFIED: i32 = 0;
pub(crate) const REPAIR_NOTHING_TO_CHANGE: i32 = 2;
pub(crate) const REPAIR_NOT_LOCALLY_REPAIRABLE: i32 = 3;
pub(crate) const REPAIR_UAC_DECLINED: i32 = 4;
pub(crate) const REPAIR_FAILED: i32 = 5;
pub(crate) const REPAIR_REQUESTER_CANCELLED: i32 = 6;

const INSPECT_REPAIR_SCRIPT: &str = include_str!("firewall/inspect_repair.ps1");

const VALID_PROFILE_NAMES: [&str; 3] = ["Domain", "Private", "Public"];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FirewallStatus {
    Clear,
    Blocked,
    Managed,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Inspection {
    pub status: FirewallStatus,
    pub detail: String,
    pub repairable: bool,
}

fn inspection(status: FirewallStatus, detail: &str, repairable: bool) -> Inspection {
    debug_assert!(detail.len() <= DETAIL_LIMIT, "firewall detail too long");
    Inspection {
        status,
        detail: detail.to_string(),
        repairable,
    }
}

fn unknown(detail: &str) -> Inspection {
    inspection(FirewallStatus::Unknown, detail, false)
}

// ---------------------------------------------------------------------------
// Facts DTO: exactly what `inspect_repair.ps1` emits as compact JSON. Kept
// deliberately free of any Rust-side path matching until `classify_facts`, so
// "exact path" and "other app" are ordinary unit tests rather than requiring a
// real Windows Firewall to exercise.
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct ProfileFact {
    name: String,
    enabled: bool,
    #[serde(rename = "blockAllInbound")]
    block_all_inbound: bool,
    #[serde(rename = "outboundBlock")]
    outbound_block: bool,
}

#[derive(Debug, Deserialize)]
struct AppRuleFact {
    program: String,
    enabled: bool,
    action: String,
    protocol: String,
    profiles: Vec<String>,
    restricted: bool,
}

#[derive(Debug, Deserialize)]
struct FirewallFacts {
    #[serde(rename = "activeProfiles")]
    active_profiles: Vec<String>,
    profiles: Vec<ProfileFact>,
    #[serde(rename = "appRules")]
    app_rules: Vec<AppRuleFact>,
    #[serde(rename = "managedOrigin")]
    managed_origin: bool,
}

#[derive(Debug, Deserialize)]
struct CancelledMarker {
    #[serde(rename = "requesterCancelled")]
    requester_cancelled: bool,
}

#[derive(Debug, Deserialize)]
struct ScriptError {
    error: String,
}

#[derive(Debug)]
enum ScriptOutcome {
    Facts(FirewallFacts),
    RequesterCancelled,
    ScriptError(#[allow(dead_code)] String),
}

/// Only ever called with an exit code this module recognises as trustworthy
/// (see `run_facts_script`): 0 for a completed scan/repair, 1 for the
/// script's own classified `Write-FailureAndExit`. Anything else is rejected
/// before this is reached, so it is never asked to make sense of arbitrary
/// process output.
fn parse_script_outcome(exit_code: i32, stdout: &[u8]) -> Result<ScriptOutcome, &'static str> {
    let text = std::str::from_utf8(stdout).map_err(|_| "malformed")?.trim();
    match exit_code {
        0 => {
            if let Ok(facts) = serde_json::from_str::<FirewallFacts>(text) {
                return Ok(ScriptOutcome::Facts(facts));
            }
            if let Ok(marker) = serde_json::from_str::<CancelledMarker>(text) {
                if marker.requester_cancelled {
                    return Ok(ScriptOutcome::RequesterCancelled);
                }
            }
            Err("malformed")
        }
        1 => serde_json::from_str::<ScriptError>(text)
            .map(|error| ScriptOutcome::ScriptError(error.error))
            .map_err(|_| "malformed"),
        _ => Err("unexpected-exit"),
    }
}

fn paths_match(a: &str, b: &str) -> bool {
    let a: Vec<u16> = a.trim().encode_utf16().collect();
    let b: Vec<u16> = b.trim().encode_utf16().collect();
    // SAFETY: both slices contain initialized UTF-16 and remain valid for the
    // call. Their explicit lengths are bounded by the facts/output path limits.
    unsafe { CompareStringOrdinal(&a, &b, true) == CSTR_EQUAL }
}

/// The full classification decision. Pure and total: every branch returns,
/// nothing here touches the filesystem or the network. Precedence matters:
/// a managed origin or a "block all incoming connections" policy must refuse
/// repair even when a local rule also happens to match, because clearing the
/// local rule alone would not restore connectivity and reporting success
/// would be a lie.
fn classify_facts(facts: &FirewallFacts, exe: &str) -> Inspection {
    if facts.active_profiles.is_empty() {
        return unknown("Could not determine the active network profile.");
    }
    if facts
        .active_profiles
        .iter()
        .any(|name| !VALID_PROFILE_NAMES.contains(&name.as_str()))
    {
        return unknown("Firewall diagnostic data reported an unrecognized network profile.");
    }
    if facts
        .active_profiles
        .iter()
        .any(|active| !facts.profiles.iter().any(|p| &p.name == active))
    {
        // An active profile with no corresponding fact record must never be
        // read as "that profile is off" - it means the data is incomplete.
        return unknown("Firewall diagnostic data did not cover every active network profile.");
    }
    let active: HashSet<&str> = facts.active_profiles.iter().map(String::as_str).collect();

    let any_enabled = facts
        .profiles
        .iter()
        .any(|p| active.contains(p.name.as_str()) && p.enabled);
    if !any_enabled {
        return inspection(
            FirewallStatus::Clear,
            "Windows Firewall is off for the active network profile.",
            false,
        );
    }

    if facts.managed_origin {
        return inspection(
            FirewallStatus::Managed,
            "Managed Windows policy prevents a local Orange repair.",
            false,
        );
    }

    let block_all_inbound = facts
        .profiles
        .iter()
        .any(|p| active.contains(p.name.as_str()) && p.block_all_inbound);
    if block_all_inbound {
        return inspection(
            FirewallStatus::Managed,
            "Block all incoming connections is active for the active network profile.",
            false,
        );
    }

    let matching: Vec<&AppRuleFact> = facts
        .app_rules
        .iter()
        .filter(|rule| rule.enabled)
        .filter(|rule| rule.action == "Block")
        .filter(|rule| matches!(rule.protocol.as_str(), "TCP" | "UDP" | "Any"))
        .filter(|rule| paths_match(&rule.program, exe))
        .filter(|rule| rule.profiles.iter().any(|p| active.contains(p.as_str())))
        .collect();

    if !matching.is_empty() {
        if matching.iter().any(|rule| rule.restricted) {
            return inspection(
                FirewallStatus::Blocked,
                "A scoped local rule blocks Orange; it is not automatically repairable.",
                false,
            );
        }
        return inspection(
            FirewallStatus::Blocked,
            "A local rule blocks Orange on the active network profile.",
            true,
        );
    }

    let outbound_block = facts
        .profiles
        .iter()
        .any(|p| active.contains(p.name.as_str()) && p.outbound_block);
    if outbound_block {
        return inspection(
            FirewallStatus::Managed,
            "A default outbound block policy is active for the active network profile.",
            false,
        );
    }

    inspection(
        FirewallStatus::Clear,
        "No matching inbound block rule found for Orange on the active network profile.",
        false,
    )
}

// ---------------------------------------------------------------------------
// Bounded child execution
// ---------------------------------------------------------------------------

struct BoundedRun {
    stdout: Vec<u8>,
    timed_out: bool,
    /// True if either stream hit its retention cap. A result this is set on
    /// is never parsed as trustworthy: truncated JSON can coincidentally
    /// still be *valid* JSON for a smaller, wrong object.
    truncated: bool,
    /// `None` when the process never produced a normal exit status (killed
    /// on timeout, or the platform could not report one).
    exit_code: Option<i32>,
}

/// Closes the one handle this process holds to an unnamed Job Object created
/// with `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`. Windows terminates every member
/// process (the direct child and anything it spawned that stayed in the job)
/// the moment the last handle to the job closes, which is exactly what should
/// happen whether this run finished normally, timed out, or unwound through
/// an early `return`/panic.
struct JobGuard(HANDLE);

impl JobGuard {
    fn new() -> windows::core::Result<Self> {
        // SAFETY: `CreateJobObjectW` is called with no security attributes
        // and no name, which are both valid per its documented contract; the
        // returned handle is immediately owned by `Self`, whose `Drop` closes
        // it, before anything that could fail is attempted.
        let job = unsafe { CreateJobObjectW(None, PCWSTR::null()) }?;
        let guard = Self(job);
        let mut info = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        // SAFETY: `info` outlives the call, and its size and pointer match
        // the class (`JobObjectExtendedLimitInformation`) being set, per
        // `SetInformationJobObject`'s documented contract.
        unsafe {
            SetInformationJobObject(
                guard.0,
                JobObjectExtendedLimitInformation,
                &info as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION as *const c_void,
                std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            )
        }?;
        Ok(guard)
    }

    fn assign(&self, child: &Child) -> windows::core::Result<()> {
        // SAFETY: `child` is a live process this function owns a handle to
        // for the duration of the call (`Child::as_raw_handle` borrows it),
        // and `self.0` is a valid Job Object handle owned by this `JobGuard`.
        let handle = HANDLE(child.as_raw_handle());
        unsafe { AssignProcessToJobObject(self.0, handle) }
    }
}

impl Drop for JobGuard {
    fn drop(&mut self) {
        // SAFETY: `self.0` is a handle this `JobGuard` uniquely owns and has
        // not yet closed; closing it here, exactly once, is what makes
        // `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` take effect.
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}

struct PipeWorker<T> {
    receiver: mpsc::Receiver<T>,
    thread: Option<thread::JoinHandle<()>>,
}

impl<T> PipeWorker<T> {
    fn join(&mut self) -> bool {
        let Some(worker) = self.thread.take() else {
            return true;
        };
        let deadline = Instant::now() + PIPE_JOIN_TIMEOUT;
        while !worker.is_finished() {
            if Instant::now() >= deadline {
                // The job is already closed and every child pipe should have
                // reached EOF. Do not return an unowned native I/O thread.
                eprintln!("Windows policy pipe cleanup exceeded its deadline");
                std::process::abort();
            }
            thread::sleep(Duration::from_millis(1));
        }
        worker.join().is_ok()
    }

    fn finish(mut self) -> Option<T> {
        self.join().then(|| self.receiver.try_recv().ok()).flatten()
    }
}

impl<T> Drop for PipeWorker<T> {
    fn drop(&mut self) {
        self.join();
    }
}

fn bounded_reader(mut pipe: impl Read + Send + 'static, cap: usize) -> PipeWorker<(Vec<u8>, bool)> {
    let (tx, rx) = mpsc::channel();
    let worker = thread::spawn(move || {
        let mut buf = Vec::with_capacity(cap.min(4096));
        let mut chunk = [0u8; 4096];
        let mut truncated = false;
        // Keep draining past the cap (discarding the excess) so a chatty
        // child never blocks on a full pipe waiting for a reader that has
        // stopped reading; `truncated` records that the excess was real,
        // rather than silently discarding it as if nothing was lost.
        loop {
            match pipe.read(&mut chunk) {
                Ok(0) => break,
                Ok(n) => {
                    if buf.len() < cap {
                        let take = (cap - buf.len()).min(n);
                        buf.extend_from_slice(&chunk[..take]);
                        if take < n {
                            truncated = true;
                        }
                    } else {
                        truncated = true;
                    }
                }
                Err(_) => break,
            }
        }
        let _ = tx.send((buf, truncated));
    });
    PipeWorker {
        receiver: rx,
        thread: Some(worker),
    }
}

fn bounded_writer(mut pipe: impl Write + Send + 'static, data: Vec<u8>) -> PipeWorker<bool> {
    let (tx, rx) = mpsc::channel();
    let worker = thread::spawn(move || {
        let written = pipe.write_all(&data).is_ok();
        // Dropping `pipe` here closes the handle, which is the EOF that
        // `powershell -Command -` is waiting for.
        drop(pipe);
        let _ = tx.send(written);
    });
    PipeWorker {
        receiver: rx,
        thread: Some(worker),
    }
}

struct ChildRunGuard {
    child: Child,
    job: Option<JobGuard>,
    stdout: Option<PipeWorker<(Vec<u8>, bool)>>,
    stderr: Option<PipeWorker<(Vec<u8>, bool)>>,
    writer: Option<PipeWorker<bool>>,
}

impl ChildRunGuard {
    fn stop(&mut self) {
        drop(self.job.take());
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Drop for ChildRunGuard {
    fn drop(&mut self) {
        // Also applies on panic/early return: close the job before joining
        // pipes, otherwise a reader could wait for a still-running child.
        self.stop();
        drop(self.writer.take());
        drop(self.stdout.take());
        drop(self.stderr.take());
    }
}

/// Runs `command` with `stdin_data` piped to its stdin (then closed), bounded
/// to `time_limit` wall-clock and `OUTPUT_LIMIT` retained stdout/stderr bytes.
/// A Job Object owned for the lifetime of this call ensures the child and
/// anything it spawns dies when this function returns, one way or another;
/// job assignment happening before the child is given any input is
/// mandatory - assignment failing kills and reaps the child unstarted rather
/// than ever letting it run outside the job.
fn run_bounded_while(
    mut command: Command,
    stdin_data: &[u8],
    time_limit: Duration,
    keep_running: impl Fn() -> bool,
) -> BoundedRun {
    let deadline = Instant::now() + time_limit;
    let unstarted = || BoundedRun {
        stdout: Vec::new(),
        timed_out: false,
        truncated: false,
        exit_code: None,
    };

    if !keep_running() {
        return unstarted();
    }

    command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(_) => return unstarted(),
    };

    let job = match JobGuard::new() {
        Ok(job) => job,
        Err(_) => {
            let _ = child.kill();
            let _ = child.wait();
            return unstarted();
        }
    };
    if job.assign(&child).is_err() {
        let _ = child.kill();
        let _ = child.wait();
        return unstarted();
    }

    // Readers start before the writer, and the writer runs on its own
    // thread: a child that never reads stdin therefore cannot hold this
    // function's wall-clock budget hostage on a blocked `write_all` call.
    let mut run = ChildRunGuard {
        child,
        job: Some(job),
        stdout: None,
        stderr: None,
        writer: None,
    };
    run.stdout = run
        .child
        .stdout
        .take()
        .map(|pipe| bounded_reader(pipe, OUTPUT_LIMIT));
    run.stderr = run
        .child
        .stderr
        .take()
        .map(|pipe| bounded_reader(pipe, OUTPUT_LIMIT));
    run.writer = run
        .child
        .stdin
        .take()
        .map(|pipe| bounded_writer(pipe, stdin_data.to_vec()));

    let mut timed_out = true;
    let mut exit_code = None;
    loop {
        if !keep_running() {
            break;
        }
        match run.child.try_wait() {
            Ok(Some(status)) => {
                timed_out = false;
                exit_code = status.code();
                break;
            }
            Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(20)),
            Ok(None) => break,
            Err(_) => break,
        }
    }
    if timed_out {
        let _ = run.child.kill();
        if let Ok(status) = run.child.wait() {
            exit_code = status.code();
        }
    }

    // Closes the job handle, which terminates any grandchild still alive
    // (including one left over from a killed timeout) before this function
    // returns to its caller.
    run.stop();

    let written = run.writer.take().and_then(PipeWorker::finish) == Some(true);
    let stdout_result = run.stdout.take().and_then(PipeWorker::finish);
    // stderr is intentionally not surfaced: this module never reports raw
    // script text, and reading it out still bounds/drains the pipe.
    let stderr_result = run.stderr.take().and_then(PipeWorker::finish);
    let incomplete = !written || stdout_result.is_none() || stderr_result.is_none();
    let (stdout, stdout_truncated) = stdout_result.unwrap_or_default();
    let stderr_truncated = stderr_result.unwrap_or_default().1;

    BoundedRun {
        stdout,
        timed_out,
        truncated: incomplete || stdout_truncated || stderr_truncated,
        exit_code,
    }
}

fn run_bounded(command: Command, stdin_data: &[u8], time_limit: Duration) -> BoundedRun {
    run_bounded_while(command, stdin_data, time_limit, || true)
}

// ---------------------------------------------------------------------------
// Trusted system paths (never the `SystemRoot`/`WINDIR` environment variable,
// which is an ordinary user-writable-adjacent value, not a privileged one)
// ---------------------------------------------------------------------------

/// The real `System32` directory from the OS itself
/// (`GetSystemDirectoryW`), not the `SystemRoot` environment variable.
fn system32_path() -> Result<PathBuf, ()> {
    let mut buffer = [0u16; 512];
    // SAFETY: `buffer` is a valid, writable `u16` slice; `GetSystemDirectoryW`
    // writes at most its length and returns the number of characters written
    // (excluding the terminator) on success, or the required size (including
    // the terminator) if the buffer was too small. A return at or past the
    // buffer's length is treated as failure below rather than read as if the
    // path fit, so this never reads uninitialized or out-of-bounds memory.
    let len = unsafe { GetSystemDirectoryW(Some(&mut buffer)) };
    if len == 0 || (len as usize) >= buffer.len() {
        return Err(());
    }
    Ok(PathBuf::from(
        String::from_utf16(&buffer[..len as usize]).map_err(|_| ())?,
    ))
}

fn powershell_path(system32: &Path) -> PathBuf {
    system32.join(r"WindowsPowerShell\v1.0\powershell.exe")
}

/// Runs the embedded script with the given action/exe pair and returns the
/// outcome it reported, or a classified failure reason. Never touches
/// `PSModulePath` or any user-writable script file: the interpreter is
/// resolved from the trusted `System32` directory (never `SystemRoot`/
/// `WINDIR`), the script text is piped over stdin, and the script itself
/// replaces `PSModulePath` with a directory derived from that same trusted
/// root before importing anything.
fn run_facts_script(
    action: &str,
    exe: &str,
    requester: Option<(&RequesterHandle, u32)>,
) -> Result<ScriptOutcome, String> {
    let system32 = system32_path().map_err(|()| "system32-unavailable".to_string())?;
    let mut command = Command::new(powershell_path(&system32));
    command
        .creation_flags(windows::Win32::System::Threading::CREATE_NO_WINDOW.0)
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-WindowStyle",
            "Hidden",
            "-ExecutionPolicy",
            "Bypass",
            "-Command",
            "-",
        ])
        .env("ORANGE_FW_ACTION", action)
        .env("ORANGE_FW_EXE", exe)
        .env("ORANGE_FW_SYSTEM32", &system32);
    if let Some((_, pid)) = requester {
        command.env("ORANGE_FW_REQUESTER_PID", pid.to_string());
    }

    let run = if let Some((handle, _)) = requester {
        // The held kernel handle identifies the original requester even if
        // Windows reuses its PID while a policy query is still in progress.
        run_bounded_while(
            command,
            INSPECT_REPAIR_SCRIPT.as_bytes(),
            SCRIPT_TIMEOUT,
            || handle.is_alive(),
        )
    } else {
        run_bounded(command, INSPECT_REPAIR_SCRIPT.as_bytes(), SCRIPT_TIMEOUT)
    };
    if requester.is_some_and(|(handle, _)| !handle.is_alive()) {
        return Ok(ScriptOutcome::RequesterCancelled);
    }
    if run.timed_out {
        return Err("timeout".to_string());
    }
    if run.truncated {
        return Err("truncated".to_string());
    }
    let Some(exit_code) = run.exit_code else {
        return Err("no-exit-code".to_string());
    };
    parse_script_outcome(exit_code, &run.stdout).map_err(str::to_string)
}

/// The exact, normalized path this process was started from: canonicalized
/// (which also normalizes case on Windows) with the `\\?\` verbatim prefix
/// stripped back off, so it reads the way Windows Firewall itself stores and
/// displays a program path.
fn normalized_current_exe() -> Result<String, ()> {
    let raw = std::env::current_exe().map_err(|_| ())?;
    let canonical = std::fs::canonicalize(&raw).unwrap_or(raw);
    // Lossy replacement could turn the executable's name into a different
    // path before elevation or rule matching. Refuse an unrepresentable path.
    let text = canonical.to_str().ok_or(())?;
    Ok(text.strip_prefix(r"\\?\").unwrap_or(text).to_string())
}

// ---------------------------------------------------------------------------
// Public read-only inspection
// ---------------------------------------------------------------------------

pub(crate) fn inspect() -> Inspection {
    let Ok(exe) = normalized_current_exe() else {
        return unknown("Could not determine the running executable's path.");
    };
    match run_facts_script("Inspect", &exe, None) {
        Ok(ScriptOutcome::Facts(facts)) => classify_facts(&facts, &exe),
        Ok(ScriptOutcome::RequesterCancelled | ScriptOutcome::ScriptError(_)) | Err(_) => {
            unknown("Firewall diagnostic data could not be read.")
        }
    }
}

// ---------------------------------------------------------------------------
// Requester liveness, held across the whole elevated repair flow
// ---------------------------------------------------------------------------

/// A held process handle, not a re-opened one: `is_alive` can be called
/// repeatedly (before the read-only inspection, and again immediately before
/// launching the mutating script) and always answers about the exact same
/// kernel process object, immune to the PID being reused by an unrelated
/// process in between - which re-opening by PID each time would not be.
struct RequesterHandle(HANDLE);

impl RequesterHandle {
    fn open_verified(pid: u32, creation: u64) -> Option<Self> {
        let handle = Self::open(pid)?;
        (process_creation(handle.0) == Some(creation)).then_some(handle)
    }

    fn open(pid: u32) -> Option<Self> {
        // SAFETY: `OpenProcess` is called with a plain `u32` PID and a fixed,
        // minimal access right; its documented failure mode is returning an
        // `Err`, handled below without dereferencing anything.
        match unsafe {
            OpenProcess(
                PROCESS_SYNCHRONIZE | PROCESS_QUERY_LIMITED_INFORMATION,
                false,
                pid,
            )
        } {
            Ok(handle) => Some(Self(handle)),
            Err(_) => None,
        }
    }

    fn is_alive(&self) -> bool {
        // SAFETY: `self.0` is a valid, owned process handle for the lifetime
        // of `self`; a zero timeout makes this a non-blocking poll.
        let wait = unsafe { WaitForSingleObject(self.0, 0) };
        // WAIT_TIMEOUT means the handle is not yet signalled, i.e. the
        // process is still running. Both WAIT_OBJECT_0 (already exited) and
        // WAIT_FAILED (could not be confirmed) must resolve to "not alive":
        // a cancellation must never be missed because a wait happened to
        // fail rather than definitively answer.
        wait == WAIT_TIMEOUT
    }
}

impl Drop for RequesterHandle {
    fn drop(&mut self) {
        // SAFETY: `self.0` is a handle this `RequesterHandle` uniquely owns
        // and has not yet closed.
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}

fn process_creation(handle: HANDLE) -> Option<u64> {
    let mut created = FILETIME::default();
    let mut exited = FILETIME::default();
    let mut kernel = FILETIME::default();
    let mut user = FILETIME::default();
    // SAFETY: all output pointers refer to initialized FILETIME values valid
    // for the call. The handle is owned by RequesterHandle or is our own
    // process pseudo-handle; an invalid/denied handle produces an error.
    unsafe { GetProcessTimes(handle, &mut created, &mut exited, &mut kernel, &mut user) }.ok()?;
    Some((u64::from(created.dwHighDateTime) << 32) | u64::from(created.dwLowDateTime))
}

// ---------------------------------------------------------------------------
// Elevation
// ---------------------------------------------------------------------------

fn launch_elevated_and_wait(exe: &str, requester_pid: u32) -> i32 {
    // SAFETY: GetCurrentProcess returns a non-owning pseudo-handle for this
    // process. It is valid here and must not be closed by RequesterHandle.
    let Some(created) = process_creation(unsafe { GetCurrentProcess() }) else {
        return REPAIR_FAILED;
    };
    let operation = HSTRING::from("runas");
    let file = HSTRING::from(exe);
    let parameters = HSTRING::from(format!(
        "repair-network --elevated --requester {requester_pid} --requester-created {created}"
    ));

    let mut info = SHELLEXECUTEINFOW {
        cbSize: std::mem::size_of::<SHELLEXECUTEINFOW>() as u32,
        fMask: SEE_MASK_NOCLOSEPROCESS,
        hwnd: HWND::default(),
        lpVerb: PCWSTR(operation.as_ptr()),
        lpFile: PCWSTR(file.as_ptr()),
        lpParameters: PCWSTR(parameters.as_ptr()),
        lpDirectory: PCWSTR::null(),
        nShow: SW_HIDE.0,
        hInstApp: HINSTANCE::default(),
        lpIDList: std::ptr::null_mut(),
        lpClass: PCWSTR::null(),
        hkeyClass: HKEY::default(),
        dwHotKey: 0,
        Anonymous: Default::default(),
        hProcess: HANDLE::default(),
    };

    // SAFETY: `info` is a fully-initialized `SHELLEXECUTEINFOW` with `cbSize`
    // set to its own size as required, and it outlives this call.
    if let Err(error) = unsafe { ShellExecuteExW(&mut info) } {
        // ERROR_CANCELLED is the standard UAC-declined signal (documented as
        // "The operation was canceled by the user", Win32 code 1223).
        return if error.code() == windows::core::HRESULT::from_win32(ERROR_CANCELLED.0) {
            REPAIR_UAC_DECLINED
        } else {
            REPAIR_FAILED
        };
    }
    if info.hProcess.is_invalid() {
        return REPAIR_FAILED;
    }

    // SAFETY: `info.hProcess` was just returned by `ShellExecuteExW` with
    // `SEE_MASK_NOCLOSEPROCESS` set and has not been closed yet.
    let wait = unsafe { WaitForSingleObject(info.hProcess, ELEVATION_WAIT_MS) };
    let mut code: u32 = 0;
    // SAFETY: `info.hProcess` is still the same valid, owned handle.
    let got_code = wait.0 == 0 && unsafe { GetExitCodeProcess(info.hProcess, &mut code) }.is_ok();
    // SAFETY: `info.hProcess` is owned by this function (via
    // `SEE_MASK_NOCLOSEPROCESS`) and is closed exactly once, here.
    unsafe {
        let _ = CloseHandle(info.hProcess);
    }
    if !got_code {
        return REPAIR_FAILED;
    }
    i32::try_from(code).unwrap_or(REPAIR_FAILED)
}

// ---------------------------------------------------------------------------
// Repair orchestration
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RepairAction {
    NothingToChange,
    NotRepairable,
    Proceed,
}

fn plan_repair(inspection: &Inspection) -> RepairAction {
    match inspection.status {
        FirewallStatus::Clear => RepairAction::NothingToChange,
        FirewallStatus::Blocked if inspection.repairable => RepairAction::Proceed,
        FirewallStatus::Blocked | FirewallStatus::Managed | FirewallStatus::Unknown => {
            RepairAction::NotRepairable
        }
    }
}

/// Maps the post-mutation facts back through the same classifier used for
/// read-only inspection: a repair is only ever reported as having changed or
/// verified a setting if the block it targeted is actually gone afterward.
/// Never mutates anything itself - this only reads the classification that
/// `classify_facts` already computed from the script's post-mutation scan.
fn repair_outcome_code(post: &Inspection) -> i32 {
    match post.status {
        FirewallStatus::Clear => REPAIR_CHANGED_OR_VERIFIED,
        _ => REPAIR_FAILED,
    }
}

pub(crate) fn repair(
    elevated: bool,
    requester: Option<u32>,
    requester_created: Option<u64>,
) -> i32 {
    if elevated {
        repair_elevated(requester, requester_created)
    } else {
        repair_unelevated()
    }
}

fn repair_unelevated() -> i32 {
    let current = inspect();
    match plan_repair(&current) {
        RepairAction::NothingToChange => REPAIR_NOTHING_TO_CHANGE,
        RepairAction::NotRepairable => REPAIR_NOT_LOCALLY_REPAIRABLE,
        RepairAction::Proceed => {
            let Ok(exe) = normalized_current_exe() else {
                return REPAIR_FAILED;
            };
            let requester_pid = std::process::id();
            launch_elevated_and_wait(&exe, requester_pid)
        }
    }
}

fn repair_elevated(requester: Option<u32>, requester_created: Option<u64>) -> i32 {
    let (Some(requester_pid), Some(created)) = (requester, requester_created) else {
        return REPAIR_REQUESTER_CANCELLED;
    };
    let Some(handle) = RequesterHandle::open_verified(requester_pid, created) else {
        return REPAIR_REQUESTER_CANCELLED;
    };
    if !handle.is_alive() {
        return REPAIR_REQUESTER_CANCELLED;
    }

    let current = inspect();
    match plan_repair(&current) {
        RepairAction::NothingToChange => REPAIR_NOTHING_TO_CHANGE,
        RepairAction::NotRepairable => REPAIR_NOT_LOCALLY_REPAIRABLE,
        RepairAction::Proceed => {
            // Re-checked, on the same held handle, immediately before
            // mutation: the requester may have exited (or the user may have
            // cancelled the UI) during the read-only inspection above, or
            // during however long the UAC prompt itself was left open.
            if !handle.is_alive() {
                return REPAIR_REQUESTER_CANCELLED;
            }
            let Ok(exe) = normalized_current_exe() else {
                return REPAIR_FAILED;
            };
            match run_facts_script("Repair", &exe, Some((&handle, requester_pid))) {
                Ok(ScriptOutcome::Facts(facts)) => {
                    repair_outcome_code(&classify_facts(&facts, &exe))
                }
                Ok(ScriptOutcome::RequesterCancelled) => REPAIR_REQUESTER_CANCELLED,
                Ok(ScriptOutcome::ScriptError(_)) | Err(_) => REPAIR_FAILED,
            }
        }
    }
}

#[cfg(test)]
#[path = "firewall_tests.rs"]
mod tests;
