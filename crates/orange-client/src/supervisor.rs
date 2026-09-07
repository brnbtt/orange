//! Supervising the streaming processes.
//!
//! The client runs `orange host` and `orange watch` as child processes rather
//! than driving the pipeline in-process. GPUI runs its own executor and the
//! pipeline runs on tokio, so in-process would mean reconciling two runtimes;
//! more importantly, a crash in the media pipeline should not take the UI down
//! with it.
//!
//! Communication is one-way and line-based: we read the child's stdout and
//! look for the handful of things the UI needs to know.

use crate::session::Friend;
use anyhow::{Context, Result};
use serde::Deserialize;
use std::io::{BufRead, BufReader, Read};
use std::os::windows::process::CommandExt;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};

/// Child processes are console applications; without this each one flashes a
/// black window in front of the user.
const CREATE_NO_WINDOW: u32 = 0x0800_0000;
const MAX_DIAGNOSTIC_FILES: usize = 64;
const MAX_DIAGNOSTIC_BYTES: u64 = 256 * 1024 * 1024;

#[derive(Debug, Clone, Deserialize)]
pub struct WindowTarget {
    pub hwnd: i64,
    pub title: String,
    pub process: String,
    pub width: i32,
    pub height: i32,
}

impl WindowTarget {
    /// "Chrome" from "chrome.exe", for a tidier list.
    pub fn app_name(&self) -> String {
        let stem = self
            .process
            .strip_suffix(".exe")
            .unwrap_or(&self.process)
            .to_string();
        let mut chars = stem.chars();
        match chars.next() {
            Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
            None => stem,
        }
    }
}

/// Path to the `orange` binary, assumed to sit beside the client executable.
fn orange_exe() -> Result<std::path::PathBuf> {
    let dir = std::env::current_exe()?
        .parent()
        .context("no parent directory")?
        .to_path_buf();
    Ok(dir.join("orange.exe"))
}

/// Locate GStreamer's `bin` directory.
///
/// `orange.exe` links GStreamer dynamically, so those DLLs must be on the
/// PATH of the *child* process or it dies at load time with a
/// "gstreamer-1.0-0.dll was not found" dialog before `main` ever runs. The
/// client itself has no GStreamer dependency, which is why it starts fine and
/// only the child fails.
fn gstreamer_bin() -> Option<std::path::PathBuf> {
    // The copy installed beside us wins over everything else. It is the exact
    // tree this build was packaged and verified against, whereas a machine-wide
    // GStreamer is whatever version that machine happens to have - possibly
    // older than the plugins we need, possibly newer and differently named.
    // Preferring ours means a developer's system install cannot mask a gap in
    // the bundle either.
    if let Some(bundled) = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(|dir| dir.join(r"gstreamer\bin")))
        .filter(|bin| bin.is_dir())
    {
        return Some(bundled);
    }

    // Then an explicit root, since that is what the dev shell sets and dev
    // builds have no bundle beside them.
    if let Ok(root) = std::env::var("GSTREAMER_1_0_ROOT_MSVC_X86_64") {
        let bin = std::path::PathBuf::from(root).join("bin");
        if bin.is_dir() {
            return Some(bin);
        }
    }

    let candidates = [
        std::env::var("LOCALAPPDATA")
            .ok()
            .map(|p| std::path::PathBuf::from(p).join(r"Programs\gstreamer\1.0\msvc_x86_64\bin")),
        Some(std::path::PathBuf::from(
            r"C:\gstreamer\1.0\msvc_x86_64\bin",
        )),
        std::env::var("ProgramFiles")
            .ok()
            .map(|p| std::path::PathBuf::from(p).join(r"gstreamer\1.0\msvc_x86_64\bin")),
    ];
    candidates.into_iter().flatten().find(|p| p.is_dir())
}

/// Build a command for the `orange` binary with GStreamer reachable.
pub(super) fn orange_command() -> Result<Command> {
    let mut command = Command::new(orange_exe()?);

    if let Some(bin) = gstreamer_bin() {
        let existing = std::env::var("PATH").unwrap_or_default();
        command.env("PATH", format!("{};{}", bin.display(), existing));
    }
    // Whole-screen sharing captures every sound the machine makes. Ours are
    // meant for the person hosting, so the child is told which process tree to
    // leave out.
    command.env("ORANGE_UI_PID", std::process::id().to_string());
    if std::env::var_os("ORANGE_MEDIA_DIAGNOSTICS").is_none() {
        if let Some(directory) = diagnostics_directory() {
            prune_diagnostics(&directory);
            command.env("ORANGE_MEDIA_DIAGNOSTICS", directory);
            command.env("ORANGE_TEST_PROFILE", "beta");
            if let Some(build) = option_env!("ORANGE_BUILD_ID") {
                command.env("ORANGE_BUILD_ID", build);
            }
        }
    }
    command.creation_flags(CREATE_NO_WINDOW);
    Ok(command)
}

