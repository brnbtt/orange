//! A real system tray icon.
//!
//! GPUI has no tray support, so this is raw Win32: a hidden message-only
//! window owns the notification icon and receives its callbacks. It runs on
//! its own thread because a window's message loop must live on the thread
//! that created it.
//!
//! Clicks are reported back through a channel rather than touching the UI
//! directly, so the GPUI side stays in charge of its own state.

use anyhow::{Context, Result};
use std::cell::{Cell, RefCell};
use std::io::Write;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::sync::mpsc::{channel, Receiver, RecvTimeoutError, Sender, TryRecvError};
use std::sync::OnceLock;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};
use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{
    GetLastError, SetLastError, ERROR_SUCCESS, HANDLE, HINSTANCE, HWND, LPARAM, LRESULT, POINT,
    RECT, WAIT_FAILED, WAIT_OBJECT_0, WAIT_TIMEOUT, WPARAM,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::{
    CreateEventW, GetCurrentProcessId, SetEvent, WaitForSingleObject,
};
use windows::Win32::UI::Shell::{
    Shell_NotifyIconW, NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE, NOTIFYICONDATAW,
};
use windows::Win32::UI::WindowsAndMessaging::*;

const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(2);
const FINAL_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);
const DESTROY_ATTEMPTS: u32 = 20;
const MESSAGE_WAIT_MS: u32 = 1_000;

const ID_SHOW: usize = 1;
const ID_QUIT: usize = 2;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrayEvent {
    /// Left click, or "Open" from the menu.
    Show,
    Quit,
}

#[derive(Debug, Clone, Copy)]
enum ClassError {
    Windows(u32),
}

type ClassResult = std::result::Result<(), ClassError>;
static TRAY_CLASS_RESULT: OnceLock<ClassResult> = OnceLock::new();
static TRAY_MESSAGE_RESULT: OnceLock<std::result::Result<u32, u32>> = OnceLock::new();

struct TrayContext {
    events: RefCell<Option<Sender<TrayEvent>>>,
    icon_added: Cell<bool>,
    cleaned: Cell<bool>,
    shutdown_event: isize,
}

impl TrayContext {
    fn disconnect_events(&self) {
        self.events.borrow_mut().take();
    }

    unsafe fn cleanup(&self, hwnd: HWND) {
        if self.cleaned.replace(true) {
            return;
        }
        self.disconnect_events();
        if self.icon_added.replace(false) {
            let data = NOTIFYICONDATAW {
                cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
                hWnd: hwnd,
                uID: 1,
                ..Default::default()
            };
            let _ = Shell_NotifyIconW(NIM_DELETE, &data);
        }
    }
}

struct OwnedIcon(Option<HICON>);

impl Drop for OwnedIcon {
    fn drop(&mut self) {
        if let Some(icon) = self.0.take() {
            unsafe {
                let _ = DestroyIcon(icon);
            }
        }
    }
}

/// Owns one tray window, its events, and its native message-loop thread.
pub struct Tray {
    hwnd: isize,
    events: Receiver<TrayEvent>,
    worker: Option<JoinHandle<()>>,
    shutdown_event: Option<OwnedHandle>,
}

impl Tray {
    /// Install the tray icon and start its native message loop.
    pub fn install() -> Result<Self> {
        Self::install_inner(true)
    }

