//! Beta update discovery, verified download, and updater handoff.

use anyhow::{bail, Context, Result};
use reqwest::Url;
use semver::Version;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::ffi::OsString;
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

const BETA_MANIFEST_URL: &str =
    "https://orangealpha0d8d5893e69a3.blob.core.windows.net/releases/orange-beta.json";
const BETA_ASSET_HOST: &str = "orangealpha0d8d5893e69a3.blob.core.windows.net";
const MAX_INSTALLER_BYTES: u64 = 250 * 1024 * 1024;
const MAX_MANIFEST_BYTES: u64 = 64 * 1024;
const CREATE_NO_WINDOW: u32 = 0x0800_0000;
const UPDATE_CHECK_INTERVAL: Duration = Duration::from_secs(6 * 60 * 60);
const UPDATE_JOIN_TIMEOUT: Duration = Duration::from_secs(40);

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct UpdateInfo {
    pub(crate) version: Version,
    pub(crate) installer_url: Url,
    pub(crate) sha256: String,
    pub(crate) notes: String,
}

enum UpdateEvent {
    Checked(Result<Option<UpdateInfo>, String>),
    Downloaded {
        info: UpdateInfo,
        result: Result<PathBuf, String>,
    },
}

#[derive(Clone, Debug)]
pub(crate) enum UpdateStatus {
    Disabled,
    Checking,
    Current,
    Available(UpdateInfo),
    Downloading(UpdateInfo),
    Failed { message: String },
}

impl UpdateStatus {
    pub(crate) fn is_visible(&self) -> bool {
        matches!(
            self,
            Self::Available(_) | Self::Downloading(_) | Self::Failed { .. }
        )
    }

    pub(crate) fn action_label(&self) -> Option<&'static str> {
        match self {
            Self::Available(_) => Some("Update now"),
            Self::Failed { .. } => Some("Check again"),
            Self::Disabled | Self::Checking | Self::Current | Self::Downloading(_) => None,
        }
    }
}

fn periodic_check_due(
    updates_enabled: bool,
    receiver_idle: bool,
    deadline_reached: bool,
    status: &UpdateStatus,
) -> bool {
    updates_enabled && receiver_idle && deadline_reached && check_startable(status)
}

/// A manual check is the periodic one without the deadline: the user asking is
/// the trigger. Both share `check_startable` so the two can never disagree
/// about which states a check may begin from.
fn manual_check_due(updates_enabled: bool, receiver_idle: bool, status: &UpdateStatus) -> bool {
    updates_enabled && receiver_idle && check_startable(status)
}

/// States a check may start from. Excludes `Checking` and `Downloading`, which
/// already own a job, and `Disabled`, which has no manifest to check.
fn check_startable(status: &UpdateStatus) -> bool {
    matches!(
        status,
        UpdateStatus::Current | UpdateStatus::Available(_) | UpdateStatus::Failed { .. }
    )
}

/// How long ago the last completed check was, phrased for the settings screen.
///
/// Pure so the wording is testable without waiting on a clock. Returns `None`
/// before the first check completes.
fn checked_ago(elapsed: Option<Duration>) -> Option<String> {
    let seconds = elapsed?.as_secs();
    let plural = |n: u64, unit: &str| format!("{n} {unit}{} ago", if n == 1 { "" } else { "s" });
    Some(match seconds {
        0..=59 => "just now".to_string(),
        60..=3599 => plural(seconds / 60, "minute"),
        _ => plural(seconds / 3600, "hour"),
    })
}

pub(crate) struct UpdateController {
    status: UpdateStatus,
    job: Option<UpdateJob>,
    next_update_check: Instant,
    last_checked: Option<Instant>,
}

struct UpdateJob {
    receiver: Receiver<UpdateEvent>,
    cancel: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}

impl UpdateJob {
    fn is_finished(&self) -> bool {
        self.worker.as_ref().is_none_or(JoinHandle::is_finished)
    }

