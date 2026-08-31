//! The viewer window.
//!
//! A borderless, rounded window that hosts the video directly. GStreamer's
//! `d3d11videosink` implements `GstVideoOverlay`, so it renders into an HWND we
//! own rather than creating its own bare window.
//!
//! Why not GPUI here: GPUI expects to own its window and render loop, and
//! there is no supported way to composite GStreamer's D3D11 output underneath
//! it. Video stays native; GPUI is the right tool for the host-side tray panel,
//! where there is no video surface to share.
//!
//! Win32 requires a window's message loop to run on the thread that created
//! it, so the window lives on its own thread and hands the HWND back.

use anyhow::{bail, Result};
use std::io::Write;
use std::os::windows::io::AsRawHandle;
use std::sync::{
    atomic::{AtomicBool, AtomicU32, Ordering},
    mpsc, Arc, Condvar, Mutex, MutexGuard,
};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};
use windows::core::PCWSTR;
use windows::Win32::Foundation::{
    CloseHandle, GetLastError, ERROR_SUCCESS, HANDLE, HWND, LPARAM, WAIT_FAILED, WAIT_OBJECT_0,
    WAIT_TIMEOUT, WPARAM,
};
use windows::Win32::Graphics::Gdi::{
    EnumDisplaySettingsW, GetMonitorInfoW, MonitorFromWindow, DEVMODEW, ENUM_CURRENT_SETTINGS,
    MONITORINFOEXW, MONITOR_DEFAULTTONEAREST, MONITOR_DEFAULTTOPRIMARY,
};
use windows::Win32::System::Threading::{CreateEventW, SetEvent, WaitForSingleObject};
use windows::Win32::UI::HiDpi::{
    SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
};
use windows::Win32::UI::WindowsAndMessaging::{
    PostMessageW, SendMessageTimeoutW, SMTO_ABORTIFHUNG, WM_APP, WM_NULL,
};

mod native;

/// Opt out of DPI virtualisation, before any window exists.
///
/// Without this Windows lies to us about the screen size and stretches our
/// window's backing surface to fit the real one. On a 4K display at 150% that
/// means `d3d11videosink` renders into a 2560x1440 surface which DWM then
/// upscales - throwing away precisely the detail this project exists to
/// deliver. The bitrate is spent and then discarded at the last step.
pub fn set_dpi_aware() {
    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }
}

/// Configured refresh rate of the monitor containing the captured window.
/// Whole-screen capture (`hwnd == 0`) follows the primary display.
pub fn target_refresh_rate(hwnd: isize) -> Option<u32> {
    unsafe {
        let monitor = MonitorFromWindow(
            HWND(hwnd as *mut _),
            if hwnd == 0 {
                MONITOR_DEFAULTTOPRIMARY
            } else {
                MONITOR_DEFAULTTONEAREST
            },
        );
        let mut info = MONITORINFOEXW::default();
        info.monitorInfo.cbSize = std::mem::size_of::<MONITORINFOEXW>() as u32;
        if !GetMonitorInfoW(monitor, &mut info.monitorInfo).as_bool() {
            return None;
        }

        let mut mode = DEVMODEW {
            dmSize: std::mem::size_of::<DEVMODEW>() as u16,
            ..Default::default()
        };
        if !EnumDisplaySettingsW(
            PCWSTR(info.szDevice.as_ptr()),
            ENUM_CURRENT_SETTINGS,
            &mut mode,
        )
        .as_bool()
        {
            return None;
        }
        (mode.dmDisplayFrequency > 1).then_some(mode.dmDisplayFrequency)
    }
}

const REVEAL_MESSAGE: u32 = WM_APP + 1;
const ASPECT_MESSAGE: u32 = WM_APP + 2;
const WORKER_COMPLETION_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PlaybackProfile {
    FriendViewer { cascade: u32 },
    LiveMonitor,
}

impl PlaybackProfile {
    fn envelope(self) -> (i32, i32) {
        match self {
            Self::FriendViewer { .. } => (1280, 720),
            Self::LiveMonitor => (480, 270),
        }
    }

    pub(crate) fn initial_volume(self) -> f64 {
        0.3
    }

    pub(crate) fn starts_muted(self) -> bool {
        matches!(self, Self::LiveMonitor)
    }

    pub(crate) fn persistent_live_status(self) -> bool {
        matches!(self, Self::LiveMonitor)
    }

    fn always_on_top(self) -> bool {
        matches!(self, Self::LiveMonitor)
    }

    fn activates_on_reveal(self) -> bool {
        matches!(self, Self::FriendViewer { .. })
    }
}

