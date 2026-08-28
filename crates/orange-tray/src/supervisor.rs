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

#[derive(Debug, Clone, Deserialize)]
pub struct WindowTarget {
    pub hwnd: i64,
    pub pid: u32,
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

pub fn list_windows() -> Result<Vec<WindowTarget>> {
    let output = Command::new(orange_exe()?)
        .args(["list", "--json"])
        .creation_flags(CREATE_NO_WINDOW)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .context("could not run `orange list`")?;
    let text = String::from_utf8_lossy(&output.stdout);
    // The binary prints nothing else on stdout in JSON mode, but be forgiving.
    let json = text.lines().find(|l| l.trim_start().starts_with('[')).unwrap_or("[]");
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
    let child = Command::new(orange_exe()?)
        .arg("login")
        .args(["--server", server])
        .creation_flags(CREATE_NO_WINDOW)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .context("could not start `orange login`")?;
    Ok(LoginAttempt { child })
}

pub struct LoginAttempt {
    child: Child,
}

impl LoginAttempt {
    /// `None` while still running; `Some(reason)` once it has exited without
    /// producing a session.
    pub fn failure(&mut self) -> Option<String> {
        match self.child.try_wait() {
            Ok(Some(_)) => {
                let mut reason = String::new();
                if let Some(stderr) = self.child.stderr.take() {
                    for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                        if !line.trim().is_empty() {
                            reason = line;
                        }
                    }
                }
                Some(if reason.is_empty() {
                    "Login was cancelled or timed out".to_string()
                } else {
                    reason
                })
            }
            _ => None,
        }
    }
}

/// What the UI has learned from a running child process.
#[derive(Debug, Default, Clone)]
pub struct StreamStatus {
    pub code: Option<String>,
    pub viewers: Vec<String>,
    pub error: Option<String>,
    pub signed_in_as: Option<String>,
}

pub struct Supervisor {
    child: Child,
    pub status: Arc<Mutex<StreamStatus>>,
}

impl Supervisor {
    /// Start `orange host` for a window and begin parsing its output.
    pub fn host(target: &WindowTarget, quality: &Quality, server: &str) -> Result<Self> {
        let mut command = Command::new(orange_exe()?);
        command
            .arg("host")
            .args(["--hwnd", &target.hwnd.to_string()])
            .args(["--server", server])
            .args(["--codec", quality.codec])
            .args(["--bitrate", &quality.bitrate.to_string()])
            .args(["--scale", &quality.scale]);
        Self::spawn(command)
    }

    /// Start `orange watch` for a code.
    pub fn watch(code: &str, server: &str) -> Result<Self> {
        let mut command = Command::new(orange_exe()?);
        command
            .arg("watch")
            .args(["--code", code])
            .args(["--server", server]);
        Self::spawn(command)
    }

    fn spawn(mut command: Command) -> Result<Self> {
        let mut child = command
            .creation_flags(CREATE_NO_WINDOW)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .context("could not start the orange binary")?;

        let status = Arc::new(Mutex::new(StreamStatus::default()));

        if let Some(stdout) = child.stdout.take() {
            let status = status.clone();
            std::thread::spawn(move || {
                for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                    parse_line(&line, &status);
                }
            });
        }
        if let Some(stderr) = child.stderr.take() {
            let status = status.clone();
            std::thread::spawn(move || {
                for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                    if line.contains("Error") || line.contains("error") {
                        if let Ok(mut status) = status.lock() {
                            status.error = Some(line);
                        }
                    }
                }
            });
        }

        Ok(Self { child, status })
    }

    pub fn running(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }

    pub fn stop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Drop for Supervisor {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Extract the few facts the UI cares about from the child's log lines.
fn parse_line(line: &str, status: &Arc<Mutex<StreamStatus>>) {
    let Ok(mut status) = status.lock() else { return };

    if let Some(rest) = line.split("Share this code:").nth(1) {
        status.code = Some(rest.trim().to_string());
    } else if let Some(rest) = line.strip_prefix("[host] signed in as ") {
        status.signed_in_as = Some(rest.trim().to_string());
    } else if line.starts_with("[host] ") && line.contains(" joined (") {
        if let Some(name) = line
            .strip_prefix("[host] ")
            .and_then(|r| r.split(" joined (").next())
        {
            let name = name.trim().to_string();
            if !status.viewers.contains(&name) {
                status.viewers.push(name);
            }
        }
    } else if line.starts_with("[host] ") && line.contains(" left (") {
        if let Some(name) = line
            .strip_prefix("[host] ")
            .and_then(|r| r.split(" left (").next())
        {
            status.viewers.retain(|v| v != name.trim());
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Quality {
    pub label: &'static str,
    pub scale: &'static str,
    pub bitrate: u32,
    pub codec: &'static str,
    /// Rough upload cost per viewer, shown so the tradeoff is visible.
    pub mbps: u32,
}

pub const QUALITIES: &[Quality] = &[
    Quality {
        label: "720p",
        scale: "1280x720",
        bitrate: 4_000,
        codec: "av1",
        mbps: 4,
    },
    Quality {
        label: "1080p",
        scale: "1920x1080",
        bitrate: 8_000,
        codec: "av1",
        mbps: 8,
    },
    Quality {
        label: "1440p",
        scale: "2560x1440",
        bitrate: 18_000,
        codec: "av1",
        mbps: 18,
    },
];
