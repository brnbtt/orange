#![windows_subsystem = "windows"]

use anyhow::{bail, Context, Result};
use sha2::{Digest, Sha256};
use std::ffi::OsString;
use std::fs::File;
use std::io::Read;
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;
use windows::Win32::Foundation::{
    CloseHandle, ERROR_INVALID_PARAMETER, HANDLE, WAIT_FAILED, WAIT_OBJECT_0, WAIT_TIMEOUT,
};
use windows::Win32::System::Threading::{OpenProcess, WaitForSingleObject, PROCESS_SYNCHRONIZE};

const CREATE_NO_WINDOW: u32 = 0x0800_0000;

#[derive(Clone, Debug, PartialEq, Eq)]
struct UpdateArgs {
    installer: PathBuf,
    sha256: String,
    parent: u32,
    install_dir: PathBuf,
}

impl UpdateArgs {
    fn parse<I, S>(args: I) -> Result<Self>
    where
        I: IntoIterator<Item = S>,
        S: Into<OsString>,
    {
        let mut installer = None;
        let mut sha256 = None;
        let mut parent = None;
        let mut install_dir = None;
        let mut args = args.into_iter().map(Into::into).skip(1);
        while let Some(flag) = args.next() {
            let flag = flag.to_string_lossy();
            let value = args
                .next()
                .with_context(|| format!("missing value for {flag}"))?;
            match flag.as_ref() {
                "--installer" if installer.is_none() => installer = Some(PathBuf::from(value)),
                "--sha256" if sha256.is_none() => {
                    sha256 = Some(value.to_string_lossy().into_owned())
                }
                "--parent" if parent.is_none() => {
                    parent = Some(
                        value
                            .to_string_lossy()
                            .parse::<u32>()
                            .context("invalid parent PID")?,
                    )
                }
                "--install-dir" if install_dir.is_none() => {
                    install_dir = Some(PathBuf::from(value))
                }
                "--installer" | "--sha256" | "--parent" | "--install-dir" => {
                    bail!("duplicate argument {flag}")
                }
                _ => bail!("unknown argument {flag}"),
            }
        }
        let installer = installer.context("missing --installer")?;
        let sha256 = sha256.context("missing --sha256")?;
        let parent = parent.context("missing --parent")?;
        let install_dir = install_dir.context("missing --install-dir")?;
        if !installer.is_absolute() || !install_dir.is_absolute() || parent == 0 {
            bail!("update paths must be absolute and parent PID must be nonzero");
        }
        if sha256.len() != 64 || !sha256.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            bail!("invalid installer SHA-256");
        }
        Ok(Self {
            installer,
            sha256: sha256.to_ascii_uppercase(),
            parent,
            install_dir,
        })
    }
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

fn verify_installer(path: &Path, expected: &str) -> Result<()> {
    if !path.is_file() || sha256_file(path)? != expected.to_ascii_uppercase() {
        bail!("installer checksum mismatch");
    }
    Ok(())
}

fn installer_succeeded(code: i32) -> bool {
    matches!(code, 0 | 3010)
}

fn wait_for_parent(pid: u32) -> Result<()> {
    wait_for_parent_with_timeout(pid, 2 * 60 * 1000)
}

fn wait_for_parent_with_timeout(pid: u32, timeout_ms: u32) -> Result<()> {
    let Some(handle) = open_parent(pid)? else {
        return Ok(());
    };
    let result = unsafe { WaitForSingleObject(handle, timeout_ms) };
    let wait_error = (result == WAIT_FAILED).then(windows::core::Error::from_thread);
    unsafe {
        let _ = CloseHandle(handle);
    }
    if let Some(error) = wait_error {
        return Err(error).context("could not wait for Orange to exit");
    }
    if result == WAIT_TIMEOUT {
        bail!("Orange did not exit before the update timeout");
    }
    if result != WAIT_OBJECT_0 {
        bail!("unexpected result while waiting for Orange to exit");
    }
    std::thread::sleep(Duration::from_millis(200));
    Ok(())
}