/// Where child processes write their JSONL media diagnostics.
pub fn diagnostics_directory() -> Option<std::path::PathBuf> {
    std::env::var_os("LOCALAPPDATA").map(|local| {
        std::path::PathBuf::from(local)
            .join("orange")
            .join("diagnostics")
    })
}

/// Reveal a folder so a tester can attach its contents to a bug report.
///
/// Uses `explorer` rather than `ShellExecuteW` to keep this free of `unsafe`.
/// Explorer's exit code is unreliable, so the spawn is not waited on.
pub fn open_directory(path: &std::path::Path) -> Result<()> {
    std::fs::create_dir_all(path)
        .with_context(|| format!("could not create {}", path.display()))?;
    Command::new("explorer")
        .arg(path)
        .spawn()
        .with_context(|| format!("could not open {}", path.display()))?;
    Ok(())
}

fn prune_diagnostics(directory: &std::path::Path) {
    let Ok(entries) = std::fs::read_dir(directory) else {
        return;
    };
    let retention = std::time::Duration::from_secs(7 * 24 * 60 * 60);
    let mut files = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        let matches = path
            .extension()
            .is_some_and(|extension| extension == "jsonl")
            && path
                .file_name()
                .is_some_and(|name| name.to_string_lossy().starts_with("orange-media-"));
        if !matches {
            continue;
        }
        let Ok(metadata) = entry.metadata() else {
            continue;
        };
        let modified = metadata.modified().unwrap_or(std::time::UNIX_EPOCH);
        if modified.elapsed().is_ok_and(|age| age > retention) {
            let _ = std::fs::remove_file(path);
        } else {
            files.push((modified, metadata.len(), path));
        }
    }
    files.sort_by_key(|(modified, _, _)| *modified);
    let mut bytes = files.iter().map(|(_, size, _)| size).sum::<u64>();
    let mut count = files.len();
    for (_, size, path) in files {
        if count <= MAX_DIAGNOSTIC_FILES && bytes <= MAX_DIAGNOSTIC_BYTES {
            break;
        }
        if std::fs::remove_file(path).is_ok() {
            count -= 1;
            bytes = bytes.saturating_sub(size);
        }
    }
}

/// Whether the media stack is present, so the UI can say so plainly rather
/// than letting Windows show a DLL error dialog.
pub fn gstreamer_available() -> bool {
    gstreamer_bin().is_some()
}

/// Shown when the media runtime cannot be found. It ships inside the installer,
/// so on a released build its absence means the installation is damaged rather
/// than that anything is missing from the machine.
pub const MEDIA_RUNTIME_MISSING: &str =
    "The media runtime that ships with orange is missing. Reinstalling orange will restore it.";

pub fn list_windows() -> Result<Vec<WindowTarget>> {
    list_windows_cancelled(&AtomicBool::new(false))
}

fn list_windows_cancelled(cancel: &AtomicBool) -> Result<Vec<WindowTarget>> {
    let mut command = orange_command()?;
    command.args(["list", "--json"]);
    list_windows_command(command, cancel, Duration::from_secs(10))
}

pub(super) fn picker_windows(cancel: &AtomicBool) -> Result<Vec<WindowTarget>> {
    let mut windows = list_windows_cancelled(cancel)?;
    windows.retain(|window| !window.process.to_lowercase().starts_with("orange"));
    let (width, height) = crate::capture::screen_size().unwrap_or((0, 0));
    // Zero is the existing whole-screen sentinel, not a capturable HWND.
    windows.insert(
        0,
        WindowTarget {
            hwnd: 0,
            title: "Entire screen".into(),
            process: "Desktop".into(),
            width,
            height,
        },
    );
    Ok(windows)
}

// Bounded command owner: cancellation/timeout kills and reaps the child, and
// always joins both pipe readers before returning.
struct BoundedCommandChild {
    child: Child,
    readers: Vec<std::thread::JoinHandle<()>>,
    output_name: &'static str,
    reap_name: &'static str,
}

impl BoundedCommandChild {
    fn finish_readers(&mut self) {
        for reader in self.readers.drain(..) {
            crate::background::join_background_worker(
                reader,
                Duration::from_secs(5),
                self.output_name,
            );
        }
    }
}