    fn install_inner(add_icon: bool) -> Result<Self> {
        let (event_tx, events) = channel();
        let (ready_tx, ready_rx) = channel::<Result<isize>>();
        // SAFETY: CreateEventW returns a new owning handle. from_raw_handle
        // transfers that sole ownership into OwnedHandle exactly once.
        let shutdown_event = unsafe {
            let handle = CreateEventW(None, true, false, PCWSTR::null())?;
            OwnedHandle::from_raw_handle(handle.0)
        };
        let shutdown_handle = shutdown_event.as_raw_handle() as isize;
        let worker = std::thread::spawn(move || {
            // SAFETY: The owner keeps shutdown_handle open until this worker
            // terminates, and tray_worker owns all native window operations.
            unsafe { tray_worker(event_tx, ready_tx, add_icon, shutdown_handle) };
        });

        match ready_rx.recv_timeout(SHUTDOWN_TIMEOUT) {
            Ok(Ok(hwnd)) => Ok(Self {
                hwnd,
                events,
                worker: Some(worker),
                shutdown_event: Some(shutdown_event),
            }),
            Ok(Err(error)) => {
                finish_setup_worker(worker, &shutdown_event, FINAL_SHUTDOWN_TIMEOUT);
                Err(error)
            }
            Err(error) => {
                finish_setup_worker(worker, &shutdown_event, FINAL_SHUTDOWN_TIMEOUT);
                match error {
                    RecvTimeoutError::Timeout => {
                        anyhow::bail!("tray thread was not ready within {SHUTDOWN_TIMEOUT:?}")
                    }
                    RecvTimeoutError::Disconnected => {
                        Err(error).context("tray thread died before it was ready")
                    }
                }
            }
        }
    }

    /// Receive a pending tray action without blocking.
    pub fn try_recv(&self) -> std::result::Result<TrayEvent, TryRecvError> {
        self.events.try_recv()
    }

    /// Remove the icon and window on their creator thread, then join it.
    ///
    /// # Errors
    ///
    /// Returns an error without consuming the worker handle if shutdown does
    /// not complete before the deadline. A later call may retry.
    pub fn shutdown(&mut self) -> Result<()> {
        self.shutdown_with_timeout(SHUTDOWN_TIMEOUT)
    }

    pub(crate) fn shutdown_final(&mut self) -> Result<()> {
        self.shutdown_with_timeout(FINAL_SHUTDOWN_TIMEOUT)
    }

    fn shutdown_with_timeout(&mut self, timeout: Duration) -> Result<()> {
        let Some(worker) = self.worker.as_ref() else {
            return Ok(());
        };
        if worker.thread().id() == std::thread::current().id() {
            anyhow::bail!("cannot join tray worker from itself");
        }

        let deadline = Instant::now() + timeout;
        if !wait_for_thread(worker, Duration::ZERO)? {
            let shutdown_event = self
                .shutdown_event
                .as_ref()
                .expect("live worker retains its shutdown event");
            signal_event(shutdown_event)?;
            if let Ok(message) = unsafe { tray_message() } {
                if let Err(error) = unsafe {
                    PostMessageW(
                        Some(HWND(self.hwnd as *mut _)),
                        message,
                        WPARAM(0),
                        LPARAM(0),
                    )
                } {
                    let _ = writeln!(
                        std::io::stderr().lock(),
                        "[tray] modal wake post failed; event wait remains active: {error}"
                    );
                }
            }
        }
        match wait_for_thread(worker, deadline.saturating_duration_since(Instant::now()))? {
            true => self.join_finished_worker(),
            false => anyhow::bail!("tray worker did not stop within {timeout:?}"),
        }
    }

    fn join_finished_worker(&mut self) -> Result<()> {
        let worker = self
            .worker
            .take()
            .expect("worker remains owned until its native handle is signaled");
        debug_assert!(worker.is_finished());
        let result = worker.join();
        self.hwnd = 0;
        self.shutdown_event.take();
        result.map_err(|_| anyhow::anyhow!("tray worker panicked"))
    }
}

impl Drop for Tray {
    fn drop(&mut self) {
        if let Err(error) = self.shutdown_final() {
            fail_fast("tray Drop could not complete native cleanup", &error);
        }
    }
}

fn raw_handle(handle: &OwnedHandle) -> HANDLE {
    HANDLE(handle.as_raw_handle())
}

fn signal_event(event: &OwnedHandle) -> Result<()> {
    // SAFETY: OwnedHandle keeps this valid event handle open for the call.
    unsafe { SetEvent(raw_handle(event))? };
    Ok(())
}

