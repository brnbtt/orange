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
use windows::Win32::Foundation::CloseHandle;
use windows::Win32::System::Threading::{
    OpenProcess, WaitForSingleObject, INFINITE, PROCESS_SYNCHRONIZE,
};

const CREATE_NO_WINDOW: u32 = 0x0800_0000;

#[derive(Debug, PartialEq, Eq)]
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

fn wait_for_parent(pid: u32) {
    unsafe {
        if let Ok(handle) = OpenProcess(PROCESS_SYNCHRONIZE, false, pid) {
            WaitForSingleObject(handle, INFINITE);
            let _ = CloseHandle(handle);
        }
    }
    std::thread::sleep(Duration::from_millis(200));
}

fn apply_update(args: UpdateArgs) -> Result<()> {
    verify_installer(&args.installer, &args.sha256)?;
    wait_for_parent(args.parent);
    verify_installer(&args.installer, &args.sha256)?;

    let status = Command::new(&args.installer)
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
    let code = status.code().unwrap_or(-1);
    if !installer_succeeded(code) {
        bail!("Orange installer exited with code {code}");
    }

    let tray = args.install_dir.join("orange-tray.exe");
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
    message.truncate(2_000);
    let _ = std::fs::write(directory.join("update-error.txt"), message);
}

fn main() {
    let result = UpdateArgs::parse(std::env::args_os()).and_then(apply_update);
    if let Err(error) = result {
        write_failure(&error);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
