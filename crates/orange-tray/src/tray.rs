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
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{channel, Receiver, RecvTimeoutError, Sender, TryRecvError};
use std::sync::OnceLock;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};
use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{
    GetLastError, SetLastError, ERROR_SUCCESS, HINSTANCE, HWND, LPARAM, LRESULT, POINT, RECT,
    WPARAM,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::{GetCurrentProcessId, GetCurrentThreadId};
use windows::Win32::UI::Shell::{
    Shell_NotifyIconW, NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE, NOTIFYICONDATAW,
};
use windows::Win32::UI::WindowsAndMessaging::*;

/// Private message id for icon callbacks.
const WM_TRAY: u32 = WM_APP + 1;
/// Private message requesting cleanup on the native creator thread.
const WM_SHUTDOWN: u32 = WM_APP + 2;
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(2);

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
static NEXT_TRAY_TOKEN: AtomicUsize = AtomicUsize::new(1);

struct TrayContext {
    events: RefCell<Option<Sender<TrayEvent>>>,
    icon_added: Cell<bool>,
    cleaned: Cell<bool>,
    token: usize,
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

struct Ready {
    hwnd: isize,
    thread_id: u32,
    token: usize,
}

type WorkerResult = std::result::Result<(), String>;

/// Owns one tray window, its events, and its native message-loop thread.
pub struct Tray {
    hwnd: isize,
    thread_id: u32,
    token: usize,
    events: Receiver<TrayEvent>,
    completion_rx: Receiver<WorkerResult>,
    completion: Option<WorkerResult>,
    worker: Option<JoinHandle<()>>,
}

impl Tray {
    /// Install the tray icon and start its native message loop.
    pub fn install() -> Result<Self> {
        Self::install_inner(true)
    }

    fn install_inner(add_icon: bool) -> Result<Self> {
        let (event_tx, events) = channel();
        let (ready_tx, ready_rx) = channel::<Result<Ready>>();
        let (completion_tx, completion_rx) = channel();
        let worker = std::thread::spawn(move || {
            let result = unsafe { tray_worker(event_tx, ready_tx, add_icon) };
            let _ = completion_tx.send(result);
        });

        match ready_rx.recv() {
            Ok(Ok(ready)) => Ok(Self {
                hwnd: ready.hwnd,
                thread_id: ready.thread_id,
                token: ready.token,
                events,
                completion_rx,
                completion: None,
                worker: Some(worker),
            }),
            Ok(Err(error)) => {
                join_setup_worker(worker, completion_rx)?;
                Err(error)
            }
            Err(error) => {
                join_setup_worker(worker, completion_rx)?;
                Err(error).context("tray thread died before it was ready")
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
        let Some(worker) = self.worker.as_ref() else {
            return Ok(());
        };

        if worker.thread().id() == std::thread::current().id() {
            anyhow::bail!("cannot join tray worker from itself");
        }

        let deadline = Instant::now() + SHUTDOWN_TIMEOUT;
        let post_error = if worker.is_finished() {
            None
        } else {
            unsafe {
                match PostMessageW(
                    Some(HWND(self.hwnd as *mut _)),
                    WM_SHUTDOWN,
                    WPARAM(self.token),
                    LPARAM(0),
                ) {
                    Ok(()) => None,
                    Err(window_error) => match PostThreadMessageW(
                        self.thread_id,
                        WM_SHUTDOWN,
                        WPARAM(self.token),
                        LPARAM(0),
                    ) {
                        Ok(()) => Some(format!(
                            "window shutdown post failed ({window_error}); thread fallback posted"
                        )),
                        Err(thread_error) => Some(format!(
                            "window shutdown post failed ({window_error}); thread fallback failed ({thread_error})"
                        )),
                    },
                }
            }
        };

        if self.completion.is_none() {
            let remaining = deadline.saturating_duration_since(Instant::now());
            self.completion = match self.completion_rx.recv_timeout(remaining) {
                Ok(result) => Some(result),
                Err(RecvTimeoutError::Disconnected) => {
                    Some(Err("tray worker completion channel disconnected".into()))
                }
                Err(RecvTimeoutError::Timeout) => {
                    anyhow::bail!(
                        "tray worker did not complete within {:?}{}",
                        SHUTDOWN_TIMEOUT,
                        post_error
                            .as_deref()
                            .map(|error| format!("; {error}"))
                            .unwrap_or_default()
                    );
                }
            };
        }

        while !worker.is_finished() {
            if Instant::now() >= deadline {
                anyhow::bail!("tray worker signaled completion but did not finish before deadline");
            }
            std::thread::yield_now();
        }

        let worker = self
            .worker
            .take()
            .expect("worker remains present until bounded completion");
        let join_result = worker.join();
        let completion = self
            .completion
            .take()
            .expect("completion is recorded before joining");
        self.hwnd = 0;
        self.thread_id = 0;
        self.token = 0;

        join_result.map_err(|_| anyhow::anyhow!("tray worker panicked"))?;
        completion.map_err(anyhow::Error::msg)
    }
}

impl Drop for Tray {
    fn drop(&mut self) {
        if let Err(error) = self.shutdown() {
            let _ = writeln!(
                std::io::stderr().lock(),
                "[tray] bounded shutdown failed: {error:#}"
            );
        }
    }
}

fn join_setup_worker(worker: JoinHandle<()>, completion_rx: Receiver<WorkerResult>) -> Result<()> {
    let deadline = Instant::now() + SHUTDOWN_TIMEOUT;
    match completion_rx.recv_timeout(SHUTDOWN_TIMEOUT) {
        Ok(result) => result.map_err(anyhow::Error::msg)?,
        Err(RecvTimeoutError::Disconnected) => {}
        Err(RecvTimeoutError::Timeout) => {
            anyhow::bail!("tray setup worker did not complete within {SHUTDOWN_TIMEOUT:?}")
        }
    }
    while !worker.is_finished() {
        if Instant::now() >= deadline {
            anyhow::bail!(
                "tray setup worker signaled completion but did not finish before deadline"
            );
        }
        std::thread::yield_now();
    }
    worker
        .join()
        .map_err(|_| anyhow::anyhow!("tray thread panicked during setup"))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MessageResult {
    Error,
    Quit,
    Dispatch,
}

fn message_result(result: i32) -> MessageResult {
    if result > 0 {
        MessageResult::Dispatch
    } else if result == 0 {
        MessageResult::Quit
    } else {
        MessageResult::Error
    }
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

unsafe fn tray_worker(
    events: Sender<TrayEvent>,
    ready_tx: Sender<Result<Ready>>,
    add_icon: bool,
) -> WorkerResult {
    let mut context = Box::new(TrayContext {
        events: RefCell::new(Some(events)),
        icon_added: Cell::new(false),
        cleaned: Cell::new(false),
        token: 0,
    });
    context.token = NEXT_TRAY_TOKEN.fetch_add(1, Ordering::Relaxed);
    if context.token == 0 {
        context.token = NEXT_TRAY_TOKEN.fetch_add(1, Ordering::Relaxed);
    }

    let (hwnd, owned_icon) = match create(&context, add_icon) {
        Ok(created) => created,
        Err(error) => {
            let _ = ready_tx.send(Err(error));
            return Ok(());
        }
    };
    let ready = Ready {
        hwnd: hwnd.0 as isize,
        thread_id: GetCurrentThreadId(),
        token: context.token,
    };
    if ready_tx.send(Ok(ready)).is_ok() {
        run_message_loop(hwnd, context.token);
    }

    if IsWindow(Some(hwnd)).as_bool() {
        let _ = DestroyWindow(hwnd);
    }
    if IsWindow(Some(hwnd)).as_bool() {
        context.disconnect_events();
        std::mem::forget(owned_icon);
        Box::leak(context);
        return Err("tray window remained live after creator-thread destruction".into());
    }

    context.disconnect_events();
    drop(owned_icon);
    drop(context);
    Ok(())
}

unsafe fn create(context: &TrayContext, add_icon: bool) -> Result<(HWND, OwnedIcon)> {
    let instance = GetModuleHandleW(None)?;
    let class_name = w!("orange_tray_icon");
    ensure_tray_class(instance.into())?;

    // HWND_MESSAGE creates a message-only window: no pixels, never shown.
    let hwnd = CreateWindowExW(
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

    let (icon, owned_icon) = match LoadImageW(
        Some(instance.into()),
        PCWSTR(std::ptr::with_exposed_provenance(1)),
        IMAGE_ICON,
        GetSystemMetrics(SM_CXSMICON),
        GetSystemMetrics(SM_CYSMICON),
        LR_DEFAULTCOLOR,
    ) {
        Ok(handle) => {
            let icon = HICON(handle.0);
            (icon, OwnedIcon(Some(icon)))
        }
        Err(_) => match LoadIconW(None, IDI_APPLICATION) {
            Ok(icon) => (icon, OwnedIcon(None)),
            Err(error) => {
                let _ = DestroyWindow(hwnd);
                return Err(error.into());
            }
        },
    };

    SetLastError(ERROR_SUCCESS);
    let context_ptr = context as *const TrayContext as isize;
    let previous = SetWindowLongPtrW(hwnd, GWLP_USERDATA, context_ptr);
    if previous == 0 {
        let error = GetLastError();
        if error != ERROR_SUCCESS {
            let _ = DestroyWindow(hwnd);
            anyhow::bail!("failed to install tray context (Win32 error {})", error.0);
        }
    } else {
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
        let _ = DestroyWindow(hwnd);
        let _ = writeln!(
            std::io::stderr().lock(),
            "[tray] replaced unexpected existing window context"
        );
        anyhow::bail!("tray window unexpectedly had an existing context");
    }

    if !add_icon {
        return Ok((hwnd, owned_icon));
    }

    let mut data = NOTIFYICONDATAW {
        cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
        hWnd: hwnd,
        uID: 1,
        uFlags: NIF_MESSAGE | NIF_ICON | NIF_TIP,
        uCallbackMessage: WM_TRAY,
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
        let _ = DestroyWindow(hwnd);
        anyhow::bail!("Shell_NotifyIcon refused to add the tray icon");
    }
    Ok((hwnd, owned_icon))
}

unsafe fn run_message_loop(hwnd: HWND, token: usize) {
    let mut msg = MSG::default();
    loop {
        match message_result(GetMessageW(&mut msg, None, 0, 0).0) {
            MessageResult::Error => {
                let error = GetLastError().0;
                let _ = writeln!(
                    std::io::stderr().lock(),
                    "[tray] GetMessageW failed (Win32 error {error})"
                );
                break;
            }
            MessageResult::Quit => break,
            MessageResult::Dispatch
                if msg.hwnd.0.is_null() && msg.message == WM_SHUTDOWN && msg.wParam.0 == token =>
            {
                if IsWindow(Some(hwnd)).as_bool() {
                    let _ = DestroyWindow(hwnd);
                }
            }
            MessageResult::Dispatch => {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
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
            WM_TRAY => {
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
            WM_SHUTDOWN => {
                let matches =
                    with_context(hwnd, |context| context.token == wparam.0).unwrap_or(false);
                if matches {
                    let _ = DestroyWindow(hwnd);
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
    use super::{message_result, MessageResult, Tray, WM_SHUTDOWN};
    use std::os::windows::io::AsRawHandle;
    use std::process::Command;
    use windows::Win32::Foundation::{HANDLE, LPARAM, WAIT_OBJECT_0, WAIT_TIMEOUT, WPARAM};
    use windows::Win32::System::Threading::WaitForSingleObject;
    use windows::Win32::UI::WindowsAndMessaging::{
        PostThreadMessageW, SendMessageTimeoutW, SMTO_ABORTIFHUNG, WM_QUIT,
    };

    const LIFECYCLE_CHILD: &str = "ORANGE_TRAY_LIFECYCLE_CHILD";

    #[test]
    fn message_result_preserves_get_message_tri_state() {
        assert_eq!(message_result(-42), MessageResult::Error);
        assert_eq!(message_result(-1), MessageResult::Error);
        assert_eq!(message_result(0), MessageResult::Quit);
        assert_eq!(message_result(1), MessageResult::Dispatch);
        assert_eq!(message_result(42), MessageResult::Dispatch);
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
            child.kill().expect("timed-out child should terminate");
            child.wait().expect("timed-out child should be reaped");
            panic!("tray lifecycle child exceeded five seconds");
        }
        if wait != WAIT_OBJECT_0 {
            let _ = child.kill();
            let _ = child.wait();
            panic!("waiting for tray lifecycle child failed: {wait:?}");
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
        let bad_token = first.token.wrapping_add(1);
        let delivered = unsafe {
            SendMessageTimeoutW(
                windows::Win32::Foundation::HWND(first.hwnd as *mut _),
                WM_SHUTDOWN,
                WPARAM(bad_token),
                LPARAM(0),
                SMTO_ABORTIFHUNG,
                1_000,
                None,
            )
        };
        assert_ne!(delivered.0, 0, "wrong-token message should be dispatched");
        assert!(!first.worker.as_ref().unwrap().is_finished());
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
        unsafe {
            PostThreadMessageW(second.thread_id, WM_QUIT, WPARAM(0), LPARAM(0))
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

        let mut timed_out = Tray::install_inner(false).expect("timeout tray should install");
        let hwnd = timed_out.hwnd;
        let thread_id = timed_out.thread_id;
        let token = timed_out.token;
        timed_out.token = token.wrapping_add(1);
        let error = timed_out
            .shutdown()
            .expect_err("accepted wrong-token post must not count as completion");
        assert!(error.to_string().contains("did not complete"));
        assert_eq!(timed_out.hwnd, hwnd);
        assert_eq!(timed_out.thread_id, thread_id);
        assert!(timed_out.worker.is_some());
        assert!(timed_out.completion.is_none());
        timed_out.token = token;
        timed_out
            .shutdown()
            .expect("owner should remain usable after timeout");

        let mut fallback = Tray::install_inner(false).expect("fallback tray should install");
        fallback.hwnd = 1;
        fallback
            .shutdown()
            .expect("thread-message fallback should complete shutdown");

        drop(Tray::install_inner(false).expect("drop-owned tray should install"));
        let mut after_drop = Tray::install_inner(false).expect("install after Drop should succeed");
        after_drop
            .shutdown()
            .expect("post-Drop tray should shut down");
    }
}