fn wait_for_thread(worker: &JoinHandle<()>, timeout: Duration) -> Result<bool> {
    let timeout_ms = timeout.as_millis().min(u128::from(u32::MAX - 1)) as u32;
    // SAFETY: AsRawHandle borrows the native thread handle. `worker` remains
    // alive and unmoved for the complete wait, so Windows cannot observe a
    // closed handle. Thread handles are valid waitable synchronization objects.
    let result = unsafe { WaitForSingleObject(HANDLE(worker.as_raw_handle()), timeout_ms) };
    if result == WAIT_OBJECT_0 {
        Ok(true)
    } else if result == WAIT_TIMEOUT {
        Ok(false)
    } else if result == WAIT_FAILED {
        Err(anyhow::anyhow!(
            "waiting for tray thread failed (Win32 error {})",
            unsafe { GetLastError().0 }
        ))
    } else {
        Err(anyhow::anyhow!(
            "waiting for tray thread returned unexpected status {result:?}"
        ))
    }
}

fn finish_setup_worker(worker: JoinHandle<()>, event: &OwnedHandle, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    signal_event(event).unwrap_or_else(|error| fail_fast("could not stop tray setup", &error));
    match wait_for_thread(&worker, deadline.saturating_duration_since(Instant::now())) {
        Ok(signaled) if setup_cleanup_decision(signaled) == CleanupDecision::Complete => {}
        Ok(_) => fail_fast(
            "tray setup did not terminate before the final deadline",
            &anyhow::anyhow!("timeout after {timeout:?}"),
        ),
        Err(error) => fail_fast("could not wait for tray setup", &error),
    }
    if worker.join().is_err() {
        fail_fast(
            "tray setup worker panicked",
            &anyhow::anyhow!("native tray ownership may be incomplete"),
        );
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CleanupDecision {
    Complete,
    Retry,
    Abort,
}

fn cleanup_decision(window_alive: bool, attempts: u32, before_deadline: bool) -> CleanupDecision {
    if !window_alive {
        CleanupDecision::Complete
    } else if attempts < DESTROY_ATTEMPTS && before_deadline {
        CleanupDecision::Retry
    } else {
        CleanupDecision::Abort
    }
}

fn setup_cleanup_decision(worker_signaled: bool) -> CleanupDecision {
    if worker_signaled {
        CleanupDecision::Complete
    } else {
        CleanupDecision::Abort
    }
}

pub(crate) fn fail_fast(context: &str, error: &anyhow::Error) -> ! {
    let _ = writeln!(std::io::stderr().lock(), "[tray] {context}: {error:#}");
    std::process::abort();
}

unsafe fn ensure_tray_class(instance: HINSTANCE) -> Result<()> {
    let result = TRAY_CLASS_RESULT.get_or_init(|| {
        let class = WNDCLASSW {
            lpfnWndProc: Some(tray_proc),
            hInstance: instance,
            lpszClassName: w!("orange_tray_icon"),
            ..Default::default()
        };
        if RegisterClassW(&class) != 0 {
            Ok(())
        } else {
            Err(ClassError::Windows(GetLastError().0))
        }
    });
    match result {
        Ok(()) => Ok(()),
        Err(ClassError::Windows(error)) => {
            anyhow::bail!("failed to register tray window class (Win32 error {error})")
        }
    }
}

unsafe fn tray_message() -> Result<u32> {
    let result = TRAY_MESSAGE_RESULT.get_or_init(|| {
        let message = RegisterWindowMessageW(w!(
            "orange_tray_callback_88dd87b7-0a96-42aa-babd-fc9841a93f71"
        ));
        if message == 0 {
            Err(GetLastError().0)
        } else {
            Ok(message)
        }
    });
    result
        .as_ref()
        .copied()
        .map_err(|error| anyhow::anyhow!("failed to register tray callback (Win32 error {error})"))
}

fn is_tray_message(message: u32) -> bool {
    TRAY_MESSAGE_RESULT
        .get()
        .and_then(|result| result.as_ref().ok())
        .is_some_and(|registered| *registered == message)
}

unsafe fn tray_worker(
    events: Sender<TrayEvent>,
    ready_tx: Sender<Result<isize>>,
    add_icon: bool,
    shutdown_handle: isize,
) {
    let context = Box::new(TrayContext {
        events: RefCell::new(Some(events)),
        icon_added: Cell::new(false),
        cleaned: Cell::new(false),
        shutdown_event: shutdown_handle,
    });
    let mut hwnd = None;
    let mut owned_icon = None;
    match create(&context, add_icon, &mut hwnd, &mut owned_icon) {
        Ok(()) => {
            let window = hwnd.expect("successful tray setup has an HWND");
            if ready_tx.send(Ok(window.0 as isize)).is_ok() {
                if let Err(error) = run_message_loop(HANDLE(shutdown_handle as *mut _)) {
                    let _ = writeln!(std::io::stderr().lock(), "[tray] {error}");
                }
            }
        }
        Err(error) => {
            let _ = ready_tx.send(Err(error));
        }
    }

    if let Some(hwnd) = hwnd {
        destroy_window(hwnd);
    }
    context.disconnect_events();
    drop(owned_icon);
    drop(context);
}

unsafe fn create(
    context: &TrayContext,
    add_icon: bool,
    hwnd: &mut Option<HWND>,
    owned_icon: &mut Option<OwnedIcon>,
) -> Result<()> {
    let instance = GetModuleHandleW(None)?;
    let class_name = w!("orange_tray_icon");
    ensure_tray_class(instance.into())?;

    // HWND_MESSAGE creates a message-only window: no pixels, never shown.
    let window = CreateWindowExW(
        WINDOW_EX_STYLE::default(),
        class_name,
        w!("orange"),
        WINDOW_STYLE::default(),
        0,
        0,
        0,
        0,
        Some(HWND_MESSAGE),
        None,
        Some(instance.into()),
        None,
    )?;
    *hwnd = Some(window);

    let icon = match LoadImageW(
        Some(instance.into()),
        PCWSTR(std::ptr::with_exposed_provenance(1)),
        IMAGE_ICON,
        GetSystemMetrics(SM_CXSMICON),
        GetSystemMetrics(SM_CYSMICON),
        LR_DEFAULTCOLOR,
    ) {
        Ok(handle) => {
            let icon = HICON(handle.0);
            *owned_icon = Some(OwnedIcon(Some(icon)));
            icon
        }
        Err(_) => {
            let icon = LoadIconW(None, IDI_APPLICATION)?;
            *owned_icon = Some(OwnedIcon(None));
            icon
        }
    };

    SetLastError(ERROR_SUCCESS);
    let context_ptr = context as *const TrayContext as isize;
    let previous = SetWindowLongPtrW(window, GWLP_USERDATA, context_ptr);
    if previous == 0 {
        let error = GetLastError();
        if error != ERROR_SUCCESS {
            anyhow::bail!("failed to install tray context (Win32 error {})", error.0);
        }
    } else {
        SetWindowLongPtrW(window, GWLP_USERDATA, 0);
        let _ = writeln!(
            std::io::stderr().lock(),
            "[tray] replaced unexpected existing window context"
        );
        anyhow::bail!("tray window unexpectedly had an existing context");
    }

    let callback_message = tray_message()?;
    if !add_icon {
        return Ok(());
    }

    let mut data = NOTIFYICONDATAW {
        cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
        hWnd: window,
        uID: 1,
        uFlags: NIF_MESSAGE | NIF_ICON | NIF_TIP,
        uCallbackMessage: callback_message,
        // Load the optical small-size entry from the multi-resolution icon;
        // asking for the system tray metric avoids a blurry 32px downscale.
        hIcon: icon,
        ..Default::default()
    };

    let tip: Vec<u16> = "orange".encode_utf16().chain(std::iter::once(0)).collect();
    data.szTip[..tip.len()].copy_from_slice(&tip);

    // Mark first so even synchronous destruction during the shell call takes
    // the matching delete path. Deleting an icon that was not added is safe.
    context.icon_added.set(true);
    if !Shell_NotifyIconW(NIM_ADD, &data).as_bool() {
        anyhow::bail!("Shell_NotifyIcon refused to add the tray icon");
    }
    Ok(())
}

unsafe fn run_message_loop(shutdown_event: HANDLE) -> std::result::Result<(), String> {
    let handles = [shutdown_event];
    loop {
        let wait = MsgWaitForMultipleObjectsEx(
            Some(&handles),
            MESSAGE_WAIT_MS,
            QS_ALLINPUT,
            MWMO_INPUTAVAILABLE,
        );
        if wait == WAIT_OBJECT_0 {
            return Ok(());
        }
        if wait.0 == WAIT_OBJECT_0.0 + handles.len() as u32 {
            let mut msg = MSG::default();
            while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                if msg.message == WM_QUIT {
                    return Ok(());
                }
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);

                match WaitForSingleObject(shutdown_event, 0) {
                    status if status == WAIT_OBJECT_0 => return Ok(()),
                    status if status == WAIT_TIMEOUT => {}
                    status if status == WAIT_FAILED => {
                        return Err(format!(
                            "checking shutdown event failed (Win32 error {})",
                            GetLastError().0
                        ));
                    }
                    status => {
                        return Err(format!(
                            "checking shutdown event returned unexpected status {status:?}"
                        ));
                    }
                }
            }
            continue;
        }
        if wait == WAIT_FAILED {
            return Err(format!(
                "message wait failed (Win32 error {})",
                GetLastError().0
            ));
        }
        if wait == WAIT_TIMEOUT {
            continue;
        }
        return Err(format!("message wait returned unexpected status {wait:?}"));
    }
}