fn open_parent(pid: u32) -> Result<Option<HANDLE>> {
    match unsafe { OpenProcess(PROCESS_SYNCHRONIZE, false, pid) } {
        Ok(handle) => Ok(Some(handle)),
        Err(error)
            if error.code() == windows::core::HRESULT::from_win32(ERROR_INVALID_PARAMETER.0) =>
        {
            Ok(None)
        }
        Err(error) => Err(error).context("could not inspect the Orange process"),
    }
}

fn run_installer(installer: &Path) -> Result<i32> {
    let status = Command::new(installer)
        .args([
            "/VERYSILENT",
            "/SUPPRESSMSGBOXES",
            "/NORESTART",
            "/CLOSEAPPLICATIONS",
            "/SP-",
        ])
        .creation_flags(CREATE_NO_WINDOW)
        .status()
        .context("could not start the Orange installer")?;
    Ok(status.code().unwrap_or(-1))
}

fn parent_has_exited(pid: u32) -> Result<bool> {
    let Some(handle) = open_parent(pid)? else {
        return Ok(true);
    };
    let result = unsafe { WaitForSingleObject(handle, 0) };
    let wait_error = (result == WAIT_FAILED).then(windows::core::Error::from_thread);
    unsafe {
        let _ = CloseHandle(handle);
    }
    if let Some(error) = wait_error {
        return Err(error).context("could not inspect the Orange process state");
    }
    Ok(result == WAIT_OBJECT_0)
}

fn apply_update(args: UpdateArgs) -> Result<()> {
    wait_for_parent(args.parent)?;
    verify_installer(&args.installer, &args.sha256)?;

    let code = run_installer(&args.installer)?;
    if !installer_succeeded(code) {
        bail!("Orange installer exited with code {code}");
    }

    let tray = app_binary(&args.install_dir);
    Command::new(&tray)
        .current_dir(&args.install_dir)
        .spawn()
        .with_context(|| format!("could not reopen {}", tray.display()))?;
    let _ = std::fs::remove_file(args.installer);
    Ok(())
}

fn write_failure(error: &anyhow::Error) {
    let Some(local) = std::env::var_os("LOCALAPPDATA") else {
        return;
    };
    let directory = PathBuf::from(local).join("orange");
    let _ = std::fs::create_dir_all(&directory);
    let mut message = format!("{error:#}");
    truncate_utf8(&mut message, 2_000);
    let _ = std::fs::write(directory.join("update-error.txt"), message);
}

/// The application binary, newest name first.
///
/// The updater that performs an upgrade is the one already installed, spawned
/// before the new installer runs, so it cannot be taught anything by the
/// release it is installing. A rename therefore has to be accepted by the
/// updater one release *before* it happens; otherwise the old updater reopens
/// a binary the new installer did not write, or -- because the installer has
/// no `[InstallDelete]` and leaves old files in place -- reopens the previous
/// version, which finds the same update waiting and loops.
const APP_BINARIES: [&str; 2] = ["orange-client.exe", "orange-tray.exe"];

/// Where the application actually landed, preferring the newest name present.
///
/// Falls back to the last candidate so a missing install still produces a
/// specific path to report rather than an empty one.
fn app_binary(install_dir: &Path) -> PathBuf {
    APP_BINARIES
        .iter()
        .map(|name| install_dir.join(name))
        .find(|candidate| candidate.exists())
        .unwrap_or_else(|| install_dir.join(APP_BINARIES[APP_BINARIES.len() - 1]))
}

fn truncate_utf8(value: &mut String, max_bytes: usize) {
    let mut end = value.len().min(max_bytes);
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    value.truncate(end);
}

