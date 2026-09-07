//! Tests for [`super`], kept out of `update.rs` so the update state
//! machine and the cases that pin it down can be read separately.

use super::*;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

fn en() -> &'static crate::i18n::Catalog {
    crate::i18n::Locale::En.catalog()
}

struct ReleaseOnDrop(Option<mpsc::Sender<()>>);

impl ReleaseOnDrop {
    fn new(sender: mpsc::Sender<()>) -> Self {
        Self(Some(sender))
    }

    fn release(&mut self) {
        if let Some(sender) = self.0.take() {
            let _ = sender.send(());
        }
    }
}

impl Drop for ReleaseOnDrop {
    fn drop(&mut self) {
        self.release();
    }
}

fn update_info() -> UpdateInfo {
    UpdateInfo {
        version: Version::parse("9.0.0").unwrap(),
        installer_url: Url::parse("https://orangealpha0d8d5893e69a3.blob.core.windows.net/releases/orange-setup-9.0.0.exe").unwrap(),
        sha256: "A".repeat(64),
        notes: "Faster joining".into(),
    }
}

fn controller(status: UpdateStatus, job: Option<UpdateJob>) -> UpdateController {
    UpdateController {
        status,
        job,
        next_update_check: Instant::now() + UPDATE_CHECK_INTERVAL,
        last_checked: None,
    }
}

fn finished_worker() -> JoinHandle<()> {
    let worker = std::thread::spawn(|| {});
    while !worker.is_finished() {
        std::thread::yield_now();
    }
    worker
}