unsafe fn destroy_window(hwnd: HWND) {
    let deadline = Instant::now() + SHUTDOWN_TIMEOUT;
    let mut attempts = 0u32;
    let mut destroy_error = None;
    loop {
        match cleanup_decision(
            IsWindow(Some(hwnd)).as_bool(),
            attempts,
            Instant::now() < deadline,
        ) {
            CleanupDecision::Complete => return,
            CleanupDecision::Abort => fail_fast(
                "creator thread could not destroy tray HWND",
                &destroy_error.unwrap_or_else(|| {
                    anyhow::anyhow!(
                        "HWND remained live after {attempts} attempts within {SHUTDOWN_TIMEOUT:?}"
                    )
                }),
            ),
            CleanupDecision::Retry => {}
        }

        attempts += 1;
        destroy_error = DestroyWindow(hwnd).err().map(anyhow::Error::from);
        if IsWindow(Some(hwnd)).as_bool() {
            if attempts == 1 || attempts.is_multiple_of(20) {
                let _ = writeln!(
                    std::io::stderr().lock(),
                    "[tray] HWND remained live after DestroyWindow attempt {attempts}: {}",
                    destroy_error
                        .as_ref()
                        .map(ToString::to_string)
                        .as_deref()
                        .unwrap_or("no Win32 error reported")
                );
            }
            let wait_ms = deadline
                .saturating_duration_since(Instant::now())
                .as_millis()
                .min(100) as u32;
            let wait = MsgWaitForMultipleObjectsEx(None, wait_ms, QS_ALLINPUT, MWMO_INPUTAVAILABLE);
            if wait == WAIT_FAILED {
                let _ = writeln!(
                    std::io::stderr().lock(),
                    "[tray] cleanup message wait failed (Win32 error {})",
                    GetLastError().0
                );
            }
            let mut msg = MSG::default();
            while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                if msg.message != WM_QUIT {
                    let _ = TranslateMessage(&msg);
                    DispatchMessageW(&msg);
                }
            }
        }
    }
}