fn main() {
    match UpdateArgs::parse(std::env::args_os()) {
        Ok(args) => {
            let retry = args.clone();
            if let Err(error) = apply_update(args) {
                write_failure(&error);
                if parent_has_exited(retry.parent).unwrap_or(false) {
                    let tray = app_binary(&retry.install_dir);
                    let _ = Command::new(tray).current_dir(retry.install_dir).spawn();
                }
            }
        }
        Err(error) => write_failure(&error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// This updater ships one release ahead of the rename it enables, so its
    /// whole job is to already know a name that does not exist yet.
    ///
    /// The installer has no `[InstallDelete]`, so an upgrade leaves the old
    /// binary next to the new one. Preferring the old name there would reopen
    /// the previous version, which would find the same update waiting and
    /// download it again -- an update loop, from code already on users' disks
    /// and therefore unfixable by the release doing the renaming.
    #[test]
    fn the_newest_binary_name_wins_when_an_upgrade_leaves_both_behind() {
        let dir = tempfile::tempdir().unwrap();

        // Nothing installed yet: still names a specific path to report.
        assert_eq!(app_binary(dir.path()), dir.path().join("orange-tray.exe"));

        // Only the old name, which is every install that exists today.
        std::fs::write(dir.path().join("orange-tray.exe"), b"old").unwrap();
        assert_eq!(app_binary(dir.path()), dir.path().join("orange-tray.exe"));

        // Both, which is what an upgrade across the rename actually leaves.
        std::fs::write(dir.path().join("orange-client.exe"), b"new").unwrap();
        assert_eq!(app_binary(dir.path()), dir.path().join("orange-client.exe"));
    }

    #[test]
    fn parses_complete_update_request() {
        let args = UpdateArgs::parse([
            "orange-updater.exe",
            "--installer",
            r"C:\Temp\orange-setup.exe",
            "--sha256",
            "A4A8D1D5F0F69CCB5B4BF31117B1103A17C78D5D7F1B4A2C8DDC2E1D3E86AB12",
            "--parent",
            "42",
            "--install-dir",
            r"C:\Users\tester\AppData\Local\Programs\orange",
        ])
        .unwrap();

        assert_eq!(args.parent, 42);
        assert!(args.installer.ends_with("orange-setup.exe"));
        assert!(args.install_dir.ends_with("orange"));
    }

    #[test]
    fn rejects_missing_duplicate_and_unknown_arguments() {
        assert!(UpdateArgs::parse(["updater", "--parent", "42"]).is_err());
        assert!(UpdateArgs::parse([
            "updater",
            "--parent",
            "42",
            "--parent",
            "43",
            "--installer",
            "a",
            "--sha256",
            &"A".repeat(64),
            "--install-dir",
            "b"
        ])
        .is_err());
        assert!(UpdateArgs::parse([
            "updater",
            "--wat",
            "x",
            "--parent",
            "42",
            "--installer",
            "a",
            "--sha256",
            &"A".repeat(64),
            "--install-dir",
            "b"
        ])
        .is_err());
    }

    #[test]
    fn accepts_only_successful_installer_exit_codes() {
        assert!(installer_succeeded(0));
        assert!(installer_succeeded(3010));
        assert!(!installer_succeeded(1));
        assert!(!installer_succeeded(1638));
    }

    #[test]
    fn verifies_installer_hash() {
        let directory = tempfile::tempdir().unwrap();
        let installer = directory.path().join("setup.exe");
        std::fs::write(&installer, b"orange beta").unwrap();
        let hash = sha256_file(&installer).unwrap();
        assert!(verify_installer(&installer, &hash).is_ok());
        assert!(verify_installer(&installer, &"0".repeat(64)).is_err());
    }

    #[test]
    fn current_process_is_not_treated_as_exited() {
        assert!(!parent_has_exited(std::process::id()).unwrap());
        assert!(parent_has_exited(u32::MAX).unwrap());
    }

    #[test]
    fn failure_messages_truncate_only_at_utf8_boundaries() {
        let mut message = "a".repeat(1_999) + "é trailing";
        truncate_utf8(&mut message, 2_000);

        assert_eq!(message, "a".repeat(1_999));
        assert!(message.len() <= 2_000);
    }

    #[test]
    fn parent_wait_is_bounded() {
        let started = std::time::Instant::now();
        assert!(wait_for_parent_with_timeout(std::process::id(), 1).is_err());
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn installer_process_receives_silent_arguments() {
        let directory = tempfile::tempdir().unwrap();
        let marker = directory.path().join("args.txt");
        let installer = directory.path().join("fixture.cmd");
        std::fs::write(
            &installer,
            format!(
                "@echo off\r\necho %* > \"{}\"\r\nexit /b 0\r\n",
                marker.display()
            ),
        )
        .unwrap();

        assert_eq!(run_installer(&installer).unwrap(), 0);
        let arguments = std::fs::read_to_string(marker).unwrap();
        assert!(arguments.contains("/VERYSILENT"));
        assert!(arguments.contains("/CLOSEAPPLICATIONS"));
    }
}