    fn join(&mut self) {
        let Some(worker) = self.worker.take() else {
            return;
        };
        let deadline = Instant::now() + UPDATE_JOIN_TIMEOUT;
        while !worker.is_finished() {
            if Instant::now() >= deadline {
                crate::client::fail_fast(
                    "update worker did not terminate before its deadline",
                    &anyhow::anyhow!("timeout after {UPDATE_JOIN_TIMEOUT:?}"),
                );
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        if worker.join().is_err() {
            eprintln!("[client] update worker panicked");
        }
    }
}

impl Drop for UpdateJob {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Release);
        self.join();
    }
}

impl UpdateController {
    pub(crate) fn new() -> Self {
        let job = start_check();
        let status = if job.is_some() {
            UpdateStatus::Checking
        } else {
            UpdateStatus::Disabled
        };
        Self {
            status,
            job,
            next_update_check: Instant::now() + UPDATE_CHECK_INTERVAL,
            last_checked: None,
        }
    }

    pub(crate) fn status(&self) -> &UpdateStatus {
        &self.status
    }

    pub(crate) fn poll_event(&mut self) -> Option<(UpdateInfo, PathBuf)> {
        if self.job.as_ref().is_some_and(|job| !job.is_finished()) {
            return None;
        }
        let event = self
            .job
            .as_ref()
            .and_then(|job| match job.receiver.try_recv() {
                Ok(event) => Some(Ok(event)),
                Err(mpsc::TryRecvError::Disconnected) => Some(Err(())),
                Err(mpsc::TryRecvError::Empty) => None,
            });
        if event.is_some() {
            if let Some(mut job) = self.job.take() {
                job.join();
            }
        }
        match event {
            Some(Ok(UpdateEvent::Checked(Ok(Some(info))))) => {
                self.last_checked = Some(Instant::now());
                self.status = UpdateStatus::Available(info);
            }
            Some(Ok(UpdateEvent::Checked(Ok(None)))) => {
                self.last_checked = Some(Instant::now());
                self.status = UpdateStatus::Current;
            }
            Some(Ok(UpdateEvent::Checked(Err(_)))) | Some(Err(())) => {
                let message = if matches!(self.status, UpdateStatus::Downloading(_)) {
                    "Update download failed"
                } else {
                    "Could not check for updates"
                };
                self.status = UpdateStatus::Failed {
                    message: message.into(),
                };
            }
            Some(Ok(UpdateEvent::Downloaded { info, result })) => match result {
                Ok(installer) => return Some((info, installer)),
                Err(_) => {
                    self.status = UpdateStatus::Failed {
                        message: "Update download failed".into(),
                    };
                }
            },
            None => {}
        }
        None
    }

    pub(crate) fn updater_launch_failed(&mut self) {
        self.status = UpdateStatus::Failed {
            message: "Could not start the updater".into(),
        };
    }

    pub(crate) fn schedule_periodic(&mut self) {
        if periodic_check_due(
            enabled(),
            self.job.is_none(),
            Instant::now() >= self.next_update_check,
            &self.status,
        ) {
            self.begin_check();
        }
    }

    /// Whether the settings screen should offer a manual check.
    pub(crate) fn can_check_now(&self) -> bool {
        manual_check_due(enabled(), self.job.is_none(), &self.status)
    }

    /// Check immediately, ignoring the periodic deadline.
    ///
    /// Guarded rather than assumed: the button is hidden when a check cannot
    /// start, but the controller stays correct if that is ever called anyway.
    pub(crate) fn check_now(&mut self) {
        if self.can_check_now() {
            self.begin_check();
        }
    }

    fn begin_check(&mut self) {
        self.status = UpdateStatus::Checking;
        self.stop_job();
        self.job = start_check();
        self.next_update_check = Instant::now() + UPDATE_CHECK_INTERVAL;
    }