/// Runs `action` synchronously with the context installed for this HWND.
///
/// # Safety
///
/// The HWND must be accessed on its creator thread. The action must not destroy
/// the window or dispatch reentrant messages. A non-null `GWLP_USERDATA` must
/// be the live `TrayContext` allocation installed by `create`.
unsafe fn with_context<R>(
    hwnd: HWND,
    action: impl for<'a> FnOnce(&'a TrayContext) -> R,
) -> Option<R> {
    let context = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *const TrayContext;
    // SAFETY: The caller upholds the pointer validity and reentrancy contract.
    context.as_ref().map(action)
}

unsafe fn emit(hwnd: HWND, event: TrayEvent) {
    let _ = with_context(hwnd, |context| {
        if let Some(events) = context.events.borrow().as_ref() {
            let _ = events.send(event);
        }
    });
}

extern "system" fn tray_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    unsafe {
        match msg {
            message if is_tray_message(message) => {
                let shutdown = with_context(hwnd, |context| {
                    WaitForSingleObject(HANDLE(context.shutdown_event as *mut _), 0)
                });
                match shutdown {
                    Some(status) if status == WAIT_OBJECT_0 => {
                        let _ = DestroyWindow(hwnd);
                        return LRESULT(0);
                    }
                    Some(status) if status == WAIT_TIMEOUT => {}
                    Some(status) if status == WAIT_FAILED => fail_fast(
                        "tray callback could not inspect shutdown event",
                        &anyhow::anyhow!("Win32 error {}", GetLastError().0),
                    ),
                    Some(status) => fail_fast(
                        "tray callback received unexpected event wait status",
                        &anyhow::anyhow!("status {status:?}"),
                    ),
                    None => return DefWindowProcW(hwnd, msg, wparam, lparam),
                }
                // The mouse message arrives in the low word of lparam.
                match (lparam.0 as u32) & 0xFFFF {
                    x if x == WM_LBUTTONUP => emit(hwnd, TrayEvent::Show),
                    x if x == WM_RBUTTONUP => show_menu(hwnd),
                    _ => {}
                }
                LRESULT(0)
            }
            WM_COMMAND => {
                match wparam.0 & 0xFFFF {
                    ID_SHOW => emit(hwnd, TrayEvent::Show),
                    ID_QUIT => emit(hwnd, TrayEvent::Quit),
                    _ => {}
                }
                LRESULT(0)
            }
            WM_DESTROY => {
                let context = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *const TrayContext;
                SetLastError(ERROR_SUCCESS);
                let previous = SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
                if previous == 0 {
                    let error = GetLastError();
                    if error != ERROR_SUCCESS {
                        let _ = writeln!(
                            std::io::stderr().lock(),
                            "[tray] failed to clear tray context (Win32 error {})",
                            error.0
                        );
                    }
                }
                // SAFETY: The worker owns this allocation until after
                // DestroyWindow returns and the HWND is confirmed dead.
                if let Some(context) = context.as_ref() {
                    context.cleanup(hwnd);
                }
                PostQuitMessage(0);
                LRESULT(0)
            }
            WM_NCDESTROY => {
                // Retry a failed WM_DESTROY clear while the worker-owned
                // context is still live. Cleanup itself is idempotent.
                let context = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *const TrayContext;
                if !context.is_null() {
                    SetLastError(ERROR_SUCCESS);
                    let _ = SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
                    if let Some(context) = context.as_ref() {
                        context.cleanup(hwnd);
                    }
                }
                DefWindowProcW(hwnd, msg, wparam, lparam)
            }
            _ => DefWindowProcW(hwnd, msg, wparam, lparam),
        }
    }
}

