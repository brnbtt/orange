//! Tests for [`super`], kept out of `window.rs` so the ownership and teardown policy and the
//! cases that pin it down can be read separately.

use super::{
    finish_window_startup, join_after_worker_completion, set_taskbar_identity, shutdown_policy,
    CleanupFailure, CleanupResult, PlaybackProfile, PlaybackWindow, ShutdownPolicy, WorkerFinish,
    APP_USER_MODEL_ID,
};
use crate::connection::{ConnectionEvent, ConnectionFailure, ConnectionStage};
use anyhow::anyhow;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::time::Duration;
use windows::Win32::Foundation::{HWND, LPARAM, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{GetDC, GetPixel, ReleaseDC};
use windows::Win32::UI::HiDpi::GetDpiForWindow;
use windows::Win32::UI::WindowsAndMessaging::{
    GetClientRect, IsWindowVisible, SendMessageTimeoutW, SMTO_ABORTIFHUNG, WM_CLOSE, WM_LBUTTONDOWN,
};

fn hidden_window(title: &str) -> PlaybackWindow {
    PlaybackWindow::spawn(title, PlaybackProfile::FriendViewer { cascade: 0 }).unwrap()
}

fn close_window(handle: &super::PlaybackWindowHandle) {
    let delivered = handle
        .with_hwnd(|current| unsafe {
            SendMessageTimeoutW(
                HWND(current as *mut _),
                WM_CLOSE,
                WPARAM(0),
                LPARAM(0),
                SMTO_ABORTIFHUNG,
                1_000,
                None,
            )
            .0 != 0
        })
        .unwrap_or(false);
    assert!(delivered);
}

fn wait_until_visible(hwnd: isize) -> bool {
    let deadline = std::time::Instant::now() + Duration::from_secs(1);
    while std::time::Instant::now() < deadline {
        if unsafe { IsWindowVisible(HWND(hwnd as *mut _)).as_bool() } {
            return true;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    false
}

fn shutdown_hidden(mut owner: PlaybackWindow) {
    assert_eq!(
        owner.try_shutdown_worker(Duration::from_secs(5)),
        WorkerFinish::Joined(Ok(()))
    );
}

#[test]
fn taskbar_identity_matches_client_process() {
    assert_eq!(APP_USER_MODEL_ID, "brnbtt.orange");
    set_taskbar_identity().expect("taskbar identity should be accepted by Windows");
}

#[test]
fn shutdown_event_wakes_creator_before_join_and_invalidates_stale_handle() {
    let mut owner = hidden_window("orange owner drop test");
    let handle = owner.handle();
    assert!(handle.hwnd().is_some());

    assert_eq!(
        owner.try_shutdown_worker(Duration::from_secs(5)),
        WorkerFinish::Joined(Ok(()))
    );

    assert_eq!(handle.hwnd(), None);
    assert!(!handle.is_alive());
    assert!(!handle.is_responsive());
    handle.reveal();
    handle.set_source_size(1920, 1080);
}

#[test]
fn dropping_passive_handles_does_not_stop_the_window() {
    let owner = hidden_window("orange passive handle test");
    let handle = owner.handle();
    let hwnd = handle.hwnd();

    drop(handle.clone());

    assert_eq!(owner.handle().hwnd(), hwnd);
    assert!(owner.handle().is_alive());
    shutdown_hidden(owner);
}

#[test]
fn close_hides_and_marks_dead_but_reserves_hwnd_until_owner_drop() {
    let owner = hidden_window("orange close reservation test");
    let handle = owner.handle();
    let hwnd = handle.hwnd().expect("window did not publish its HWND");

    let delivered = handle
        .with_hwnd(|current| unsafe {
            SendMessageTimeoutW(
                HWND(current as *mut _),
                WM_CLOSE,
                WPARAM(0),
                LPARAM(0),
                SMTO_ABORTIFHUNG,
                1_000,
                None,
            )
            .0 != 0
        })
        .unwrap_or(false);

    assert!(delivered);
    assert!(!handle.is_alive());
    assert_eq!(handle.hwnd(), Some(hwnd));
    assert!(!unsafe { IsWindowVisible(HWND(hwnd as *mut _)).as_bool() });

    shutdown_hidden(owner);
    assert_eq!(handle.hwnd(), None);
}

#[test]
fn reveal_after_close_cannot_show_or_revive_window() {
    let owner = hidden_window("orange close before reveal test");
    let handle = owner.handle();
    let hwnd = handle.hwnd().expect("window did not publish its HWND");
    handle
        .with_hwnd(|current| unsafe {
            let _ = SendMessageTimeoutW(
                HWND(current as *mut _),
                WM_CLOSE,
                WPARAM(0),
                LPARAM(0),
                SMTO_ABORTIFHUNG,
                1_000,
                None,
            );
        })
        .expect("window disappeared before close");

    handle.reveal();
    handle
        .with_hwnd(|current| unsafe {
            let _ = SendMessageTimeoutW(
                HWND(current as *mut _),
                super::REVEAL_MESSAGE,
                WPARAM(0),
                LPARAM(0),
                SMTO_ABORTIFHUNG,
                1_000,
                None,
            );
        })
        .expect("window disappeared before reveal check");

    assert!(!handle.is_alive());
    assert!(!unsafe { IsWindowVisible(HWND(hwnd as *mut _)).as_bool() });
    shutdown_hidden(owner);
}

#[test]
fn connection_window_reveals_and_responds_before_any_video_sink_attaches() {
    let owner = hidden_window("orange early connection feedback test");
    let handle = owner.handle();
    let hwnd = handle.hwnd().expect("window did not publish its HWND");

    handle.begin_connection();
    let revealed_at = std::time::Instant::now();
    handle.reveal();

    assert!(wait_until_visible(hwnd));
    assert!(handle.is_responsive());
    assert!(revealed_at.elapsed() < Duration::from_millis(300));
    assert_eq!(
        handle.connection_stage(),
        Some(ConnectionStage::JoiningRoom)
    );
    shutdown_hidden(owner);
}

#[test]
fn connection_surface_is_actually_presented_to_the_native_client() {
    let owner = hidden_window("orange native connection paint test");
    let handle = owner.handle();
    let hwnd = handle.hwnd().expect("window did not publish its HWND");
    handle.begin_connection();
    handle.reveal();
    assert!(wait_until_visible(hwnd));
    assert!(handle.is_responsive());

    let hwnd = HWND(hwnd as *mut _);
    let mut client = RECT::default();
    unsafe { GetClientRect(hwnd, &mut client) }.unwrap();
    let dpi = unsafe { GetDpiForWindow(hwnd) }.max(96) as f32 / 96.0;
    let x = (client.right - client.left) / 2;
    let y = (client.bottom - client.top) / 2 - (75.0 * dpi).round() as i32;
    let dc = unsafe { GetDC(Some(hwnd)) };
    let pixel = unsafe { GetPixel(dc, x, y) };
    unsafe { ReleaseDC(Some(hwnd), dc) };

    let red = pixel.0 & 0xff;
    let green = (pixel.0 >> 8) & 0xff;
    let blue = (pixel.0 >> 16) & 0xff;
    assert!(
        red > 240 && (60..130).contains(&green) && blue < 70,
        "expected orange accent, got COLORREF #{red:02x}{green:02x}{blue:02x} at ({x}, {y})"
    );
    shutdown_hidden(owner);
}

#[test]
fn connection_surface_close_control_closes_before_video_exists() {
    let mut owner = hidden_window("orange pre-video close control test");
    let handle = owner.handle();
    let hwnd = handle.hwnd().expect("window did not publish its HWND");
    handle.begin_connection();
    handle.reveal();
    assert!(wait_until_visible(hwnd));

    let mut client = RECT::default();
    unsafe { GetClientRect(HWND(hwnd as *mut _), &mut client) }.unwrap();
    let dpi = unsafe { GetDpiForWindow(HWND(hwnd as *mut _)) }.max(96) as f32 / 96.0;
    let x = client.right - (38.0 * dpi).round() as i32;
    let y = (38.0 * dpi).round() as i32;
    let point = (x as u16 as u32) | ((y as u16 as u32) << 16);
    unsafe {
        SendMessageTimeoutW(
            HWND(hwnd as *mut _),
            WM_LBUTTONDOWN,
            WPARAM(0),
            LPARAM(point as isize),
            SMTO_ABORTIFHUNG,
            1_000,
            None,
        )
    };
    let deadline = std::time::Instant::now() + Duration::from_secs(1);
    while handle.is_alive() && std::time::Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(5));
    }

    assert!(!handle.is_alive());
    assert_eq!(
        owner.try_shutdown_worker(Duration::from_secs(1)),
        WorkerFinish::Joined(Ok(()))
    );
}

#[test]
fn connected_media_keeps_the_original_playback_hwnd() {
    let owner = hidden_window("orange same HWND connection test");
    let handle = owner.handle();
    let hwnd = handle.hwnd().expect("window did not publish its HWND");
    handle.begin_connection();

    for event in [
        ConnectionEvent::StreamInfo,
        ConnectionEvent::IceChecking,
        ConnectionEvent::IceConnected,
        ConnectionEvent::PeerConnected,
        ConnectionEvent::FirstVideoFrame,
    ] {
        handle.connection_event(event);
        assert_eq!(handle.hwnd(), Some(hwnd));
    }

    assert_eq!(handle.connection_stage(), Some(ConnectionStage::Connected));
    shutdown_hidden(owner);
}

#[test]
fn connection_stage_diagnostics_are_timed_and_privacy_safe() {
    let owner = hidden_window("orange connection diagnostics test");
    let handle = owner.handle();

    let captured = crate::media_diagnostics::capture_diagnostics(|| {
        handle.begin_connection();
        handle.connection_event(ConnectionEvent::StreamInfo);
        handle.connection_event(ConnectionEvent::IceChecking);
        handle.connection_event(ConnectionEvent::Failed(ConnectionFailure::Network));
        handle.connection_event(ConnectionEvent::PeerConnected);
    });

    assert_eq!(captured.len(), 4);
    assert_eq!(captured[0]["payload"]["stage"], "joining-room");
    assert_eq!(captured[1]["payload"]["stage"], "exchanging-stream-details");
    assert_eq!(captured[2]["payload"]["stage"], "finding-direct-route");
    assert_eq!(captured[3]["payload"]["stage"], "failed-network");
    for record in captured {
        assert_eq!(record["event"], "connection-stage");
        assert!(record["payload"]["event"].is_string());
        assert!(record["payload"]["elapsed_ms"].is_number());
        assert!(record["payload"]["previous_stage_ms"].is_number());
        let serialized = record.to_string();
        for sensitive in ["candidate:", "a=", "SENSITIVE-ROOM-CODE", "192.168."] {
            assert!(!serialized.contains(sensitive));
        }
    }
    shutdown_hidden(owner);
}

#[test]
fn connection_failure_is_painted_before_the_update_returns() {
    let owner = hidden_window("orange synchronous connection failure test");
    let handle = owner.handle();
    let hwnd = handle.hwnd().expect("window did not publish its HWND");
    handle.begin_connection();
    handle.reveal();
    assert!(wait_until_visible(hwnd));
    assert!(handle.is_responsive());

    let hwnd = HWND(hwnd as *mut _);
    let mut client = RECT::default();
    unsafe { GetClientRect(hwnd, &mut client) }.unwrap();
    let width = (client.right - client.left) as u32;
    let height = (client.bottom - client.top) as u32;
    let dpi = unsafe { GetDpiForWindow(hwnd) }.max(96) as f32 / 96.0;
    let joining =
        super::connection_surface::render(width, height, dpi, ConnectionStage::JoiningRoom)
            .unwrap();
    let failed = super::connection_surface::render(
        width,
        height,
        dpi,
        ConnectionStage::Failed(ConnectionFailure::Network),
    )
    .unwrap();
    let pixel_index = failed
        .data()
        .as_chunks::<4>()
        .0
        .iter()
        .zip(joining.data().as_chunks::<4>().0)
        .position(|(failed, joining)| {
            failed != joining && failed[..3] != [7, 7, 8] && joining[..3] == [7, 7, 8]
        })
        .expect("failure copy did not produce a distinct pixel");
    let x = (pixel_index as u32 % width) as i32;
    let y = (pixel_index as u32 / width) as i32;
    let expected = &failed.data()[pixel_index * 4..pixel_index * 4 + 3];

    handle.connection_event(ConnectionEvent::Failed(ConnectionFailure::Network));

    let dc = unsafe { GetDC(Some(hwnd)) };
    let actual = unsafe { GetPixel(dc, x, y) };
    unsafe { ReleaseDC(Some(hwnd), dc) };
    assert_eq!(
        [
            (actual.0 & 0xff) as u8,
            ((actual.0 >> 8) & 0xff) as u8,
            ((actual.0 >> 16) & 0xff) as u8,
        ],
        expected
    );
    shutdown_hidden(owner);
}

#[test]
fn closing_during_every_connection_stage_completes_owner_teardown_promptly() {
    let stages = [
        vec![],
        vec![ConnectionEvent::StreamInfo],
        vec![ConnectionEvent::IceChecking],
        vec![ConnectionEvent::IceConnected],
        vec![ConnectionEvent::PeerConnected],
        vec![ConnectionEvent::FirstVideoFrame],
        vec![ConnectionEvent::Failed(ConnectionFailure::Network)],
    ];

    for (index, events) in stages.into_iter().enumerate() {
        let mut owner = hidden_window(&format!("orange connection close stage {index}"));
        let handle = owner.handle();
        handle.begin_connection();
        for event in events {
            handle.connection_event(event);
        }
        handle.reveal();
        close_window(&handle);

        let started = std::time::Instant::now();
        assert_eq!(
            owner.try_shutdown_worker(Duration::from_secs(1)),
            WorkerFinish::Joined(Ok(()))
        );
        assert!(started.elapsed() < Duration::from_secs(1));
        assert!(!handle.is_alive());
        assert_eq!(handle.hwnd(), None);
    }
}

#[test]
fn poisoned_native_slot_is_recovered() {
    let owner = hidden_window("orange poisoned HWND slot test");
    let handle = owner.handle();
    let hwnd = handle.hwnd();
    let native = handle.native.clone();
    let poisoner = std::thread::spawn(move || {
        let _slot = native.hwnd.lock().unwrap();
        panic!("poison native HWND slot");
    });
    assert!(poisoner.join().is_err());

    assert_eq!(handle.hwnd(), hwnd);
    shutdown_hidden(owner);
    assert_eq!(handle.hwnd(), None);
}

#[test]
fn startup_error_joins_window_worker_without_scheduling_delay() {
    let (startup, ready) = mpsc::sync_channel(1);
    let (completed, completion) = mpsc::sync_channel::<CleanupResult>(1);
    let (release, released) = mpsc::sync_channel(1);
    let (blocked, worker_blocked) = mpsc::sync_channel(1);
    let worker_finished = Arc::new(AtomicBool::new(false));
    let worker_finished_in_thread = worker_finished.clone();
    let worker = std::thread::spawn(move || {
        startup.send(Err(anyhow!("creation failed"))).unwrap();
        blocked.send(()).unwrap();
        released.recv().unwrap();
        worker_finished_in_thread.store(true, Ordering::Release);
        completed.send(Ok(())).unwrap();
    });
    let mut worker = Some(worker);

    worker_blocked.recv_timeout(Duration::from_secs(5)).unwrap();
    release.send(()).unwrap();
    assert!(finish_window_startup(ready, &completion, &mut worker).is_err());
    assert!(worker.is_none());
    assert!(worker_finished.load(Ordering::Acquire));
}

#[test]
fn completion_acknowledgement_gates_worker_join() {
    let (completed, completion) = mpsc::sync_channel::<CleanupResult>(1);
    let worker_finished = Arc::new(AtomicBool::new(false));
    let worker_finished_in_thread = worker_finished.clone();
    let worker = std::thread::spawn(move || {
        worker_finished_in_thread.store(true, Ordering::Release);
        completed.send(Ok(())).unwrap();
    });
    let mut worker = Some(worker);
    let mut cleanup = None;

    assert_eq!(
        join_after_worker_completion(
            &completion,
            &mut cleanup,
            &mut worker,
            std::time::Instant::now() + Duration::from_secs(5),
            "test"
        ),
        WorkerFinish::Joined(Ok(()))
    );
    assert!(worker.is_none());
    assert!(worker_finished.load(Ordering::Acquire));
}

#[test]
fn completion_timeout_retains_worker_for_explicit_retry() {
    let (completed, completion) = mpsc::sync_channel::<CleanupResult>(1);
    let (release, released) = mpsc::sync_channel(1);
    let worker = std::thread::spawn(move || {
        released.recv().unwrap();
        completed.send(Ok(())).unwrap();
    });
    let mut worker = Some(worker);
    let mut cleanup = None;

    assert_eq!(
        join_after_worker_completion(
            &completion,
            &mut cleanup,
            &mut worker,
            std::time::Instant::now(),
            "timeout test"
        ),
        WorkerFinish::Pending
    );
    assert!(worker.is_some());
    release.send(()).unwrap();
    assert_eq!(
        join_after_worker_completion(
            &completion,
            &mut cleanup,
            &mut worker,
            std::time::Instant::now() + Duration::from_secs(5),
            "timeout retry test"
        ),
        WorkerFinish::Joined(Ok(()))
    );
    assert!(worker.is_none());
}

#[test]
fn acknowledged_but_running_worker_is_retained_for_retry() {
    let (completed, completion) = mpsc::sync_channel::<CleanupResult>(1);
    let (blocked, worker_blocked) = mpsc::sync_channel(1);
    let (release, released) = mpsc::sync_channel(1);
    let worker = std::thread::spawn(move || {
        completed.send(Ok(())).unwrap();
        blocked.send(()).unwrap();
        released.recv().unwrap();
    });
    let mut worker = Some(worker);
    let mut cleanup = None;
    worker_blocked.recv_timeout(Duration::from_secs(5)).unwrap();

    assert_eq!(
        join_after_worker_completion(
            &completion,
            &mut cleanup,
            &mut worker,
            std::time::Instant::now(),
            "thread wait timeout test"
        ),
        WorkerFinish::Pending
    );
    assert_eq!(cleanup, Some(Ok(())));
    assert!(worker.is_some());

    release.send(()).unwrap();
    assert_eq!(
        join_after_worker_completion(
            &completion,
            &mut cleanup,
            &mut worker,
            std::time::Instant::now() + Duration::from_secs(5),
            "thread wait retry test"
        ),
        WorkerFinish::Joined(Ok(()))
    );
    assert!(worker.is_none());
}

#[test]
fn destroy_failure_completion_is_preserved_through_join() {
    let (completed, completion) = mpsc::sync_channel::<CleanupResult>(1);
    let worker = std::thread::spawn(move || {
        completed
            .send(Err(CleanupFailure::DestroyFailed(87)))
            .unwrap();
    });
    let mut worker = Some(worker);
    let mut cleanup = None;

    assert_eq!(
        join_after_worker_completion(
            &completion,
            &mut cleanup,
            &mut worker,
            std::time::Instant::now() + Duration::from_secs(5),
            "destroy failure test"
        ),
        WorkerFinish::Joined(Err(CleanupFailure::DestroyFailed(87)))
    );
    assert!(worker.is_none());
}

#[test]
fn shutdown_policy_fails_fast_on_every_unresolved_native_outcome() {
    assert_eq!(
        shutdown_policy(&WorkerFinish::Joined(Ok(()))),
        ShutdownPolicy::Complete
    );
    assert_eq!(
        shutdown_policy(&WorkerFinish::Joined(Err(CleanupFailure::DestroyFailed(
            87
        )))),
        ShutdownPolicy::Abort
    );
    assert_eq!(
        shutdown_policy(&WorkerFinish::Pending),
        ShutdownPolicy::Abort
    );
    assert_eq!(
        shutdown_policy(&WorkerFinish::SelfJoin),
        ShutdownPolicy::Abort
    );
    assert_eq!(
        shutdown_policy(&WorkerFinish::CompletionDisconnected),
        ShutdownPolicy::Abort
    );
    assert_eq!(
        shutdown_policy(&WorkerFinish::ThreadWaitFailed(6)),
        ShutdownPolicy::Abort
    );
}