impl Drop for BoundedCommandChild {
    fn drop(&mut self) {
        if !matches!(self.child.try_wait(), Ok(Some(_))) {
            let _ = self.child.kill();
            if let Err(error) = self.child.wait() {
                crate::client::fail_fast(self.reap_name, &error.into());
            }
        }
        self.finish_readers();
    }
}

#[derive(Debug)]
pub(super) struct BoundedCommandOutput {
    pub status: std::process::ExitStatus,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

pub(super) fn run_bounded_command(
    mut command: Command,
    cancel: &AtomicBool,
    timeout: Duration,
    max_output: u64,
    label: &'static str,
) -> Result<BoundedCommandOutput> {
    let child = command
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("could not run {label}"))?;
    let mut owner = BoundedCommandChild {
        child,
        readers: Vec::new(),
        output_name: label,
        reap_name: "could not reap bounded child",
    };
    let (sender, receiver) = mpsc::channel();
    let stdout = owner
        .child
        .stdout
        .take()
        .context("missing command stdout")?;
    let stderr = owner
        .child
        .stderr
        .take()
        .context("missing command stderr")?;
    let out_sender = sender.clone();
    owner.readers.push(std::thread::spawn(move || {
        let mut bytes = Vec::new();
        let result = stdout
            .take(max_output + 1)
            .read_to_end(&mut bytes)
            .map(|_| bytes);
        let _ = out_sender.send((true, result));
    }));
    owner.readers.push(std::thread::spawn(move || {
        let mut bytes = Vec::new();
        let result = stderr
            .take(max_output + 1)
            .read_to_end(&mut bytes)
            .map(|_| bytes);
        let _ = sender.send((false, result));
    }));
    let deadline = Instant::now() + timeout;
    let status = loop {
        anyhow::ensure!(!cancel.load(Ordering::Acquire), "{label} cancelled");
        anyhow::ensure!(Instant::now() < deadline, "{label} timed out");
        if let Some(status) = owner.child.try_wait()? {
            break status;
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    owner.finish_readers();
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    for (is_stdout, result) in receiver {
        let bytes = result?;
        anyhow::ensure!(bytes.len() as u64 <= max_output, "{label} output too large");
        if is_stdout {
            stdout = bytes;
        } else {
            stderr = bytes;
        }
    }
    Ok(BoundedCommandOutput {
        status,
        stdout,
        stderr,
    })
}

fn list_windows_command(
    command: Command,
    cancel: &AtomicBool,
    timeout: Duration,
) -> Result<Vec<WindowTarget>> {
    const MAX_OUTPUT: u64 = 4 * 1024 * 1024;
    // Read simultaneously: waiting first can deadlock if a large list fills
    // stdout, or a loader failure fills stderr. Each buffer is capped.
    let output = run_bounded_command(command, cancel, timeout, MAX_OUTPUT, "window enumeration")?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!(
            "`orange list` exited with {}: {}",
            output.status,
            stderr.trim()
        );
    }
    let text = String::from_utf8_lossy(&output.stdout);
    // The binary prints nothing else on stdout in JSON mode, but be forgiving.
    let json = text
        .lines()
        .find(|l| l.trim_start().starts_with('['))
        .unwrap_or("[]");
    Ok(serde_json::from_str(json)?)
}

/// Kick off `orange login`, which opens the browser and writes the session
/// file when it completes. The client notices by watching for that file.
///
/// The server must be passed explicitly: the binary's default points at
/// localhost, which is not where the relay lives.
///
/// The child is returned so the caller can tell "still waiting for the user"
/// apart from "it died", which otherwise looks identical from the UI.
pub fn start_login(server: &str) -> Result<LoginAttempt> {
    let mut child = orange_command()?
        .arg("login")
        .args(["--server", server])
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .context("could not start `orange login`")?;
    let last_stderr = Arc::new(Mutex::new(String::new()));
    let stderr_reader = child.stderr.take().map(|stderr| {
        let last_stderr = last_stderr.clone();
        std::thread::spawn(move || {
            for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                if !line.trim().is_empty() {
                    if let Ok(mut last) = last_stderr.lock() {
                        *last = line;
                    }
                }
            }
        })
    });
    Ok(LoginAttempt {
        child,
        last_stderr,
        stderr_reader,
    })
}

pub struct LoginAttempt {
    child: Child,
    last_stderr: Arc<Mutex<String>>,
    stderr_reader: Option<std::thread::JoinHandle<()>>,
}