/// Hide the main window entirely, leaving the app alive in the tray.
///
/// GPUI exposes `minimize` but no per-window hide, so this goes through Win32.
/// The handle is found by enumerating our own top-level windows and cached,
/// because once hidden the window is no longer discoverable by visibility.
pub fn hide_main_window() {
    if let Some(hwnd) = main_window() {
        unsafe {
            let _ = ShowWindow(hwnd, SW_HIDE);
        }
    }
}

/// Bring the main window back and focus it.
pub fn show_main_window() {
    if let Some(hwnd) = main_window() {
        unsafe {
            let _ = ShowWindow(hwnd, SW_SHOW);
            let _ = ShowWindow(hwnd, SW_RESTORE);
            let _ = SetForegroundWindow(hwnd);
        }
    }
}

static MAIN_WINDOW: std::sync::OnceLock<isize> = std::sync::OnceLock::new();

fn main_window() -> Option<HWND> {
    if let Some(handle) = MAIN_WINDOW.get() {
        return Some(HWND(*handle as *mut _));
    }
    let found = unsafe { find_main_window() }?;
    let _ = MAIN_WINDOW.set(found.0 as isize);
    Some(found)
}

unsafe extern "system" fn find_proc(hwnd: HWND, lparam: LPARAM) -> windows::core::BOOL {
    let out = &mut *(lparam.0 as *mut Option<HWND>);
    let mut pid = 0u32;
    GetWindowThreadProcessId(hwnd, Some(&mut pid));
    if pid != GetCurrentProcessId() || !IsWindowVisible(hwnd).as_bool() {
        return windows::core::BOOL(1);
    }
    let mut rect = RECT::default();
    if GetWindowRect(hwnd, &mut rect).is_err() {
        return windows::core::BOOL(1);
    }
    // The message-only tray window has no size; the UI window does.
    if rect.right - rect.left > 100 && rect.bottom - rect.top > 100 {
        *out = Some(hwnd);
        return windows::core::BOOL(0);
    }
    windows::core::BOOL(1)
}

