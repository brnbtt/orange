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
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant};

const BETA_MANIFEST_URL: &str =
    "https://orangealpha0d8d5893e69a3.blob.core.windows.net/releases/orange-beta.json";
const BETA_ASSET_HOST: &str = "orangealpha0d8d5893e69a3.blob.core.windows.net";
const MAX_INSTALLER_BYTES: u64 = 250 * 1024 * 1024;
const MAX_MANIFEST_BYTES: u64 = 64 * 1024;
const CREATE_NO_WINDOW: u32 = 0x0800_0000;
const UPDATE_CHECK_INTERVAL: Duration = Duration::from_secs(6 * 60 * 60);

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct UpdateInfo {
    pub(crate) version: Version,
    pub(crate) build: String,
    pub(crate) installer_url: Url,
    pub(crate) sha256: String,
    pub(crate) notes: String,
}

pub(crate) enum UpdateEvent {
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

enum Request {
    Check,
    Download(UpdateInfo),
}

fn request_for(status: &UpdateStatus) -> Option<Request> {
    match status {
        UpdateStatus::Available(info) => Some(Request::Download(info.clone())),
        UpdateStatus::Failed { .. } => Some(Request::Check),
        _ => None,
    }
}

fn periodic_check_due(
    updates_enabled: bool,
    receiver_idle: bool,
    deadline_reached: bool,
    status: &UpdateStatus,
) -> bool {
    updates_enabled
        && receiver_idle
        && deadline_reached
        && matches!(
            status,
            UpdateStatus::Current | UpdateStatus::Available(_) | UpdateStatus::Failed { .. }
        )
}

pub(crate) struct UpdateController {
    status: UpdateStatus,
    receiver: Option<Receiver<UpdateEvent>>,
    next_update_check: Instant,
}

impl UpdateController {
    pub(crate) fn new() -> Self {
        let receiver = start_check();
        let status = if receiver.is_some() {
            UpdateStatus::Checking
        } else {
            UpdateStatus::Disabled
        };
        Self {
            status,
            receiver,
            next_update_check: Instant::now() + UPDATE_CHECK_INTERVAL,
        }
    }

    pub(crate) fn status(&self) -> &UpdateStatus {
        &self.status
    }

    pub(crate) fn poll_event(&mut self) -> Option<(UpdateInfo, PathBuf)> {
        let event = self
            .receiver
            .as_ref()
            .and_then(|receiver| match receiver.try_recv() {
                Ok(event) => Some(Ok(event)),
                Err(mpsc::TryRecvError::Disconnected) => Some(Err(())),
                Err(mpsc::TryRecvError::Empty) => None,
            });
        match event {
            Some(Ok(UpdateEvent::Checked(Ok(Some(info))))) => {
                self.status = UpdateStatus::Available(info);
                self.receiver = None;
            }
            Some(Ok(UpdateEvent::Checked(Ok(None)))) => {
                self.status = UpdateStatus::Current;
                self.receiver = None;
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
                self.receiver = None;
            }
            Some(Ok(UpdateEvent::Downloaded { info, result })) => {
                self.receiver = None;
                match result {
                    Ok(installer) => return Some((info, installer)),
                    Err(_) => {
                        self.status = UpdateStatus::Failed {
                            message: "Update download failed".into(),
                        };
                    }
                }
            }
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
            self.receiver.is_none(),
            Instant::now() >= self.next_update_check,
            &self.status,
        ) {
            self.status = UpdateStatus::Checking;
            self.receiver = start_check();
            self.next_update_check = Instant::now() + UPDATE_CHECK_INTERVAL;
        }
    }