fn controller_with_event(status: UpdateStatus, event: UpdateEvent) -> UpdateController {
    let (sender, receiver) = mpsc::channel();
    sender.send(event).unwrap();
    controller(
        status,
        Some(UpdateJob {
            receiver,
            cancel: Arc::new(AtomicBool::new(false)),
            worker: Some(finished_worker()),
        }),
    )
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
            let (sender, receiver) = mpsc::channel();
            drop(sender);
            controller(
                status,
                Some(UpdateJob {
                    receiver,
                    cancel: Arc::new(AtomicBool::new(false)),
                    worker: Some(finished_worker()),
                }),
            )
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
fn polling_waits_until_a_terminal_background_job_is_finished_then_joins_it() {
    let (sender, receiver) = mpsc::channel();
    let (sent_tx, sent_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let (poll_returned_tx, poll_returned_rx) = mpsc::channel();
    let (exited_tx, exited_rx) = mpsc::channel();
    let exited = Arc::new(AtomicBool::new(false));
    let worker_exited = Arc::clone(&exited);
    let worker = std::thread::spawn(move || {
        sender.send(UpdateEvent::Checked(Ok(None))).unwrap();
        sent_tx.send(()).unwrap();
        let _ = release_rx.recv_timeout(Duration::from_secs(1));
        worker_exited.store(true, Ordering::SeqCst);
        let _ = exited_tx.send(());
    });
    sent_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("update event was not sent");
    let cancel = Arc::new(AtomicBool::new(false));
    let mut controller = controller(
        UpdateStatus::Checking,
        Some(UpdateJob {
            receiver,
            cancel,
            worker: Some(worker),
        }),
    );
    std::thread::scope(|scope| {
        let releaser = scope.spawn(move || {
            let poll_returned = poll_returned_rx.recv_timeout(Duration::from_secs(1));
            let _ = release_tx.send(());
            poll_returned.expect("poll_event did not return before the release timeout");
        });

        let result = controller.poll_event();
        let _ = poll_returned_tx.send(());
        assert!(result.is_none());
        assert!(controller.job.is_some());

        releaser.join().unwrap();
    });
    exited_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("update worker did not exit after release");
    assert!(exited.load(Ordering::SeqCst));
    let deadline = Instant::now() + Duration::from_secs(1);
    while controller
        .job
        .as_ref()
        .is_some_and(|job| !job.is_finished())
        && Instant::now() < deadline
    {
        std::thread::yield_now();
    }
    assert!(controller.job.as_ref().is_some_and(UpdateJob::is_finished));
    assert!(controller.poll_event().is_none());
    assert!(controller.job.is_none());
}

#[test]
fn controller_drop_cancels_and_joins_its_background_job() {
    let cancel = Arc::new(AtomicBool::new(false));
    let (cancelled_tx, cancelled_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let exited = Arc::new(AtomicBool::new(false));
    let worker_exited = Arc::clone(&exited);
    let (_sender, receiver) = mpsc::channel();
    let worker = std::thread::spawn(move || {
        let _ = release_rx.recv_timeout(Duration::from_secs(1));
        worker_exited.store(true, Ordering::Release);
    });
    let controller = controller(
        UpdateStatus::Checking,
        Some(UpdateJob {
            receiver,
            cancel,
            worker: Some(worker),
        }),
    );
    let old_cancel = Arc::clone(&controller.job.as_ref().unwrap().cancel);

    std::thread::scope(|scope| {
        let (caller_done_tx, caller_done_rx) = mpsc::channel();
        let observer = scope.spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(1);
            while !old_cancel.load(Ordering::Acquire) && Instant::now() < deadline {
                std::thread::yield_now();
            }
            let _ = cancelled_tx.send(old_cancel.load(Ordering::Acquire));
        });
        let caller = scope.spawn(move || {
            drop(controller);
            let _ = caller_done_tx.send(());
        });
        let mut release = ReleaseOnDrop::new(release_tx);

        assert!(cancelled_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("worker did not report cancellation"));
        assert!(matches!(
            caller_done_rx.try_recv(),
            Err(mpsc::TryRecvError::Empty)
        ));
        release.release();
        caller_done_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("controller drop did not return after worker release");
        assert!(exited.load(Ordering::Acquire));
        caller.join().unwrap();
        observer.join().unwrap();
    });
}

#[test]
fn controller_job_replacement_cancels_and_joins_the_old_worker() {
    let cancel = Arc::new(AtomicBool::new(false));
    let (cancelled_tx, cancelled_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let exited = Arc::new(AtomicBool::new(false));
    let worker_exited = Arc::clone(&exited);
    let (_sender, receiver) = mpsc::channel();
    let worker = std::thread::spawn(move || {
        let _ = release_rx.recv_timeout(Duration::from_secs(1));
        worker_exited.store(true, Ordering::Release);
    });
    let mut controller = controller(
        UpdateStatus::Failed {
            message: "offline".into(),
        },
        Some(UpdateJob {
            receiver,
            cancel,
            worker: Some(worker),
        }),
    );
    let old_cancel = Arc::clone(&controller.job.as_ref().unwrap().cancel);

    std::thread::scope(|scope| {
        let (caller_done_tx, caller_done_rx) = mpsc::channel();
        let observer = scope.spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(1);
            while !old_cancel.load(Ordering::Acquire) && Instant::now() < deadline {
                std::thread::yield_now();
            }
            let _ = cancelled_tx.send(old_cancel.load(Ordering::Acquire));
        });
        let controller = &mut controller;
        let caller = scope.spawn(move || {
            controller.stop_job();
            let _ = caller_done_tx.send(());
        });
        let mut release = ReleaseOnDrop::new(release_tx);

        assert!(cancelled_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("worker did not report cancellation"));
        assert!(matches!(
            caller_done_rx.try_recv(),
            Err(mpsc::TryRecvError::Empty)
        ));
        release.release();
        caller_done_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("controller replacement did not return after worker release");
        assert!(exited.load(Ordering::Acquire));
        caller.join().unwrap();
        observer.join().unwrap();
    });
    assert!(controller.job.is_none());
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
fn manual_check_is_the_periodic_check_without_the_deadline() {
    let current = UpdateStatus::Current;
    let available = UpdateStatus::Available(update_info());
    let failed = UpdateStatus::Failed {
        message: "offline".into(),
    };
    let downloading = UpdateStatus::Downloading(update_info());
    for (enabled, idle, status, expected) in [
        (true, true, &current, true),
        (true, true, &available, true),
        (true, true, &failed, true),
        (false, true, &current, false),
        (true, false, &current, false),
        (true, true, &UpdateStatus::Disabled, false),
        (true, true, &UpdateStatus::Checking, false),
        (true, true, &downloading, false),
    ] {
        assert_eq!(manual_check_due(enabled, idle, status), expected);
        // A manual check must never start from a state the periodic one
        // would refuse; only the deadline may differ.
        assert_eq!(
            manual_check_due(enabled, idle, status),
            periodic_check_due(enabled, idle, true, status)
        );
    }
}

#[test]
fn checked_ago_phrases_each_magnitude_and_singularises() {
    assert_eq!(checked_ago(en(), None), None);
    for (seconds, expected) in [
        (0u64, "just now"),
        (59, "just now"),
        (60, "1 minute ago"),
        (119, "1 minute ago"),
        (120, "2 minutes ago"),
        (3599, "59 minutes ago"),
        (3600, "1 hour ago"),
        (7199, "1 hour ago"),
        (7200, "2 hours ago"),
    ] {
        assert_eq!(
            checked_ago(en(), Some(Duration::from_secs(seconds))).as_deref(),
            Some(expected),
            "{seconds}s"
        );
    }
}

#[test]
fn settings_detail_describes_every_status() {
    let cases = [
        (UpdateStatus::Disabled, "off in this build"),
        (UpdateStatus::Checking, "Checking for updates"),
        (UpdateStatus::Current, "Up to date"),
        (UpdateStatus::Available(update_info()), "is available"),
        (UpdateStatus::Downloading(update_info()), "Downloading"),
        (
            UpdateStatus::Failed {
                message: "Could not check for updates".into(),
            },
            "Could not check for updates",
        ),
    ];
    for (status, expected) in cases {
        let detail = controller(status, None).settings_detail(en());
        assert!(detail.contains(expected), "{detail:?} lacks {expected:?}");
        assert!(!detail.is_empty());
        // These are captions, not sentences. A trailing period at 12px in a
        // dim colour is visual lint, and the screen mixed both styles.
        assert!(
            !detail.ends_with('.'),
            "{detail:?} should not end in a period"
        );
    }
}

#[test]
fn settings_detail_reports_when_the_last_check_happened() {
    let mut controller = controller(UpdateStatus::Current, None);
    // Before any check completes there is nothing truthful to report.
    assert_eq!(controller.settings_detail(en()), "Up to date");

    controller.last_checked = Instant::now().checked_sub(Duration::from_secs(120));
    assert_eq!(
        controller.settings_detail(en()),
        "Up to date \u{b7} last checked 2 minutes ago"
    );
}

#[test]
fn manual_check_is_refused_while_one_is_already_running() {
    let mut controller = controller(UpdateStatus::Checking, None);
    assert!(!controller.can_check_now());
    controller.check_now();
    // Still Checking: check_now must not restart or clobber a live job.
    assert!(matches!(controller.status(), UpdateStatus::Checking));
}

#[test]
fn settings_action_offers_install_for_available_and_nothing_while_busy() {
    // `Available` offers to install regardless of the compile-time channel;
    // it is unreachable when updates are off, since a disabled build never
    // starts a check. The check-offering states route through
    // `can_check_now`, which is false here because tests are not built with
    // ORANGE_UPDATE_CHANNEL set.
    assert_eq!(
        controller(UpdateStatus::Available(update_info()), None).settings_action(en()),
        Some("Update now")
    );
    for status in [
        UpdateStatus::Disabled,
        UpdateStatus::Checking,
        UpdateStatus::Downloading(update_info()),
        UpdateStatus::Current,
        UpdateStatus::Failed {
            message: "offline".into(),
        },
    ] {
        let controller = controller(status.clone(), None);
        assert_eq!(
            controller.settings_action(en()),
            None,
            "unexpected action for {status:?}"
        );
    }
}

#[test]
fn settings_action_is_inert_when_it_offers_nothing() {
    for status in [
        UpdateStatus::Checking,
        UpdateStatus::Downloading(update_info()),
        UpdateStatus::Disabled,
    ] {
        let mut controller = controller(status.clone(), None);
        assert!(controller.settings_action(en()).is_none());
        controller.activate_settings_action();
        assert_eq!(
            std::mem::discriminant(controller.status()),
            std::mem::discriminant(&status),
            "activating a offerless action changed {status:?}"
        );
    }
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
fn only_a_semantically_newer_version_is_offered() {
    let hash = "A".repeat(64);
    let offered = |version: &str, current: &str| {
        let url = format!(
            "https://orangealpha0d8d5893e69a3.blob.core.windows.net/releases/orange-setup-{version}.exe"
        );
        parse_update_manifest(&manifest(version, &hash, &url), current)
            .unwrap()
            .map(|info| info.version.to_string())
    };

    assert_eq!(offered("0.3.1", "0.3.0"), Some("0.3.1".into()));
    assert_eq!(offered("0.4.0", "0.3.9"), Some("0.4.0".into()));
    assert_eq!(offered("1.0.0", "0.9.9"), Some("1.0.0".into()));
    // The one-time move off the old -beta.N scheme. Every client already in
    // the wild has to see a plain 0.3.0 as newer than its pre-release, or it
    // is stranded on a version that will never be published again.
    assert_eq!(offered("0.3.0", "0.2.0-beta.11"), Some("0.3.0".into()));
    // Pre-release identifiers compare numerically, not as text: this is why
    // beta.10 ever superseded beta.9.
    assert_eq!(
        offered("0.2.0-beta.10", "0.2.0-beta.9"),
        Some("0.2.0-beta.10".into())
    );

    assert_eq!(offered("0.3.0", "0.3.0"), None);
    assert_eq!(offered("0.2.9", "0.3.0"), None);
    assert_eq!(offered("0.9.9", "1.0.0"), None);
    // A pre-release sorts below its own release, so it is not an upgrade.
    assert_eq!(offered("1.0.0-beta.1", "1.0.0"), None);
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
    let url =
        "https://orangealpha0d8d5893e69a3.blob.core.windows.net/releases/orange-setup-wrong.exe";
    assert!(parse_update_manifest(&manifest("0.2.0-beta.2", &hash, url), "0.2.0-beta.1").is_err());
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
    assert_eq!(
        copy_bounded(&mut exact, &mut exact_output, 4, None).unwrap(),
        4
    );
    assert_eq!(exact_output, b"1234");

    let mut oversized = &b"12345"[..];
    assert!(copy_bounded(&mut oversized, &mut Vec::new(), 4, None).is_err());
}

#[test]
fn cancelled_download_copy_removes_its_temporary_file() {
    struct CancelAfterFirstChunk {
        reads: usize,
        cancel: Arc<AtomicBool>,
    }

    impl Read for CancelAfterFirstChunk {
        fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
            self.reads += 1;
            if self.reads == 2 {
                self.cancel.store(true, Ordering::Release);
            }
            buffer[..4].copy_from_slice(b"data");
            Ok(4)
        }
    }

    let directory = tempfile::tempdir().unwrap();
    let temporary = directory.path().join("download.tmp");
    let destination = directory.path().join("orange-setup.exe");
    let cancel = Arc::new(AtomicBool::new(false));
    let mut reader = CancelAfterFirstChunk {
        reads: 0,
        cancel: Arc::clone(&cancel),
    };

    assert!(finish_download(
        &mut reader,
        &temporary,
        &destination,
        &"0".repeat(64),
        &cancel,
    )
    .is_err());
    assert_eq!(reader.reads, 2);
    assert!(!temporary.exists());
    assert!(!destination.exists());
}

#[test]
fn successful_download_copy_syncs_checksums_and_renames_the_temporary_file() {
    let bytes = b"known installer bytes";
    let expected_sha256 = format!("{:X}", Sha256::digest(bytes));
    let directory = tempfile::tempdir().unwrap();
    let temporary = directory.path().join("download.tmp");
    let destination = directory.path().join("orange-setup.exe");
    let cancel = AtomicBool::new(false);

    finish_download(
        &mut &bytes[..],
        &temporary,
        &destination,
        &expected_sha256,
        &cancel,
    )
    .unwrap();

    assert_eq!(std::fs::read(&destination).unwrap(), bytes);
    assert!(!temporary.exists());
}

#[test]
fn updater_handoff_uses_a_detached_temporary_copy() {
    let directory = tempfile::tempdir().unwrap();
    let install_dir = directory.path().join("installed");
    let temporary = directory.path().join("temporary");
    std::fs::create_dir_all(&install_dir).unwrap();
    std::fs::create_dir_all(&temporary).unwrap();
    std::fs::write(install_dir.join("orange-tray.exe"), b"client").unwrap();
    std::fs::write(install_dir.join("orange-updater.exe"), b"updater").unwrap();
    std::fs::write(install_dir.join("vcruntime140.dll"), b"runtime").unwrap();
    let installer = directory.path().join("orange-setup-0.2.0-beta.2.exe");
    std::fs::write(&installer, b"installer").unwrap();
    let info = UpdateInfo {
        version: Version::parse("0.2.0-beta.2").unwrap(),
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
        installer_url: Url::parse("https://orangealpha0d8d5893e69a3.blob.core.windows.net/releases/orange-setup-0.2.0-beta.2.exe").unwrap(),
        sha256: "A".repeat(64),
        notes: "Faster joining".into(),
    };
    assert_eq!(
        UpdateStatus::Available(info.clone()).action_label(en()),
        Some("Update now")
    );
    assert_eq!(UpdateStatus::Downloading(info).action_label(en()), None);
    assert_eq!(
        UpdateStatus::Failed {
            message: "offline".into(),
        }
        .action_label(en()),
        Some("Check again")
    );
    assert!(!UpdateStatus::Current.is_visible());
}