unsafe fn find_main_window() -> Option<HWND> {
    let mut found: Option<HWND> = None;
    let _ = EnumWindows(
        Some(find_proc),
        LPARAM(&mut found as *mut Option<HWND> as isize),
    );
    found
}

unsafe fn show_menu(hwnd: HWND) {
    let Ok(menu) = CreatePopupMenu() else { return };
    let _ = AppendMenuW(menu, MF_STRING, ID_SHOW, w!("Open orange"));
    let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
    let _ = AppendMenuW(menu, MF_STRING, ID_QUIT, w!("Quit"));

    let mut point = POINT::default();
    let _ = GetCursorPos(&mut point);
    // Required, or the menu refuses to close when clicking elsewhere.
    let _ = SetForegroundWindow(hwnd);
    let _ = TrackPopupMenu(
        menu,
        TPM_RIGHTALIGN | TPM_BOTTOMALIGN,
        point.x,
        point.y,
        Some(0),
        hwnd,
        None,
    );
    let _ = DestroyMenu(menu);
}

#[cfg(test)]
mod tests {
    use super::{cleanup_decision, setup_cleanup_decision, tray_message, CleanupDecision, Tray};
    use std::os::windows::io::AsRawHandle;
    use std::process::Command;
    use windows::Win32::Foundation::{HANDLE, LPARAM, WAIT_OBJECT_0, WAIT_TIMEOUT, WPARAM};
    use windows::Win32::System::Threading::WaitForSingleObject;
    use windows::Win32::UI::WindowsAndMessaging::{
        GetWindowThreadProcessId, PostThreadMessageW, SendMessageTimeoutW, SMTO_ABORTIFHUNG,
        WM_QUIT,
    };

    const LIFECYCLE_CHILD: &str = "ORANGE_TRAY_LIFECYCLE_CHILD";

    #[test]
    fn cleanup_deadlines_choose_retry_or_fail_fast() {
        assert_eq!(cleanup_decision(false, 0, true), CleanupDecision::Complete);
        assert_eq!(cleanup_decision(true, 0, true), CleanupDecision::Retry);
        assert_eq!(
            cleanup_decision(true, super::DESTROY_ATTEMPTS, true),
            CleanupDecision::Abort
        );
        assert_eq!(cleanup_decision(true, 0, false), CleanupDecision::Abort);
        assert_eq!(setup_cleanup_decision(true), CleanupDecision::Complete);
        assert_eq!(setup_cleanup_decision(false), CleanupDecision::Abort);
    }

