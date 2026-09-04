//! Test-only helpers. `main.rs` declares this module under `#[cfg(test)]`, so
//! nothing here reaches a shipped binary despite sitting beside the production
//! modules. Used by the tests in `webrtc/workers.rs` and `peer/host_branch.rs`,
//! which spawn a child process so a hang fails on a deadline instead of
//! blocking the suite.

use std::io;
use std::process::{Child, Command, ExitStatus};
use std::time::{Duration, Instant};

pub(crate) fn run_in_bounded_subprocess(env: &str, test: &str) -> bool {
    if std::env::var_os(env).is_some() {
        return false;
    }
    let mut child = Command::new(std::env::current_exe().expect("test executable is unavailable"))
        .args(["--exact", test, "--nocapture"])
        .env(env, "1")
        .spawn()
        .unwrap_or_else(|error| panic!("could not spawn child test {test}: {error}"));
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match child.try_wait() {
            Ok(Some(status)) if status.success() => return true,
            Ok(Some(status)) => panic!("child test failed with terminal status {status}: {test}"),
            Ok(None) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(10));
            }
            Ok(None) => {
                terminate_and_report(&mut child, format!("child test exceeded deadline: {test}"))
            }
            Err(error) => terminate_and_report(
                &mut child,
                format!("could not poll child test {test}: {error}"),
            ),
        }
    }
}

fn terminate_and_report(child: &mut Child, reason: String) -> ! {
    let kill = child
        .kill()
        .map(|()| "kill succeeded".to_string())
        .unwrap_or_else(|error| format!("kill failed: {error}"));
    match wait_uninterrupted(child) {
        Ok(status) => panic!("{reason}; {kill}; child terminal status confirmed: {status}"),
        Err(error) => panic!("{reason}; {kill}; child reaping failed after retries: {error}"),
    }
}

fn wait_uninterrupted(child: &mut Child) -> io::Result<ExitStatus> {
    loop {
        match child.wait() {
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            result => return result,
        }
    }
}
