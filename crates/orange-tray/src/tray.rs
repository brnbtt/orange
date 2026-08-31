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
use std::cell::Cell;
use std::io::Write;
use std::sync::mpsc::{channel, Receiver, Sender, TryRecvError};
use std::sync::OnceLock;
use std::thread::JoinHandle;
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

struct TrayContext {
    events: Sender<TrayEvent>,
    icon_added: Cell<bool>,
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
}

/// Owns one tray window, its events, and its native message-loop thread.
pub struct Tray {
    hwnd: isize,
    thread_id: u32,
    events: Receiver<TrayEvent>,
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
        let worker = std::thread::spawn(move || unsafe {
            match create(event_tx, add_icon) {
                Ok((hwnd, owned_icon)) => {
                    let ready = Ready {
                        hwnd: hwnd.0 as isize,
                        thread_id: GetCurrentThreadId(),
                    };
                    if ready_tx.send(Ok(ready)).is_err() {
                        let _ = DestroyWindow(hwnd);
                        return;
                    }
                    run_message_loop(hwnd);
                    drop(owned_icon);
                }
                Err(error) => {
                    let _ = ready_tx.send(Err(error));
                }
            }
        });

        match ready_rx.recv() {
            Ok(Ok(ready)) => Ok(Self {
                hwnd: ready.hwnd,
                thread_id: ready.thread_id,
                events,
                worker: Some(worker),
            }),
            Ok(Err(error)) => {
                join_setup_worker(worker)?;
                Err(error)
            }
            Err(error) => {
                join_setup_worker(worker)?;
                Err(error).context("tray thread died before it was ready")
            }
        }
    }

    /// Receive a pending tray action without blocking.
    pub fn try_recv(&self) -> std::result::Result<TrayEvent, TryRecvError> {
        self.events.try_recv()
    }

    /// Remove the icon and window on their creator thread, then join it.
    pub fn shutdown(&mut self) {
        let Some(worker) = self.worker.take() else {
            return;
        };
        let hwnd = std::mem::take(&mut self.hwnd);

        if worker.thread().id() == std::thread::current().id() {
            // Joining the current thread would deadlock. This is unreachable
            // through the public API, but keep Drop safe if ownership changes.
            unsafe {
                if IsWindow(Some(HWND(hwnd as *mut _))).as_bool() {
                    let _ = DestroyWindow(HWND(hwnd as *mut _));
                }
            }
            return;
        }

        let posted = worker.is_finished()
            || unsafe {
                PostThreadMessageW(self.thread_id, WM_SHUTDOWN, WPARAM(0), LPARAM(0)).is_ok()
            };
        if !posted && !worker.is_finished() {
            let _ = writeln!(
                std::io::stderr().lock(),
                "[tray] failed to wake tray thread for shutdown (HWND {hwnd:#x})"
            );
            // Do not risk an unbounded join when Windows refused the wakeup.
            return;
        }
        if worker.join().is_err() {
            let _ = writeln!(std::io::stderr().lock(), "[tray] tray thread panicked");
        }
    }
}

impl Drop for Tray {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn join_setup_worker(worker: JoinHandle<()>) -> Result<()> {
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

unsafe fn create(events: Sender<TrayEvent>, add_icon: bool) -> Result<(HWND, OwnedIcon)> {
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

    let context = Box::into_raw(Box::new(TrayContext {
        events,
        icon_added: Cell::new(false),
    }));
    SetLastError(ERROR_SUCCESS);
    let previous = SetWindowLongPtrW(hwnd, GWLP_USERDATA, context as isize);
    if previous == 0 {
        let error = GetLastError();
        if error != ERROR_SUCCESS {
            // SAFETY: Installation failed, so ownership never transferred to
            // the HWND and the allocation remains exclusively owned here.
            drop(Box::from_raw(context));
            let _ = DestroyWindow(hwnd);
            anyhow::bail!("failed to install tray context (Win32 error {})", error.0);
        }
    } else {
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
        // SAFETY: The fresh allocation was removed again and is exclusively
        // owned here. The unexpected previous value remains opaque.
        drop(Box::from_raw(context));
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
    let _ = with_context(hwnd, |context| context.icon_added.set(true));
    if !Shell_NotifyIconW(NIM_ADD, &data).as_bool() {
        let _ = DestroyWindow(hwnd);
        anyhow::bail!("Shell_NotifyIcon refused to add the tray icon");
    }
    Ok((hwnd, owned_icon))
}

unsafe fn run_message_loop(hwnd: HWND) {
    let mut msg = MSG::default();
    loop {
        match message_result(GetMessageW(&mut msg, None, 0, 0).0) {
            MessageResult::Error => {
                let error = GetLastError().0;
                if IsWindow(Some(hwnd)).as_bool() {
                    let _ = DestroyWindow(hwnd);
                }
                let _ = writeln!(
                    std::io::stderr().lock(),
                    "[tray] GetMessageW failed (Win32 error {error})"
                );
                break;
            }
            MessageResult::Quit => break,
            MessageResult::Dispatch if msg.hwnd.0.is_null() && msg.message == WM_SHUTDOWN => {
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
    let _ = with_context(hwnd, |context| context.events.send(event));
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
            WM_DESTROY => {
                SetLastError(ERROR_SUCCESS);
                let context = SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0) as *mut TrayContext;
                if context.is_null() {
                    let error = GetLastError();
                    if error != ERROR_SUCCESS {
                        let _ = writeln!(
                            std::io::stderr().lock(),
                            "[tray] failed to clear tray context (Win32 error {})",
                            error.0
                        );
                    }
                } else {
                    // SAFETY: Clearing GWLP_USERDATA transferred the Box back
                    // to this creator-thread callback exactly once.
                    let context = Box::from_raw(context);
                    if context.icon_added.get() {
                        let data = NOTIFYICONDATAW {
                            cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
                            hWnd: hwnd,
                            uID: 1,
                            ..Default::default()
                        };
                        let _ = Shell_NotifyIconW(NIM_DELETE, &data);
                    }
                    drop(context);
                }
                PostQuitMessage(0);
                LRESULT(0)
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
    use super::{message_result, MessageResult, Tray};
    use std::sync::mpsc;
    use std::time::Duration;

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
        let (done_tx, done_rx) = mpsc::sync_channel(0);
        std::thread::spawn(move || {
            let mut first = Tray::install_inner(false).expect("first tray should install");
            assert!(!first.worker.as_ref().unwrap().is_finished());
            first.shutdown();
            assert!(first.worker.is_none());
            assert_eq!(first.try_recv(), Err(mpsc::TryRecvError::Disconnected));

            let mut second = Tray::install_inner(false).expect("second tray should install");
            assert!(!second.worker.as_ref().unwrap().is_finished());
            second.shutdown();
            assert!(second.worker.is_none());
            assert_eq!(second.try_recv(), Err(mpsc::TryRecvError::Disconnected));

            done_tx.send(()).unwrap();
        });

        done_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("tray shutdown or reinstall did not complete promptly");
    }
}