    #[test]
    fn owner_shutdown_joins_promptly_and_allows_reinstall() {
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args([
                "--ignored",
                "--exact",
                "tray::tests::native_lifecycle_child",
                "--test-threads=1",
            ])
            .env(LIFECYCLE_CHILD, "1")
            .spawn()
            .expect("lifecycle child should start");

        let wait = unsafe { WaitForSingleObject(HANDLE(child.as_raw_handle()), 5_000) };
        if wait == WAIT_TIMEOUT {
            let kill = child.kill();
            let reap = child.wait();
            panic!("tray lifecycle child exceeded five seconds; kill={kill:?}; wait={reap:?}");
        }
        if wait != WAIT_OBJECT_0 {
            let kill = child.kill();
            let reap = child.wait();
            panic!(
                "waiting for tray lifecycle child failed: {wait:?}; kill={kill:?}; wait={reap:?}"
            );
        }
        assert!(child.wait().unwrap().success());
    }

    #[test]
    #[ignore = "run in a bounded subprocess by owner_shutdown_joins_promptly_and_allows_reinstall"]
    fn native_lifecycle_child() {
        if std::env::var_os(LIFECYCLE_CHILD).is_none() {
            return;
        }

        let mut first = Tray::install_inner(false).expect("first tray should install");
        assert!(!first.worker.as_ref().unwrap().is_finished());
        let callback = unsafe { tray_message().expect("wake message should be registered") };
        let delivered = unsafe {
            SendMessageTimeoutW(
                windows::Win32::Foundation::HWND(first.hwnd as *mut _),
                callback,
                WPARAM(0),
                LPARAM(0),
                SMTO_ABORTIFHUNG,
                1_000,
                None,
            )
        };
        assert_ne!(delivered.0, 0, "wake message should dispatch");
        assert!(
            !first.worker.as_ref().unwrap().is_finished(),
            "an unsignaled instance event must reject the wake"
        );
        first.shutdown().expect("first tray should shut down");
        assert!(first.worker.is_none());
        assert_eq!(
            first.try_recv(),
            Err(std::sync::mpsc::TryRecvError::Disconnected)
        );
        first
            .shutdown()
            .expect("repeated shutdown should be harmless");

        let mut second = Tray::install_inner(false).expect("second tray should install");
        let thread_id = unsafe {
            GetWindowThreadProcessId(
                windows::Win32::Foundation::HWND(second.hwnd as *mut _),
                None,
            )
        };
        unsafe {
            PostThreadMessageW(thread_id, WM_QUIT, WPARAM(0), LPARAM(0))
                .expect("WM_QUIT should post");
        }
        second
            .shutdown()
            .expect("WM_QUIT cleanup should complete and join");
        assert!(second.worker.is_none());
        assert_eq!(
            second.try_recv(),
            Err(std::sync::mpsc::TryRecvError::Disconnected)
        );

        let owner = std::rc::Rc::new(std::cell::RefCell::new(Some(
            Tray::install_inner(false).expect("app-owned tray should install"),
        )));
        crate::shutdown_owned_tray(&owner).expect("app-owned tray should shut down");
        assert!(owner.borrow().is_none());

        let final_owner = std::rc::Rc::new(std::cell::RefCell::new(Some(
            Tray::install_inner(false).expect("final app tray should install"),
        )));
        crate::finish_owned_tray(&final_owner).expect("outer app owner should finish shutdown");
        assert!(final_owner.borrow().is_none());

        drop(Tray::install_inner(false).expect("drop-owned tray should install"));
        let mut after_drop = Tray::install_inner(false).expect("install after Drop should succeed");
        after_drop
            .shutdown()
            .expect("post-Drop tray should shut down");
    }
}