impl LoginAttempt {
    /// `None` while still running; `Some(reason)` once it has exited without
    /// producing a session.
    pub fn failure(&mut self) -> Option<String> {
        match self.child.try_wait() {
            Ok(Some(status)) => {
                self.finish_stderr_reader();
                let reason = self
                    .last_stderr
                    .lock()
                    .map(|reason| reason.clone())
                    .unwrap_or_default();
                Some(if reason.is_empty() {
                    if status.success() {
                        "Login was cancelled or timed out".to_string()
                    } else {
                        format!("Login process exited with {status}")
                    }
                } else {
                    reason
                })
            }
            Ok(None) => None,
            Err(error) => Some(format!("Could not check login status: {error}")),
        }
    }

    fn finish_stderr_reader(&mut self) {
        if let Some(reader) = self.stderr_reader.take() {
            let _ = reader.join();
        }
    }
}

impl Drop for LoginAttempt {
    fn drop(&mut self) {
        if !matches!(self.child.try_wait(), Ok(Some(_))) {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
        self.finish_stderr_reader();
    }
}

/// What the UI has learned from a running child process.
#[derive(Debug, Default, Clone)]
pub struct StreamStatus {
    pub code: Option<String>,
    pub viewers: Vec<String>,
    pub error: Option<String>,
    pub notice: Option<String>,
    /// Set when a watched stream finished because the host stopped, as opposed
    /// to the viewer closing their own window. Both exit zero.
    pub ended: bool,
    /// Who this session put us in contact with, if they were signed in: the
    /// host when watching, joining viewers when hosting. The client offers to
    /// keep them; it never adds them on its own.
    pub met: Vec<Friend>,
    viewer_labels: Vec<(String, String)>,
}

pub struct Supervisor {
    child: Child,
    pub status: Arc<Mutex<StreamStatus>>,
    readers: Vec<std::thread::JoinHandle<()>>,
}

impl Supervisor {
    /// Start `orange host` for a window and begin parsing its output.
    pub fn host(
        target: &WindowTarget,
        quality: &Quality,
        fps: u32,
        server: &str,
        visible_to: &[String],
    ) -> Result<Self> {
        let (width, height) = quality.fit(target);
        let mut command = orange_command()?;
        command
            .arg("host")
            .args(["--hwnd", &target.hwnd.to_string()])
            .args(["--server", server])
            // Every tier lets the child pick the best encoder its GPU offers.
            .args(["--codec", "auto"])
            // Always explicit. Omitting it lets the child fall back to the
            // display's refresh rate, which is what the cap exists to avoid.
            .args(["--fps", &fps.to_string()])
            .args(["--scale", &format!("{width}x{height}")]);
        // Omitted entirely when empty: clap would read `--visible-to` with no
        // value as the start of the next flag.
        if !visible_to.is_empty() {
            command.args(["--visible-to", &visible_to.join(",")]);
        }
        Self::spawn(command)
    }

    /// Start an ordinary friend-viewer playback session.
    pub fn watch(code: &str, server: &str, cascade: usize) -> Result<Self> {
        Self::playback(code, server, cascade, "friend")
    }

    fn playback(code: &str, server: &str, cascade: usize, profile: &str) -> Result<Self> {
        let mut command = orange_command()?;
        command
            .arg("watch")
            .args(["--code", code])
            .args(["--server", server])
            .args(["--cascade", &(cascade % 6).to_string()])
            .args(["--profile", profile]);
        Self::spawn(command)
    }

    fn spawn(mut command: Command) -> Result<Self> {
        let mut child = command
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .context("could not start the orange binary")?;

        let status = Arc::new(Mutex::new(StreamStatus::default()));
        let mut readers = Vec::with_capacity(2);

        if let Some(stdout) = child.stdout.take() {
            let status = status.clone();
            readers.push(std::thread::spawn(move || {
                for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                    parse_line(&line, &status);
                }
            }));
        }
        if let Some(stderr) = child.stderr.take() {
            let status = status.clone();
            readers.push(std::thread::spawn(move || {
                for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                    if line.contains("Error") || line.contains("error") {
                        if let Ok(mut status) = status.lock() {
                            status.error = Some(line);
                        }
                    }
                }
            }));
        }