    pub(crate) fn request_update(&mut self) {
        match request_for(&self.status) {
            Some(Request::Download(info)) => {
                self.receiver = Some(start_download(info.clone()));
                self.status = UpdateStatus::Downloading(info);
            }
            Some(Request::Check) => {
                self.status = UpdateStatus::Checking;
                self.receiver = start_check();
            }
            None => {}
        }
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

pub(crate) fn enabled() -> bool {
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
        build: manifest.build,
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

pub(crate) fn check_for_update(current_version: &str) -> Result<Option<UpdateInfo>> {
    if !enabled() {
        return Ok(None);
    }
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
    if response
        .content_length()
        .is_some_and(|length| length > MAX_MANIFEST_BYTES)
    {
        bail!("update manifest is too large");
    }
    let mut manifest = Vec::new();
    copy_bounded(&mut response, &mut manifest, MAX_MANIFEST_BYTES)?;
    let manifest = String::from_utf8(manifest).context("update manifest is not UTF-8")?;
    parse_update_manifest(&manifest, current_version)
}

pub(crate) fn download_update(info: &UpdateInfo) -> Result<PathBuf> {
    let local = std::env::var_os("LOCALAPPDATA").context("LOCALAPPDATA is not set")?;
    let directory = PathBuf::from(local).join("orange").join("updates");
    std::fs::create_dir_all(&directory)?;
    let filename = format!("orange-setup-{}.exe", info.version);
    let destination = directory.join(filename);
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
    let result = (|| -> Result<()> {
        let client = reqwest::blocking::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(10 * 60))
            .redirect(reqwest::redirect::Policy::none())
            .user_agent(concat!("orange/", env!("CARGO_PKG_VERSION")))
            .build()?;
        let mut response = client
            .get(info.installer_url.clone())
            .send()
            .context("could not download update")?
            .error_for_status()
            .context("update download failed")?;
        if response
            .content_length()
            .is_some_and(|length| length > MAX_INSTALLER_BYTES)
        {
            bail!("update is larger than 250 MiB");
        }
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)?;
        copy_bounded(&mut response, &mut file, MAX_INSTALLER_BYTES)?;
        file.flush()?;
        file.sync_all()?;
        if !installer_matches(&temporary, &info.sha256)? {
            bail!("update checksum mismatch");
        }
        std::fs::rename(&temporary, &destination)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    result?;
    Ok(destination)
}

fn copy_bounded(reader: &mut impl Read, writer: &mut impl Write, limit: u64) -> Result<u64> {
    let copied = std::io::copy(&mut reader.take(limit + 1), writer)?;
    if copied > limit {
        bail!("download exceeds its size limit");
    }
    Ok(copied)
}

pub(crate) fn start_check() -> Option<Receiver<UpdateEvent>> {
    enabled().then(|| {
        let (sender, receiver) = mpsc::channel();
        std::thread::spawn(move || {
            let result = check_for_update(current_version()).map_err(|error| error.to_string());
            let _ = sender.send(UpdateEvent::Checked(result));
        });
        receiver
    })
}

pub(crate) fn start_download(info: UpdateInfo) -> Receiver<UpdateEvent> {
    let (sender, receiver) = mpsc::channel();
    std::thread::spawn(move || {
        let result = download_update(&info).map_err(|error| error.to_string());
        let _ = sender.send(UpdateEvent::Downloaded { info, result });
    });
    receiver
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
mod tests {
    use super::*;

    fn update_info() -> UpdateInfo {
        UpdateInfo {
            version: Version::parse("9.0.0").unwrap(),
            build: "1".repeat(40),
            installer_url: Url::parse("https://orangealpha0d8d5893e69a3.blob.core.windows.net/releases/orange-setup-9.0.0.exe").unwrap(),
            sha256: "A".repeat(64),
            notes: "Faster joining".into(),
        }
    }

    fn controller(
        status: UpdateStatus,
        receiver: Option<Receiver<UpdateEvent>>,
    ) -> UpdateController {
        UpdateController {
            status,
            receiver,
            next_update_check: Instant::now() + UPDATE_CHECK_INTERVAL,
        }
    }

    fn controller_with_event(status: UpdateStatus, event: UpdateEvent) -> UpdateController {
        let (sender, receiver) = mpsc::channel();
        sender.send(event).unwrap();
        controller(status, Some(receiver))
    }

    #[test]
    fn controller_checked_events_become_available_or_current() {
        let available = update_info();
        let mut controller = controller_with_event(
            UpdateStatus::Checking,
            UpdateEvent::Checked(Ok(Some(available.clone()))),
        );
        assert!(controller.poll_event().is_none());
        assert!(matches!(
            controller.status(),
            UpdateStatus::Available(info) if info == &available
        ));

        let mut controller =
            controller_with_event(UpdateStatus::Checking, UpdateEvent::Checked(Ok(None)));
        assert!(controller.poll_event().is_none());
        assert!(matches!(controller.status(), UpdateStatus::Current));
    }

    #[test]
    fn controller_processes_at_most_one_event_per_tick() {
        let first = update_info();
        let (sender, receiver) = mpsc::channel();
        sender
            .send(UpdateEvent::Checked(Ok(Some(first.clone()))))
            .unwrap();
        sender.send(UpdateEvent::Checked(Ok(None))).unwrap();
        let mut controller = controller(UpdateStatus::Checking, Some(receiver));

        assert!(controller.poll_event().is_none());
        assert!(controller.poll_event().is_none());
        assert!(matches!(
            controller.status(),
            UpdateStatus::Available(info) if info == &first
        ));
    }

    #[test]
    fn controller_failures_use_exact_messages() {
        for (status, event, expected) in [
            (
                UpdateStatus::Checking,
                Some(UpdateEvent::Checked(Err("offline".into()))),
                "Could not check for updates",
            ),
            (
                UpdateStatus::Downloading(update_info()),
                Some(UpdateEvent::Downloaded {
                    info: update_info(),
                    result: Err("offline".into()),
                }),
                "Update download failed",
            ),
            (UpdateStatus::Checking, None, "Could not check for updates"),
            (
                UpdateStatus::Downloading(update_info()),
                None,
                "Update download failed",
            ),
        ] {
            let mut controller = if let Some(event) = event {
                controller_with_event(status, event)
            } else {
                let (_, receiver) = mpsc::channel();
                controller(status, Some(receiver))
            };

            assert!(controller.poll_event().is_none());
            assert!(matches!(
                controller.status(),
                UpdateStatus::Failed { message } if message == expected
            ));
        }
    }

    #[test]
    fn controller_exposes_downloaded_installer() {
        let info = update_info();
        let installer = PathBuf::from(r"C:\cached\orange-setup-9.0.0.exe");
        let mut controller = controller_with_event(
            UpdateStatus::Downloading(info.clone()),
            UpdateEvent::Downloaded {
                info: info.clone(),
                result: Ok(installer.clone()),
            },
        );

        assert_eq!(controller.poll_event(), Some((info, installer)));
    }

    #[test]
    fn controller_periodic_check_eligibility_matrix() {
        let current = UpdateStatus::Current;
        let available = UpdateStatus::Available(update_info());
        let failed = UpdateStatus::Failed {
            message: "offline".into(),
        };
        let downloading = UpdateStatus::Downloading(update_info());
        for (enabled, idle, due, status, expected) in [
            (true, true, true, &current, true),
            (true, true, true, &available, true),
            (true, true, true, &failed, true),
            (false, true, true, &current, false),
            (true, false, true, &current, false),
            (true, true, false, &current, false),
            (true, true, true, &UpdateStatus::Disabled, false),
            (true, true, true, &UpdateStatus::Checking, false),
            (true, true, true, &downloading, false),
        ] {
            assert_eq!(periodic_check_due(enabled, idle, due, status), expected);
        }
    }

    #[test]
    fn controller_request_policy() {
        let info = update_info();
        assert!(matches!(
            request_for(&UpdateStatus::Available(info.clone())),
            Some(Request::Download(requested)) if requested == info
        ));
        assert!(matches!(
            request_for(&UpdateStatus::Failed {
                message: "offline".into()
            }),
            Some(Request::Check)
        ));
    }

    #[test]
    fn controller_launch_failure_is_available_to_periodic_scheduling() {
        let mut controller = controller(UpdateStatus::Downloading(update_info()), None);

        controller.updater_launch_failed();
        assert!(matches!(
            controller.status(),
            UpdateStatus::Failed { message } if message == "Could not start the updater"
        ));
        assert!(periodic_check_due(true, true, true, controller.status()));
    }

    fn manifest(version: &str, hash: &str, url: &str) -> String {
        format!(
            r#"{{"schema":1,"channel":"beta","version":"{version}","build":"0123456789abcdef0123456789abcdef01234567","installer_url":"{url}","sha256":"{hash}","notes":"Faster joining"}}"#
        )
    }

    #[test]
    fn newer_beta_is_offered_but_current_and_downgrade_are_not() {
        let hash = "A".repeat(64);
        let url = "https://orangealpha0d8d5893e69a3.blob.core.windows.net/releases/orange-setup-0.2.0-beta.2.exe";
        let update = parse_update_manifest(&manifest("0.2.0-beta.2", &hash, url), "0.2.0-beta.1")
            .unwrap()
            .unwrap();
        assert_eq!(update.version.to_string(), "0.2.0-beta.2");
        assert!(parse_update_manifest(&manifest("0.2.0-beta.1", &hash, "https://orangealpha0d8d5893e69a3.blob.core.windows.net/releases/orange-setup-0.2.0-beta.1.exe"), "0.2.0-beta.1").unwrap().is_none());
        assert!(parse_update_manifest(&manifest("0.1.9", &hash, "https://orangealpha0d8d5893e69a3.blob.core.windows.net/releases/orange-setup-0.1.9.exe"), "0.2.0-beta.1").unwrap().is_none());
    }

    #[test]
    fn manifest_rejects_unknown_fields_and_untrusted_urls() {
        let hash = "B".repeat(64);
        let good = "https://orangealpha0d8d5893e69a3.blob.core.windows.net/releases/orange-setup-0.2.0-beta.2.exe";
        let extra = manifest("0.2.0-beta.2", &hash, good).replace("}", ",\"extra\":true}");
        assert!(parse_update_manifest(&extra, "0.2.0-beta.1").is_err());
        assert!(parse_update_manifest(
            &manifest(
                "0.2.0-beta.2",
                &hash,
                "https://example.com/orange-setup-0.2.0-beta.2.exe"
            ),
            "0.2.0-beta.1"
        )
        .is_err());
    }

    #[test]
    fn manifest_rejects_wrong_filename_build_and_hash() {
        let hash = "C".repeat(64);
        let url = "https://orangealpha0d8d5893e69a3.blob.core.windows.net/releases/orange-setup-wrong.exe";
        assert!(
            parse_update_manifest(&manifest("0.2.0-beta.2", &hash, url), "0.2.0-beta.1").is_err()
        );
        assert!(parse_update_manifest(
            &manifest("0.2.0-beta.2", "bad", "https://orangealpha0d8d5893e69a3.blob.core.windows.net/releases/orange-setup-0.2.0-beta.2.exe"),
            "0.2.0-beta.1"
        )
        .is_err());
    }

    #[test]
    fn cached_installer_must_match_the_manifest_hash() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("update.exe");
        std::fs::write(&path, b"installer").unwrap();
        let hash = sha256_file(&path).unwrap();
        assert!(installer_matches(&path, &hash).unwrap());
        assert!(!installer_matches(&path, &"0".repeat(64)).unwrap());
    }

    #[test]
    fn bounded_copy_accepts_the_limit_and_rejects_one_more_byte() {
        let mut exact = &b"1234"[..];
        let mut exact_output = Vec::new();
        assert_eq!(copy_bounded(&mut exact, &mut exact_output, 4).unwrap(), 4);
        assert_eq!(exact_output, b"1234");

        let mut oversized = &b"12345"[..];
        assert!(copy_bounded(&mut oversized, &mut Vec::new(), 4).is_err());
    }

    #[test]
    fn updater_handoff_uses_a_detached_temporary_copy() {
        let directory = tempfile::tempdir().unwrap();
        let install_dir = directory.path().join("installed");
        let temporary = directory.path().join("temporary");
        std::fs::create_dir_all(&install_dir).unwrap();
        std::fs::create_dir_all(&temporary).unwrap();
        std::fs::write(install_dir.join("orange-tray.exe"), b"tray").unwrap();
        std::fs::write(install_dir.join("orange-updater.exe"), b"updater").unwrap();
        std::fs::write(install_dir.join("vcruntime140.dll"), b"runtime").unwrap();
        let installer = directory.path().join("orange-setup-0.2.0-beta.2.exe");
        std::fs::write(&installer, b"installer").unwrap();
        let info = UpdateInfo {
            version: Version::parse("0.2.0-beta.2").unwrap(),
            build: "1".repeat(40),
            installer_url: Url::parse("https://orangealpha0d8d5893e69a3.blob.core.windows.net/releases/orange-setup-0.2.0-beta.2.exe").unwrap(),
            sha256: "A".repeat(64),
            notes: String::new(),
        };

        let launch = prepare_updater(&info, &installer, &install_dir, &temporary, 42).unwrap();

        assert_eq!(launch.executable.file_name().unwrap(), "orange-updater.exe");
        assert!(launch.executable.starts_with(&temporary));
        assert_ne!(launch.executable.parent().unwrap(), temporary);
        assert!(launch.executable.is_file());
        assert!(launch
            .executable
            .parent()
            .unwrap()
            .join("vcruntime140.dll")
            .is_file());
        assert_eq!(launch.arguments[0], "--installer");
        assert_eq!(launch.arguments[1], installer.as_os_str());
        assert!(launch.arguments.iter().any(|arg| arg == "42"));
        assert!(!launch
            .arguments
            .iter()
            .any(|arg| arg.to_string_lossy().contains("token")));
    }

    #[test]
    fn banner_state_exposes_one_clear_action() {
        let info = UpdateInfo {
            version: Version::parse("0.2.0-beta.2").unwrap(),
            build: "1".repeat(40),
            installer_url: Url::parse("https://orangealpha0d8d5893e69a3.blob.core.windows.net/releases/orange-setup-0.2.0-beta.2.exe").unwrap(),
            sha256: "A".repeat(64),
            notes: "Faster joining".into(),
        };
        assert_eq!(
            UpdateStatus::Available(info.clone()).action_label(),
            Some("Update now")
        );
        assert_eq!(UpdateStatus::Downloading(info).action_label(), None);
        assert_eq!(
            UpdateStatus::Failed {
                message: "offline".into(),
            }
            .action_label(),
            Some("Check again")
        );
        assert!(!UpdateStatus::Current.is_visible());
    }
}
