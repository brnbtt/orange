//! Supervising the streaming processes.
//!
//! The tray runs `orange host` and `orange watch` as child processes rather
//! than driving the pipeline in-process. GPUI runs its own executor and the
//! pipeline runs on tokio, so in-process would mean reconciling two runtimes;
//! more importantly, a crash in the media pipeline should not take the UI down
//! with it.
//!
//! Communication is one-way and line-based: we read the child's stdout and
//! look for the handful of things the UI needs to know.

use anyhow::{Context, Result};
use serde::Deserialize;
use std::io::{BufRead, BufReader};
use std::os::windows::process::CommandExt;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};

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

/// Path to the `orange` binary, assumed to sit beside the tray executable.
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
/// tray itself has no GStreamer dependency, which is why it starts fine and
/// only the child fails.
fn gstreamer_bin() -> Option<std::path::PathBuf> {
    // An explicit root wins, since that is what the dev shell sets.
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
fn orange_command() -> Result<Command> {
    let mut command = Command::new(orange_exe()?);

    if let Some(bin) = gstreamer_bin() {
        let existing = std::env::var("PATH").unwrap_or_default();
        command.env("PATH", format!("{};{}", bin.display(), existing));
    }
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

pub fn list_windows() -> Result<Vec<WindowTarget>> {
    let output = orange_command()?
        .args(["list", "--json"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .context("could not run `orange list`")?;
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
/// file when it completes. The tray notices by watching for that file.
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
        fps: Option<u32>,
        server: &str,
    ) -> Result<Self> {
        let scale = quality.scale_for(target);
        let mut command = orange_command()?;
        command
            .arg("host")
            .args(["--hwnd", &target.hwnd.to_string()])
            .args(["--server", server])
            .args(["--codec", quality.codec])
            .args(["--bitrate", &quality.bitrate.to_string()])
            .args(["--scale", &scale]);
        if let Some(fps) = fps {
            command.args(["--fps", &fps.to_string()]);
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

/// Extract the few facts the UI cares about from the child's log lines.
fn parse_line(line: &str, status: &Arc<Mutex<StreamStatus>>) {
    let Ok(mut status) = status.lock() else {
        return;
    };

    if let Some(rest) = line.split("Share this code:").nth(1) {
        status.code = Some(rest.trim().to_string());
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
    pub max_width: u32,
    pub max_height: u32,
    pub bitrate: u32,
    pub codec: &'static str,
    /// Rough upload cost per viewer, shown so the tradeoff is visible.
    pub mbps: u32,
}

impl Quality {
    /// Fit the source inside this quality tier without changing its ratio.
    fn scale_for(&self, target: &WindowTarget) -> String {
        let source_w = target.width.max(2) as f32;
        let source_h = target.height.max(2) as f32;
        let factor = (self.max_width as f32 / source_w)
            .min(self.max_height as f32 / source_h)
            .min(1.0);
        let even = |value: f32| ((value.round() as u32).max(2) / 2) * 2;
        format!("{}x{}", even(source_w * factor), even(source_h * factor))
    }
}

pub const QUALITIES: &[Quality] = &[
    Quality {
        label: "720p",
        max_width: 1280,
        max_height: 720,
        bitrate: 4_000,
        codec: "auto",
        mbps: 4,
    },
    Quality {
        label: "1080p",
        max_width: 1920,
        max_height: 1080,
        bitrate: 8_000,
        codec: "auto",
        mbps: 8,
    },
    Quality {
        label: "1440p",
        max_width: 2560,
        max_height: 1440,
        bitrate: 18_000,
        codec: "auto",
        mbps: 18,
    },
];

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
    fn quality_bounds_preserve_source_shape() {
        assert_eq!(QUALITIES[1].scale_for(&target(2002, 1804)), "1198x1080");
        assert_eq!(QUALITIES[1].scale_for(&target(3440, 1440)), "1920x804");
        assert_eq!(QUALITIES[1].scale_for(&target(1280, 720)), "1280x720");
    }

    #[test]
    fn tray_quality_tiers_request_compatible_encoder_selection() {
        assert!(QUALITIES.iter().all(|quality| quality.codec == "auto"));
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
}