        Ok(Self {
            child,
            status,
            readers,
        })
    }

    pub fn running(&mut self) -> bool {
        match self.child.try_wait() {
            Ok(None) => true,
            Ok(Some(status)) => {
                self.finish_readers();
                if !status.success() {
                    if let Ok(mut stream) = self.status.lock() {
                        if stream.error.is_none() {
                            stream.error = Some(format!("Orange exited with {status}"));
                        }
                    }
                }
                false
            }
            Err(error) => {
                if let Ok(mut stream) = self.status.lock() {
                    stream.error = Some(format!("Could not inspect Orange: {error}"));
                }
                self.stop();
                false
            }
        }
    }

    pub fn stop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        self.finish_readers();
    }

    fn finish_readers(&mut self) {
        for reader in self.readers.drain(..) {
            let _ = reader.join();
        }
    }
}

impl Drop for Supervisor {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(test)]
pub(crate) fn friend_test_child(wait: bool) -> Supervisor {
    let mut command = Command::new(std::env::current_exe().unwrap());
    command
        .args([
            "--exact",
            "supervisor::tests::friend_profile_child",
            "--nocapture",
        ])
        .env(
            "ORANGE_TEST_FRIEND_CHILD",
            if wait { "wait" } else { "exit" },
        );
    Supervisor::spawn(command).unwrap()
}

/// Printed by `orange watch` when the host stopped, mirroring the constant in
/// `crates/orange/src/peer/watch.rs`. A watch child exits zero both when the
/// stream ends and when the viewer closes their own window, so the exit code
/// cannot tell them apart and this line has to.
const WATCH_ENDED: &str = "[watch-status] ended";

/// The Discord identity of whoever this session put us in contact with: the
/// host when watching, the newest viewer when hosting.
///
/// Anonymous peers produce nothing. Without an id there is no stable way to
/// recognise the same person again, so there is nothing worth offering to keep;
/// `peer` is a routing id the relay reassigns every session.
fn parse_profile(record: &serde_json::Value) -> Option<Friend> {
    let id = record.get("id")?.as_str()?;
    if id.is_empty() {
        return None;
    }
    Some(Friend {
        id: id.to_string(),
        name: record
            .get("name")
            .and_then(|value| value.as_str())
            .or_else(|| record.get("label").and_then(|value| value.as_str()))
            .unwrap_or(id)
            .to_string(),
        avatar_url: record
            .get("avatar_url")
            .and_then(|value| value.as_str())
            .map(str::to_string),
    })
}