    /// The settings row's action, if one applies.
    ///
    /// Mirrors the banner: an available update offers to install it, anything
    /// else offers a check. Both routes call the same status machine, so the
    /// two controls are views of one state rather than two states to reconcile.
    pub(crate) fn settings_action(&self) -> Option<&'static str> {
        if matches!(self.status, UpdateStatus::Available(_)) {
            Some("Update now")
        } else if self.can_check_now() {
            Some("Check now")
        } else {
            None
        }
    }

    /// Perform whatever `settings_action` offers. No-op when it offers nothing.
    pub(crate) fn activate_settings_action(&mut self) {
        if matches!(self.status, UpdateStatus::Available(_)) {
            self.request_update();
        } else {
            self.check_now();
        }
    }

    /// One line describing update state for the settings screen.
    ///
    /// No terminal periods: these are captions, not sentences, and at 12px a
    /// trailing dot is visual lint. "installed builds only" is gone too - that
    /// was our mental model leaking, and a friend handed a build has no idea
    /// which kind they have.
    pub(crate) fn settings_detail(&self) -> String {
        match &self.status {
            UpdateStatus::Disabled => "Automatic updates are off in this build".into(),
            UpdateStatus::Checking => "Checking for updates".into(),
            UpdateStatus::Downloading(info) => format!("Downloading {}", info.version),
            UpdateStatus::Available(info) => format!("{} is available", info.version),
            UpdateStatus::Failed { message } => message.clone(),
            UpdateStatus::Current => match checked_ago(self.last_checked.map(|at| at.elapsed())) {
                Some(ago) => format!("Up to date \u{b7} last checked {ago}"),
                None => "Up to date".into(),
            },
        }
    }

    pub(crate) fn request_update(&mut self) {
        match &self.status {
            UpdateStatus::Available(info) => {
                let info = info.clone();
                self.stop_job();
                self.job = Some(start_download(info.clone()));
                self.status = UpdateStatus::Downloading(info);
            }
            UpdateStatus::Failed { .. } => {
                self.status = UpdateStatus::Checking;
                self.stop_job();
                self.job = start_check();
            }
            _ => {}
        }
    }

    fn stop_job(&mut self) {
        drop(self.job.take());
    }
}

struct UpdaterLaunch {
    executable: PathBuf,
    arguments: Vec<OsString>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct UpdateManifest {
    schema: u32,
    channel: String,
    version: String,
    build: String,
    installer_url: String,
    sha256: String,
    notes: String,
}

fn enabled() -> bool {
    option_env!("ORANGE_UPDATE_CHANNEL") == Some("beta")
}

pub(crate) fn cleanup_helpers() {
    let directory = std::env::temp_dir().join("orange-updates");
    let Ok(entries) = std::fs::read_dir(&directory) else {
        return;
    };
    for entry in entries.flatten() {
        if entry
            .file_name()
            .to_string_lossy()
            .starts_with("orange-update-")
        {
            let _ = std::fs::remove_dir_all(entry.path());
        }
    }
    let _ = std::fs::remove_dir(directory);
}

pub(crate) fn current_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

pub(crate) fn current_build() -> &'static str {
    option_env!("ORANGE_BUILD_ID").unwrap_or("development")
}

pub(crate) fn build_label() -> &'static str {
    let build = current_build();
    if is_lower_hex(build, 40) {
        &build[..7]
    } else {
        build
    }
}

fn parse_update_manifest(json: &str, current_version: &str) -> Result<Option<UpdateInfo>> {
    let manifest: UpdateManifest = serde_json::from_str(json).context("invalid update manifest")?;
    if manifest.schema != 1 || manifest.channel != "beta" {
        bail!("unsupported update manifest");
    }
    if !is_lower_hex(&manifest.build, 40) || !is_hex(&manifest.sha256, 64) {
        bail!("invalid update identity");
    }
    if manifest.notes.len() > 500 || manifest.notes.chars().any(char::is_control) {
        bail!("invalid update notes");
    }

    let version = Version::parse(&manifest.version).context("invalid update version")?;
    let current = Version::parse(current_version).context("invalid current version")?;
    let url = Url::parse(&manifest.installer_url).context("invalid installer URL")?;
    let expected_name = format!("/releases/orange-setup-{version}.exe");
    if url.scheme() != "https"
        || url.host_str() != Some(BETA_ASSET_HOST)
        || url.path() != expected_name
        || url.query().is_some()
        || url.fragment().is_some()
    {
        bail!("untrusted installer URL");
    }
    if version <= current {
        return Ok(None);
    }

    Ok(Some(UpdateInfo {
        version,
        installer_url: url,
        sha256: manifest.sha256.to_ascii_uppercase(),
        notes: manifest.notes,
    }))
}

