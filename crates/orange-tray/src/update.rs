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
use std::time::Duration;

const BETA_MANIFEST_URL: &str =
    "https://orangealpha0d8d5893e69a3.blob.core.windows.net/releases/orange-beta.json";
const BETA_ASSET_HOST: &str = "orangealpha0d8d5893e69a3.blob.core.windows.net";
const MAX_INSTALLER_BYTES: u64 = 250 * 1024 * 1024;
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

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
    Failed {
        info: Option<UpdateInfo>,
        message: String,
    },
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
            Self::Failed { info: Some(_), .. } => Some("Retry update"),
            Self::Failed { info: None, .. } => Some("Check again"),
            Self::Disabled | Self::Checking | Self::Current | Self::Downloading(_) => None,
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
        .user_agent(concat!("orange/", env!("CARGO_PKG_VERSION")))
        .build()?;
    let manifest = client
        .get(BETA_MANIFEST_URL)
        .header(reqwest::header::CACHE_CONTROL, "no-cache")
        .send()
        .context("could not check for updates")?
        .error_for_status()
        .context("update service rejected the check")?
        .text()
        .context("could not read update manifest")?;
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
        bail!("update is larger than 250 MiB");
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
    if !source.is_file() {
        bail!("orange-updater.exe is missing");
    }
    std::fs::create_dir_all(temporary_dir)?;
    let executable = temporary_dir.join(format!("orange-updater-{}.exe", &info.build[..7]));
    if executable.exists() {
        std::fs::remove_file(&executable)?;
    }
    std::fs::copy(&source, &executable)
        .with_context(|| format!("could not prepare {}", executable.display()))?;
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

        assert_eq!(
            launch.executable,
            temporary.join("orange-updater-1111111.exe")
        );
        assert!(launch.executable.is_file());
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
                info: None,
                message: "offline".into(),
            }
            .action_label(),
            Some("Check again")
        );
        assert!(!UpdateStatus::Current.is_visible());
    }
}