/// Extract the few facts the UI cares about from the child's log lines.
fn parse_line(line: &str, status: &Arc<Mutex<StreamStatus>>) {
    let Ok(mut status) = status.lock() else {
        return;
    };

    if let Some(rest) = line.split("Share this code:").nth(1) {
        status.code = Some(rest.trim().to_string());
    } else if line == WATCH_ENDED {
        status.ended = true;
    } else if let Some(record) = line.strip_prefix("[quality-status] ") {
        let Ok(record) = serde_json::from_str::<serde_json::Value>(record) else {
            return;
        };
        if record.get("event").and_then(|value| value.as_str())
            == Some("automatic-bitrate-constrained")
        {
            status.notice = record
                .get("message")
                .and_then(|value| value.as_str())
                .map(str::to_string);
        }
    } else if let Some(record) = line.strip_prefix("[watch-host] ") {
        let Ok(record) = serde_json::from_str::<serde_json::Value>(record) else {
            return;
        };
        if let Some(friend) = parse_profile(&record) {
            crate::session::remember_friend(&mut status.met, friend);
        }
    } else if let Some(record) = line.strip_prefix("[host-status] ") {
        let Ok(record) = serde_json::from_str::<serde_json::Value>(record) else {
            return;
        };
        let Some(event) = record.get("event").and_then(|value| value.as_str()) else {
            return;
        };
        let Some(peer) = record.get("peer").and_then(|value| value.as_str()) else {
            return;
        };
        match event {
            "joined" => {
                let Some(label) = record.get("label").and_then(|value| value.as_str()) else {
                    return;
                };
                // Only an authenticated viewer carries an id, and only an id
                // is worth offering to keep: `peer` is reassigned per session.
                if let Some(friend) = parse_profile(&record) {
                    crate::session::remember_friend(&mut status.met, friend);
                }
                if !status.viewer_labels.iter().any(|(id, _)| id == peer) {
                    status
                        .viewer_labels
                        .push((peer.to_string(), label.to_string()));
                }
            }
            "left" => status.viewer_labels.retain(|(id, _)| id != peer),
            _ => return,
        }
        status.viewers = status
            .viewer_labels
            .iter()
            .map(|(_, label)| label.clone())
            .collect();
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Quality {
    pub label: &'static str,
    pub detail: &'static str,
    pub max_width: u32,
    pub max_height: u32,
}

impl Quality {
    /// Fit the source inside this tier without changing its ratio.
    ///
    /// The factor is clamped at 1.0, so a tier is a ceiling and never an
    /// upscale. Choosing 4K on a 1080p window encodes 1080p, because upscaling
    /// invents no detail and only costs bits.
    pub fn fit(&self, target: &WindowTarget) -> (u32, u32) {
        let source_w = target.width.max(2) as f32;
        let source_h = target.height.max(2) as f32;
        let factor = (self.max_width as f32 / source_w)
            .min(self.max_height as f32 / source_h)
            .min(1.0);
        let even = |value: f32| ((value.round() as u32).max(2) / 2) * 2;
        (even(source_w * factor), even(source_h * factor))
    }
}

pub const QUALITIES: &[Quality] = &[
    Quality {
        label: "720p",
        detail: "Soft on a large screen. Lightest to send",
        max_width: 1280,
        max_height: 720,
    },
    Quality {
        label: "1080p",
        detail: "Crisp on most screens",
        max_width: 1920,
        max_height: 1080,
    },
    Quality {
        label: "1440p",
        detail: "Crisp on large or high-DPI screens",
        max_width: 2560,
        max_height: 1440,
    },
    Quality {
        // The ceiling only needs saying on the tier people are afraid to pick.
        label: "2160p",
        detail: "Most detail. Never upscales a smaller window",
        max_width: 3840,
        max_height: 2160,
    },
];

/// Frame-rate choices, capped at 120.
///
/// "Auto" used to lead this list and follow the captured display's refresh
/// rate. On a 180 Hz panel that meant a 180 fps capture, and a measured session
/// showed the encoder delivering 53.6 fps of it with zero packets lost in
/// transit, plus 123 decoder keyframe requests on intact data. There is no rate
/// above 120 worth offering, and so nothing left for Auto to choose between.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameRate {
    pub fps: u32,
    pub label: &'static str,
    pub detail: &'static str,
}

pub const FRAME_RATES: &[FrameRate] = &[
    FrameRate {
        fps: 60,
        label: "60",
        detail: "Works on every display",
    },
    FrameRate {
        fps: 120,
        label: "120",
        detail: "Needs a 120 Hz display. Heavier to encode",
    },
];

/// The rate to fall back to when a preferences file names one that is gone.
pub const DEFAULT_FPS: u32 = 60;

/// Resolve a stored frame rate to one that is actually offered.
///
/// `None` is Auto, which was the default every build before the cap wrote, so
/// most preferences files hold it. 240 shipped once as an explicit choice.
/// Without this the picker shows nothing selected and no description, and the
/// display's own refresh rate still reaches the encoder.
pub fn supported_frame_rate(fps: Option<u32>) -> u32 {
    fps.filter(|value| FRAME_RATES.iter().any(|rate| rate.fps == *value))
        .unwrap_or(DEFAULT_FPS)
}

#[cfg(test)]
#[path = "supervisor_list_tests.rs"]
mod list_tests;

#[cfg(test)]
mod tests {
    use super::*;

    fn target(width: i32, height: i32) -> WindowTarget {
        WindowTarget {
            hwnd: 1,
            title: String::new(),
            process: String::new(),
            width,
            height,
        }
    }

    #[test]
    fn every_caption_fits_one_line_and_reads_as_a_caption() {
        // A card's inner width is ~296px, and 12px Segoe UI runs about 5.8px
        // per character, so anything past this wraps to a second line. That is
        // not cosmetic: a wrapped caption changes the card's height, which
        // shoves every card below it while a fade is still running.
        const BUDGET: usize = 48;
        let captions = QUALITIES
            .iter()
            .map(|q| ("resolution", q.label, q.detail))
            .chain(
                FRAME_RATES
                    .iter()
                    .map(|r| ("frame rate", r.label, r.detail)),
            );

        for (card, label, detail) in captions {
            assert!(
                detail.len() <= BUDGET,
                "{card}/{label}: {} chars, over the {BUDGET} budget: {detail:?}",
                detail.len()
            );
            assert!(
                !detail.ends_with('.'),
                "{card}/{label}: captions do not take a terminal period: {detail:?}"
            );
            assert!(!detail.is_empty(), "{card}/{label} has no caption");
        }
    }

    #[test]
    fn a_frame_rate_that_is_no_longer_offered_falls_back_to_the_default() {
        // Auto (None) was the default until the cap, so nearly every installed
        // client has it in preferences.json, and 240 shipped as an explicit
        // choice before that.
        assert_eq!(supported_frame_rate(None), DEFAULT_FPS);
        assert_eq!(supported_frame_rate(Some(240)), DEFAULT_FPS);
        assert_eq!(supported_frame_rate(Some(180)), DEFAULT_FPS);
        assert_eq!(supported_frame_rate(Some(60)), 60);
        assert_eq!(supported_frame_rate(Some(120)), 120);
        // Every offered rate survives the filter, so adding one cannot silently
        // become unselectable.
        for rate in FRAME_RATES {
            assert_eq!(
                supported_frame_rate(Some(rate.fps)),
                rate.fps,
                "{}",
                rate.label
            );
        }
    }

    #[test]
    fn the_picker_offers_two_rates_and_neither_is_above_the_cap() {
        // The whole point of the list. An entry above 120, or one that follows
        // the display again, puts the 180 fps reports straight back.
        assert_eq!(
            FRAME_RATES.iter().map(|rate| rate.fps).collect::<Vec<_>>(),
            [60, 120]
        );
        assert!(FRAME_RATES.iter().any(|rate| rate.fps == DEFAULT_FPS));
    }

    #[test]
    fn quality_bounds_preserve_source_shape() {
        assert_eq!(QUALITIES[1].fit(&target(2002, 1804)), (1198, 1080));
        assert_eq!(QUALITIES[1].fit(&target(3440, 1440)), (1920, 804));
        assert_eq!(QUALITIES[1].fit(&target(1280, 720)), (1280, 720));
    }

    #[test]
    fn a_tier_is_a_ceiling_and_never_upscales() {
        // A 1080p source under the 4K tier encodes 1080p. Choosing a tier above
        // your display costs nothing, because upscaling invents no detail.
        let source = target(1920, 1080);
        assert_eq!(QUALITIES[3].fit(&source), (1920, 1080));
        assert_eq!(QUALITIES[1].fit(&source), (1920, 1080));
    }

    #[test]
    fn the_bundled_media_runtime_wins_over_one_installed_on_the_machine() {
        // The bundle is the exact tree this build was packaged and verified
        // against. A machine-wide GStreamer is whatever version that computer
        // happens to have, so letting it win turns a working install into a
        // failure that only reproduces on someone else's machine - and hides
        // a gap in the bundle from every developer who has GStreamer locally.
        let exe_directory = std::env::current_exe()
            .unwrap()
            .parent()
            .unwrap()
            .to_path_buf();
        let bundled = exe_directory.join("gstreamer").join("bin");
        std::fs::create_dir_all(&bundled).unwrap();

        let machine_wide = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(machine_wide.path().join("bin")).unwrap();
        std::env::set_var("GSTREAMER_1_0_ROOT_MSVC_X86_64", machine_wide.path());
        let chosen = gstreamer_bin();
        std::env::remove_var("GSTREAMER_1_0_ROOT_MSVC_X86_64");
        std::fs::remove_dir_all(exe_directory.join("gstreamer")).ok();

        assert_eq!(chosen.as_deref(), Some(bundled.as_path()));
    }

    #[test]
    fn beta_diagnostics_have_an_aggregate_file_quota() {
        let directory = tempfile::tempdir().unwrap();
        for index in 0..70 {
            std::fs::write(
                directory.path().join(format!("orange-media-{index}.jsonl")),
                b"x",
            )
            .unwrap();
        }
        std::fs::write(directory.path().join("keep.txt"), b"keep").unwrap();

        prune_diagnostics(directory.path());

        assert_eq!(
            std::fs::read_dir(directory.path())
                .unwrap()
                .flatten()
                .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "jsonl"))
                .count(),
            MAX_DIAGNOSTIC_FILES
        );
        assert!(directory.path().join("keep.txt").is_file());
    }

    #[test]
    fn a_stream_ending_is_reported_separately_from_a_stream_breaking() {
        // Both exit zero, so without this marker the client cannot tell the host
        // stopping from the viewer closing their own window - and it used to
        // guess, by grepping stderr for the substring "error".
        let status = Arc::new(Mutex::new(StreamStatus::default()));
        parse_line("[watch] joining ABC-123...", &status);
        assert!(!status.lock().unwrap().ended);

        parse_line(WATCH_ENDED, &status);
        let status = status.lock().unwrap();
        assert!(status.ended);
        assert!(status.error.is_none(), "an ending is not a failure");
    }

    #[test]
    fn host_status_removes_the_joined_display_name() {
        let status = Arc::new(Mutex::new(StreamStatus::default()));
        parse_line(
            r#"[host-status] {"event":"joined","peer":"a","label":"orange"}"#,
            &status,
        );
        parse_line(
            r#"[host-status] {"event":"left","peer":"a","label":"orange"}"#,
            &status,
        );

        assert!(status.lock().unwrap().viewers.is_empty());
    }

    #[test]
    fn host_status_keeps_duplicate_display_names_separate() {
        let status = Arc::new(Mutex::new(StreamStatus::default()));
        parse_line(
            r#"[host-status] {"event":"joined","peer":"a","label":"orange"}"#,
            &status,
        );
        parse_line(
            r#"[host-status] {"event":"joined","peer":"b","label":"orange"}"#,
            &status,
        );
        parse_line(
            r#"[host-status] {"event":"left","peer":"a","label":"orange"}"#,
            &status,
        );

        assert_eq!(status.lock().unwrap().viewers, ["orange"]);
    }

    /// A viewer who never signed in has no Discord id, and the routing `peer`
    /// is reassigned every session. Offering to "keep" that is offering to
    /// remember a number that will never match anyone again.
    #[test]
    fn an_anonymous_peer_produces_no_one_to_add() {
        let status = Arc::new(Mutex::new(StreamStatus::default()));

        parse_line(
            r#"[host-status] {"event":"joined","peer":"a","label":"viewer a","id":null}"#,
            &status,
        );
        parse_line(r#"[watch-host] {"name":"Anonymous Host"}"#, &status);

        assert!(status.lock().unwrap().met.is_empty());
    }

    #[test]
    fn joining_viewers_do_not_overwrite_an_unreviewed_friend_offer() {
        // Two authenticated viewers can arrive between UI ticks. A single
        // `met` slot silently lost the first person before Add could appear.
        let status = Arc::new(Mutex::new(StreamStatus::default()));
        for (peer, id) in [("first", "42"), ("second", "77")] {
            parse_line(
                &format!(
                    r#"[host-status] {{"event":"joined","peer":"{peer}","id":"{id}","label":"Friend"}}"#
                ),
                &status,
            );
        }
        assert_eq!(
            status
                .lock()
                .unwrap()
                .met
                .iter()
                .map(|friend| friend.id.as_str())
                .collect::<Vec<_>>(),
            ["42", "77"]
        );
    }

    #[test]
    fn friend_profile_child() {
        let Ok(mode) = std::env::var("ORANGE_TEST_FRIEND_CHILD") else {
            return;
        };
        println!("\n[watch-host] {{\"id\":\"42\",\"name\":\"Friend\",\"avatar_url\":null}}");
        if mode == "wait" {
            std::thread::sleep(std::time::Duration::from_secs(60));
        }
    }

    /// Both directions of a code join have to surface an identity, or only one
    /// side of a new friendship can be formed and the other silently shows
    /// "Not streaming" forever because they never listed their half.
    #[test]
    fn both_a_watched_host_and_a_joining_viewer_can_be_kept() {
        let host_side = Arc::new(Mutex::new(StreamStatus::default()));
        parse_line(
            r#"[host-status] {"event":"joined","peer":"a","label":"Vee","id":"77","avatar_url":"https://cdn/v.png"}"#,
            &host_side,
        );
        let met = host_side.lock().unwrap().met[0].clone();
        assert_eq!(met.id, "77");
        assert_eq!(met.name, "Vee");
        assert_eq!(met.avatar_url.as_deref(), Some("https://cdn/v.png"));

        let watch_side = Arc::new(Mutex::new(StreamStatus::default()));
        parse_line(
            r#"[watch-host] {"id":"42","name":"Hoss","avatar_url":"https://cdn/h.png"}"#,
            &watch_side,
        );
        let met = watch_side.lock().unwrap().met[0].clone();
        assert_eq!(met.id, "42");
        assert_eq!(met.name, "Hoss");
        assert_eq!(met.avatar_url.as_deref(), Some("https://cdn/h.png"));
    }

    #[test]
    fn automatic_quality_constraint_is_retained_for_the_live_client_notice() {
        let status = Arc::new(Mutex::new(StreamStatus::default()));

        parse_line(
            r#"[quality-status] {"event":"automatic-bitrate-constrained","message":"Automatic quality is limited"}"#,
            &status,
        );

        assert_eq!(
            status.lock().unwrap().notice.as_deref(),
            Some("Automatic quality is limited")
        );
    }
}