fn is_lower_hex(value: &str, length: usize) -> bool {
    value.len() == length
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn is_hex(value: &str, length: usize) -> bool {
    value.len() == length && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn sha256_file(path: &Path) -> Result<String> {
    let mut file =
        File::open(path).with_context(|| format!("could not open {}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(format!("{:X}", hasher.finalize()))
}

fn installer_matches(path: &Path, expected: &str) -> Result<bool> {
    Ok(path.is_file() && sha256_file(path)? == expected.to_ascii_uppercase())
}

pub(crate) fn check_for_update(
    current_version: &str,
    cancel: &AtomicBool,
) -> Result<Option<UpdateInfo>> {
    if !enabled() {
        return Ok(None);
    }
    check_cancelled(cancel)?;
    let client = reqwest::blocking::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(30))
        .redirect(reqwest::redirect::Policy::none())
        .user_agent(concat!("orange/", env!("CARGO_PKG_VERSION")))
        .build()?;
    let mut response = client
        .get(BETA_MANIFEST_URL)
        .header(reqwest::header::CACHE_CONTROL, "no-cache")
        .send()
        .context("could not check for updates")?
        .error_for_status()
        .context("update service rejected the check")?;
    check_cancelled(cancel)?;
    if response
        .content_length()
        .is_some_and(|length| length > MAX_MANIFEST_BYTES)
    {
        bail!("update manifest is too large");
    }
    let mut manifest = Vec::new();
    copy_bounded(
        &mut response,
        &mut manifest,
        MAX_MANIFEST_BYTES,
        Some(cancel),
    )?;
    check_cancelled(cancel)?;
    let manifest = String::from_utf8(manifest).context("update manifest is not UTF-8")?;
    parse_update_manifest(&manifest, current_version)
}

pub(crate) fn download_update(info: &UpdateInfo, cancel: &AtomicBool) -> Result<PathBuf> {
    let local = std::env::var_os("LOCALAPPDATA").context("LOCALAPPDATA is not set")?;
    let directory = PathBuf::from(local).join("orange").join("updates");
    std::fs::create_dir_all(&directory)?;
    let filename = format!("orange-setup-{}.exe", info.version);
    let destination = directory.join(filename);
    check_cancelled(cancel)?;
    if destination.is_file() && installer_matches(&destination, &info.sha256)? {
        return Ok(destination);
    }
    if destination.exists() {
        std::fs::remove_file(&destination)?;
    }

    let temporary = directory.join(format!(
        ".download-{}-{}.tmp",
        info.version,
        std::process::id()
    ));
    check_cancelled(cancel)?;
    let client = reqwest::blocking::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(30))
        .redirect(reqwest::redirect::Policy::none())
        .user_agent(concat!("orange/", env!("CARGO_PKG_VERSION")))
        .build()?;
    let mut response = client
        .get(info.installer_url.clone())
        .send()
        .context("could not download update")?
        .error_for_status()
        .context("update download failed")?;
    check_cancelled(cancel)?;
    if response
        .content_length()
        .is_some_and(|length| length > MAX_INSTALLER_BYTES)
    {
        bail!("update is larger than 250 MiB");
    }
    finish_download(
        &mut response,
        &temporary,
        &destination,
        &info.sha256,
        cancel,
    )?;
    Ok(destination)
}

fn finish_download(
    reader: &mut impl Read,
    temporary: &Path,
    destination: &Path,
    expected_sha256: &str,
    cancel: &AtomicBool,
) -> Result<()> {
    let result = (|| -> Result<()> {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(temporary)?;
        copy_bounded(reader, &mut file, MAX_INSTALLER_BYTES, Some(cancel))?;
        file.flush()?;
        file.sync_all()?;
        check_cancelled(cancel)?;
        if !installer_matches(temporary, expected_sha256)? {
            bail!("update checksum mismatch");
        }
        check_cancelled(cancel)?;
        std::fs::rename(temporary, destination)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(temporary);
    }
    result
}

fn copy_bounded(
    reader: &mut impl Read,
    writer: &mut impl Write,
    limit: u64,
    cancel: Option<&AtomicBool>,
) -> Result<u64> {
    let mut copied = 0u64;
    let mut buffer = [0u8; 64 * 1024];
    loop {
        if let Some(cancel) = cancel {
            check_cancelled(cancel)?;
        }
        let read = reader.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        if let Some(cancel) = cancel {
            check_cancelled(cancel)?;
        }
        copied = copied
            .checked_add(read as u64)
            .context("download size overflow")?;
        if copied > limit {
            bail!("download exceeds its size limit");
        }
        writer.write_all(&buffer[..read])?;
    }
    Ok(copied)
}

fn check_cancelled(cancel: &AtomicBool) -> Result<()> {
    if cancel.load(Ordering::Acquire) {
        bail!("update cancelled");
    }
    Ok(())
}

fn start_update_job(run: impl FnOnce(&AtomicBool) -> UpdateEvent + Send + 'static) -> UpdateJob {
    let cancel = Arc::new(AtomicBool::new(false));
    let worker_cancel = Arc::clone(&cancel);
    let (sender, receiver) = mpsc::channel();
    let worker = std::thread::spawn(move || {
        if worker_cancel.load(Ordering::Acquire) {
            return;
        }
        let event = run(&worker_cancel);
        if worker_cancel.load(Ordering::Acquire) {
            return;
        }
        if !worker_cancel.load(Ordering::Acquire) {
            let _ = sender.send(event);
        }
    });
    UpdateJob {
        receiver,
        cancel,
        worker: Some(worker),
    }
}

fn start_check() -> Option<UpdateJob> {
    enabled().then(|| {
        start_update_job(|cancel| {
            let result =
                check_for_update(current_version(), cancel).map_err(|error| error.to_string());
            UpdateEvent::Checked(result)
        })
    })
}

fn start_download(info: UpdateInfo) -> UpdateJob {
    start_update_job(move |cancel| {
        let result = download_update(&info, cancel).map_err(|error| error.to_string());
        UpdateEvent::Downloaded { info, result }
    })
}

fn prepare_updater(
    info: &UpdateInfo,
    installer: &Path,
    install_dir: &Path,
    temporary_dir: &Path,
    parent: u32,
) -> Result<UpdaterLaunch> {
    if !installer.is_file() || !install_dir.is_absolute() || parent == 0 {
        bail!("invalid updater handoff");
    }
    let source = install_dir.join("orange-updater.exe");
    let runtime = install_dir.join("vcruntime140.dll");
    if !source.is_file() {
        bail!("orange-updater.exe is missing");
    }
    if !runtime.is_file() {
        bail!("vcruntime140.dll is missing");
    }
    std::fs::create_dir_all(temporary_dir)?;
    let handoff = tempfile::Builder::new()
        .prefix("orange-update-")
        .tempdir_in(temporary_dir)?;
    let executable = handoff.path().join("orange-updater.exe");
    std::fs::copy(&source, &executable)
        .with_context(|| format!("could not prepare {}", executable.display()))?;
    std::fs::copy(&runtime, handoff.path().join("vcruntime140.dll"))
        .context("could not prepare the updater runtime")?;
    let _ = handoff.keep();
    let arguments = vec![
        OsString::from("--installer"),
        installer.as_os_str().to_owned(),
        OsString::from("--sha256"),
        OsString::from(&info.sha256),
        OsString::from("--parent"),
        OsString::from(parent.to_string()),
        OsString::from("--install-dir"),
        install_dir.as_os_str().to_owned(),
    ];
    Ok(UpdaterLaunch {
        executable,
        arguments,
    })
}

pub(crate) fn launch_updater(info: &UpdateInfo, installer: &Path) -> Result<()> {
    let current = std::env::current_exe().context("could not locate Orange")?;
    let install_dir = current
        .parent()
        .context("Orange has no install directory")?;
    let temporary_dir = std::env::temp_dir().join("orange-updates");
    let launch = prepare_updater(
        info,
        installer,
        install_dir,
        &temporary_dir,
        std::process::id(),
    )?;
    Command::new(&launch.executable)
        .args(&launch.arguments)
        .creation_flags(CREATE_NO_WINDOW)
        .spawn()
        .context("could not start the Orange updater")?;
    Ok(())
}

#[cfg(test)]
#[path = "update_tests.rs"]
mod tests;
