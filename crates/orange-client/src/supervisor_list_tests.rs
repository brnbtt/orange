use super::*;

const CHILD_MODE: &str = "ORANGE_TEST_WINDOW_LIST";

fn command(mode: &str) -> Command {
    let mut command = Command::new(std::env::current_exe().unwrap());
    command.args([
        "--exact",
        "supervisor::list_tests::window_list_child",
        "--nocapture",
    ]);
    command.env(CHILD_MODE, mode);
    command
}

#[test]
fn window_list_child() {
    let Ok(mode) = std::env::var(CHILD_MODE) else {
        return;
    };
    match mode.as_str() {
        // Serial libtest writes its test-name prefix without a newline. Keep
        // fixture JSON on its own line, as the real orange list command does,
        // even when the child inherits RUST_TEST_THREADS=1 from a release run.
        "ok" => {
            println!();
            println!(
                r#"[{{"hwnd":7,"title":"Window","process":"test.exe","width":640,"height":480}}]"#
            );
        }
        "malformed" => println!("\n[not json"),
        "error" => {
            eprintln!("enumeration fixture failed");
            std::process::exit(7);
        }
        "wait" => {
            if let Ok(ready) = std::env::var("ORANGE_TEST_WINDOW_LIST_READY") {
                std::fs::write(ready, "ready").unwrap();
            }
            std::thread::sleep(Duration::from_secs(30));
        }
        "large" => {
            use std::io::Write;
            let _ = std::io::stdout().write_all(&vec![b'x'; 4 * 1024 * 1024 + 1]);
        }
        _ => panic!("unknown child fixture"),
    }
}

#[test]
fn window_enumeration_reads_and_reaps_a_successful_child() {
    // Enumerating on a worker must preserve the existing CLI JSON contract.
    let windows = list_windows_command(
        command("ok"),
        &AtomicBool::new(false),
        Duration::from_secs(3),
    )
    .unwrap();
    assert_eq!(windows.len(), 1);
    assert_eq!(windows[0].hwnd, 7);
    assert_eq!(windows[0].width, 640);
}

#[test]
fn window_enumeration_reports_child_and_parse_failures() {
    for mode in ["error", "malformed", "large"] {
        let error = list_windows_command(
            command(mode),
            &AtomicBool::new(false),
            Duration::from_secs(3),
        )
        .unwrap_err()
        .to_string();
        match mode {
            "error" => assert!(error.contains("enumeration fixture failed"), "{error}"),
            "large" => assert!(error.contains("output too large"), "{error}"),
            _ => assert!(error.contains("expected"), "{error}"),
        }
    }
}

#[test]
fn a_stalled_window_enumeration_is_killed_at_its_deadline() {
    // Command::output had no deadline; putting that on a thread alone would
    // leave an unjoinable job when the child stopped making progress.
    let error = list_windows_command(
        command("wait"),
        &AtomicBool::new(false),
        Duration::from_millis(100),
    )
    .unwrap_err();
    assert!(error.to_string().contains("timed out"));
}

#[test]
fn cancelling_window_enumeration_kills_the_child_before_its_deadline() {
    let directory = tempfile::tempdir().unwrap();
    let ready = directory.path().join("ready");
    let mut command = command("wait");
    command.env("ORANGE_TEST_WINDOW_LIST_READY", &ready);
    let cancel = AtomicBool::new(false);
    std::thread::scope(|scope| {
        let observer = scope.spawn(|| {
            let deadline = Instant::now() + Duration::from_secs(3);
            while !ready.exists() {
                assert!(Instant::now() < deadline, "child never started");
                std::thread::sleep(Duration::from_millis(1));
            }
            cancel.store(true, Ordering::Release);
        });
        let error = list_windows_command(command, &cancel, Duration::from_secs(5)).unwrap_err();
        assert!(error.to_string().contains("cancelled"));
        observer.join().unwrap();
    });
}