/// Unique owner of one native playback window and its creator thread.
pub struct PlaybackWindow {
    handle: PlaybackWindowHandle,
    shutdown: Arc<ShutdownEvent>,
    completed: mpsc::Receiver<CleanupResult>,
    cleanup: Option<CleanupResult>,
    worker: Option<JoinHandle<()>>,
}

/// Passive access for media callbacks; dropping it never shuts down the window.
#[derive(Clone)]
pub struct PlaybackWindowHandle {
    native: Arc<NativeWindowState>,
    overlay: crate::overlay::SharedOverlay,
}

struct NativeWindowState {
    hwnd: Mutex<Option<isize>>,
    alive: AtomicBool,
    context_cleanup_error: AtomicU32,
}

struct ShutdownEvent {
    handle: HANDLE,
    requested: Mutex<bool>,
    requested_changed: Condvar,
}

// SAFETY: Win32 event handles may be signaled and waited from different
// threads. The Arc-held wrapper closes the handle only after all such users
// have dropped their references.
unsafe impl Send for ShutdownEvent {}
// SAFETY: SetEvent and waits permit concurrent access to the same event, and
// the Rust fallback state is protected by its mutex.
unsafe impl Sync for ShutdownEvent {}

impl ShutdownEvent {
    fn new() -> Result<Self> {
        // SAFETY: null security attributes/name request an unnamed event owned
        // by this process. This wrapper takes sole ownership of the result.
        let handle = unsafe { CreateEventW(None, true, false, PCWSTR::null()) }?;
        Ok(Self {
            handle,
            requested: Mutex::new(false),
            requested_changed: Condvar::new(),
        })
    }

    fn signal(&self) -> windows::core::Result<()> {
        *self
            .requested
            .lock()
            .unwrap_or_else(|poison| poison.into_inner()) = true;
        self.requested_changed.notify_all();
        // SAFETY: Arc ownership keeps this event handle open for the call.
        unsafe { SetEvent(self.handle) }
    }

    fn wait_for_request(&self) {
        let mut requested = self
            .requested
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        while !*requested {
            requested = self
                .requested_changed
                .wait(requested)
                .unwrap_or_else(|poison| poison.into_inner());
        }
    }

    fn is_requested(&self) -> bool {
        *self
            .requested
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
    }
}

impl Drop for ShutdownEvent {
    fn drop(&mut self) {
        // SAFETY: this wrapper is the event handle's sole RAII owner, and Arc
        // ensures no waiter or signaler remains when Drop runs.
        if let Err(error) = unsafe { CloseHandle(self.handle) } {
            let _ = writeln!(
                std::io::stderr().lock(),
                "[window] failed to close shutdown event: {error}"
            );
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CleanupFailure {
    DestroyFailed(u32),
    ContextCleanupFailed(u32),
    WorkerPanicked,
}

type CleanupResult = std::result::Result<(), CleanupFailure>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WorkerFinish {
    Joined(CleanupResult),
    Pending,
    SelfJoin,
    CompletionDisconnected,
    ThreadWaitFailed(u32),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ShutdownPolicy {
    Complete,
    Abort,
}

fn shutdown_policy(finish: &WorkerFinish) -> ShutdownPolicy {
    match finish {
        WorkerFinish::Joined(Ok(())) => ShutdownPolicy::Complete,
        WorkerFinish::Joined(Err(_))
        | WorkerFinish::Pending
        | WorkerFinish::SelfJoin
        | WorkerFinish::CompletionDisconnected
        | WorkerFinish::ThreadWaitFailed(_) => ShutdownPolicy::Abort,
    }
}

impl NativeWindowState {
    fn new() -> Self {
        Self {
            hwnd: Mutex::new(None),
            alive: AtomicBool::new(true),
            context_cleanup_error: AtomicU32::new(ERROR_SUCCESS.0),
        }
    }

    fn lock_hwnd(&self) -> MutexGuard<'_, Option<isize>> {
        self.hwnd
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
    }

    fn with_hwnd<R>(&self, action: impl FnOnce(isize) -> R) -> Option<R> {
        let slot = self.lock_hwnd();
        slot.map(action)
    }

    fn install(&self, hwnd: isize) {
        *self.lock_hwnd() = Some(hwnd);
    }

    fn invalidate(&self, hwnd: isize) {
        let mut slot = self.lock_hwnd();
        if *slot == Some(hwnd) {
            *slot = None;
        }
    }
}

impl PlaybackWindow {
    /// Create a playback window and run its message loop on a dedicated thread.
    pub fn spawn(title: &str, profile: PlaybackProfile) -> Result<Self> {
        Self::spawn_with_envelope(title, profile, profile.envelope())
    }

    /// The design harness can request a particular shell size while retaining
    /// the friend-viewer presentation and interaction behavior.
    pub fn spawn_preview(title: &str, width: i32, height: i32) -> Result<Self> {
        Self::spawn_with_envelope(
            title,
            PlaybackProfile::FriendViewer { cascade: 0 },
            (width, height),
        )
    }

    fn spawn_with_envelope(
        title: &str,
        profile: PlaybackProfile,
        envelope: (i32, i32),
    ) -> Result<Self> {
        let overlay = std::sync::Arc::new(std::sync::Mutex::new(
            crate::overlay::OverlayState::new(profile),
        ));
        let native = Arc::new(NativeWindowState::new());
        let window =
            native::spawn_window(title, envelope, profile, overlay.clone(), native.clone())?;
        Ok(Self {
            handle: PlaybackWindowHandle { native, overlay },
            shutdown: window.shutdown,
            completed: window.completed,
            cleanup: None,
            worker: Some(window.worker),
        })
    }

    pub fn handle(&self) -> PlaybackWindowHandle {
        self.handle.clone()
    }

    fn try_shutdown_worker(&mut self, timeout: Duration) -> WorkerFinish {
        let Some(worker) = self.worker.as_ref() else {
            return self
                .cleanup
                .map(WorkerFinish::Joined)
                .unwrap_or(WorkerFinish::CompletionDisconnected);
        };
        if worker.thread().id() == std::thread::current().id() {
            return WorkerFinish::SelfJoin;
        }
        self.handle.native.alive.store(false, Ordering::Release);
        if let Err(error) = self.shutdown.signal() {
            let _ = writeln!(
                std::io::stderr().lock(),
                "[window] failed to signal shutdown event: {error}"
            );
        }
        let deadline = Instant::now()
            .checked_add(timeout)
            .unwrap_or_else(Instant::now);
        join_after_worker_completion(
            &self.completed,
            &mut self.cleanup,
            &mut self.worker,
            deadline,
            "shutdown",
        )
    }
}

impl Drop for PlaybackWindow {
    fn drop(&mut self) {
        let finish = self.try_shutdown_worker(WORKER_COMPLETION_TIMEOUT);
        if shutdown_policy(&finish) == ShutdownPolicy::Abort {
            fail_fast_native_cleanup("shutdown", finish);
        }
    }
}

fn fail_fast_native_cleanup(phase: &str, finish: WorkerFinish) -> ! {
    // Returning would drop or detach a thread that may still own a live HWND
    // and GWLP_USERDATA allocation. Terminating is safer than allowing media
    // callbacks to continue with unprovable native ownership.
    let _ = writeln!(
        std::io::stderr().lock(),
        "[window] unrecoverable native cleanup failure during {phase}: {finish:?}; aborting to avoid a live HWND/context leak"
    );
    std::process::abort()
}

impl PlaybackWindowHandle {
    pub fn hwnd(&self) -> Option<isize> {
        self.native.with_hwnd(|hwnd| hwnd)
    }

    pub fn overlay(&self) -> &crate::overlay::SharedOverlay {
        &self.overlay
    }

    pub fn is_alive(&self) -> bool {
        self.native.alive.load(Ordering::Acquire)
    }

    pub fn is_responsive(&self) -> bool {
        // SAFETY: the HWND slot stays locked through this synchronous send;
        // WM_NULL does not re-enter the native slot lock.
        self.with_hwnd(|hwnd| unsafe {
            SendMessageTimeoutW(
                HWND(hwnd as *mut _),
                WM_NULL,
                WPARAM(0),
                LPARAM(0),
                SMTO_ABORTIFHUNG,
                250,
                None,
            )
            .0 != 0
        })
        .unwrap_or(false)
    }

    pub fn reveal(&self) {
        if !self.is_alive() {
            return;
        }
        // SAFETY: the HWND slot stays locked until the asynchronous post has
        // copied the handle into the owning thread's queue.
        let _ = self.with_hwnd(|hwnd| unsafe {
            PostMessageW(
                Some(HWND(hwnd as *mut _)),
                REVEAL_MESSAGE,
                WPARAM(0),
                LPARAM(0),
            )
        });
    }

    pub fn set_source_size(&self, width: u32, height: u32) {
        // SAFETY: the HWND slot stays locked until the asynchronous post has
        // copied the handle into the owning thread's queue.
        let _ = self.with_hwnd(|hwnd| unsafe {
            PostMessageW(
                Some(HWND(hwnd as *mut _)),
                ASPECT_MESSAGE,
                WPARAM(width as usize),
                LPARAM(height as isize),
            )
        });
    }

    fn with_hwnd<R>(&self, action: impl FnOnce(isize) -> R) -> Option<R> {
        self.native.with_hwnd(action)
    }
}

fn finish_window_startup(
    ready: mpsc::Receiver<Result<isize>>,
    completed: &mpsc::Receiver<CleanupResult>,
    worker: &mut Option<JoinHandle<()>>,
) -> Result<isize> {
    match ready.recv() {
        Ok(Ok(hwnd)) => Ok(hwnd),
        Ok(Err(error)) => {
            let mut cleanup = None;
            let finish = join_after_worker_completion(
                completed,
                &mut cleanup,
                worker,
                Instant::now() + WORKER_COMPLETION_TIMEOUT,
                "startup failure",
            );
            if shutdown_policy(&finish) == ShutdownPolicy::Abort {
                fail_fast_native_cleanup("startup failure", finish);
            }
            Err(error)
        }
        Err(_) => {
            let mut cleanup = None;
            let finish = join_after_worker_completion(
                completed,
                &mut cleanup,
                worker,
                Instant::now() + WORKER_COMPLETION_TIMEOUT,
                "startup disconnect",
            );
            if shutdown_policy(&finish) == ShutdownPolicy::Abort {
                fail_fast_native_cleanup("startup disconnect", finish);
            }
            bail!("window thread died before it was ready")
        }
    }
}

fn join_after_worker_completion(
    completed: &mpsc::Receiver<CleanupResult>,
    cleanup: &mut Option<CleanupResult>,
    worker: &mut Option<JoinHandle<()>>,
    deadline: Instant,
    phase: &str,
) -> WorkerFinish {
    let Some(thread) = worker.as_ref() else {
        return cleanup
            .map(WorkerFinish::Joined)
            .unwrap_or(WorkerFinish::CompletionDisconnected);
    };
    if thread.thread().id() == std::thread::current().id() {
        return WorkerFinish::SelfJoin;
    }
    if cleanup.is_none() {
        match completed.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
            Ok(result) => *cleanup = Some(result),
            Err(mpsc::RecvTimeoutError::Timeout) => return WorkerFinish::Pending,
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                return WorkerFinish::CompletionDisconnected;
            }
        }
    }

    let wait_ms = duration_to_wait_millis(deadline.saturating_duration_since(Instant::now()));
    // SAFETY: AsRawHandle borrows the kernel thread handle from the JoinHandle,
    // which remains in `worker` for the entire wait.
    let wait = unsafe { WaitForSingleObject(HANDLE(thread.as_raw_handle()), wait_ms) };
    if wait == WAIT_TIMEOUT {
        return WorkerFinish::Pending;
    }
    if wait == WAIT_FAILED {
        return WorkerFinish::ThreadWaitFailed(unsafe { GetLastError().0 });
    }
    if wait != WAIT_OBJECT_0 {
        return WorkerFinish::ThreadWaitFailed(wait.0);
    }

    let thread = worker
        .take()
        .expect("worker remained present after its thread handle was signaled");
    let mut result = cleanup.unwrap_or(Err(CleanupFailure::WorkerPanicked));
    if thread.join().is_err() {
        let _ = writeln!(
            std::io::stderr().lock(),
            "[window] playback worker panicked during {phase}"
        );
        result = Err(CleanupFailure::WorkerPanicked);
    }
    *cleanup = Some(result);
    WorkerFinish::Joined(result)
}

fn duration_to_wait_millis(duration: Duration) -> u32 {
    duration.as_millis().min(u128::from(u32::MAX - 1)) as u32
}

#[cfg(test)]
mod tests {
    use super::{
        finish_window_startup, join_after_worker_completion, shutdown_policy, CleanupFailure,
        CleanupResult, PlaybackProfile, PlaybackWindow, ShutdownPolicy, WorkerFinish,
    };
    use anyhow::anyhow;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{mpsc, Arc};
    use std::time::Duration;
    use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
    use windows::Win32::UI::WindowsAndMessaging::{
        IsWindowVisible, SendMessageTimeoutW, SMTO_ABORTIFHUNG, WM_CLOSE,
    };

    fn hidden_window(title: &str) -> PlaybackWindow {
        PlaybackWindow::spawn(title, PlaybackProfile::FriendViewer { cascade: 0 }).unwrap()
    }

    fn shutdown_hidden(mut owner: PlaybackWindow) {
        assert_eq!(
            owner.try_shutdown_worker(Duration::from_secs(5)),
            WorkerFinish::Joined(Ok(()))
        );
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
}
