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
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::OnceLock;
use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::GetCurrentProcessId;
use windows::Win32::UI::Shell::{
    Shell_NotifyIconW, NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE, NOTIFYICONDATAW,
};
use windows::Win32::UI::WindowsAndMessaging::*;

/// Private message id for icon callbacks.
const WM_TRAY: u32 = WM_APP + 1;

const ID_SHOW: usize = 1;
const ID_QUIT: usize = 2;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrayEvent {
    /// Left click, or "Open" from the menu.
    Show,
    Quit,
}

static EVENTS: OnceLock<Sender<TrayEvent>> = OnceLock::new();

/// Install the tray icon. Returns a receiver of user actions.
pub fn install() -> Result<Receiver<TrayEvent>> {
    let (tx, rx) = channel();
    EVENTS
        .set(tx)
        .map_err(|_| anyhow::anyhow!("tray already installed"))?;

    let (ready_tx, ready_rx) = channel::<Result<()>>();
    std::thread::spawn(move || unsafe {
        match create() {
            Ok(_) => {
                let _ = ready_tx.send(Ok(()));
                let mut msg = MSG::default();
                while GetMessageW(&mut msg, None, 0, 0).as_bool() {
                    let _ = TranslateMessage(&msg);
                    DispatchMessageW(&msg);
                }
            }
            Err(err) => {
                let _ = ready_tx.send(Err(err));
            }
        }
    });

    ready_rx.recv().context("tray thread died")??;
    Ok(rx)
}

unsafe fn create() -> Result<HWND> {
    let instance = GetModuleHandleW(None)?;
    let class_name = w!("orange_tray_icon");

    let class = WNDCLASSW {
        lpfnWndProc: Some(tray_proc),
        hInstance: instance.into(),
        lpszClassName: class_name,
        ..Default::default()
    };
    RegisterClassW(&class);

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

    let mut data = NOTIFYICONDATAW {
        cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
        hWnd: hwnd,
        uID: 1,
        uFlags: NIF_MESSAGE | NIF_ICON | NIF_TIP,
        uCallbackMessage: WM_TRAY,
        // Load the optical small-size entry from the multi-resolution icon;
        // asking for the system tray metric avoids a blurry 32px downscale.
        hIcon: LoadImageW(
            Some(instance.into()),
            PCWSTR(std::ptr::with_exposed_provenance(1)),
            IMAGE_ICON,
            GetSystemMetrics(SM_CXSMICON),
            GetSystemMetrics(SM_CYSMICON),
            LR_DEFAULTCOLOR,
        )
        .map(|handle| HICON(handle.0))
        .or_else(|_| LoadIconW(None, IDI_APPLICATION))?,
        ..Default::default()
    };

    let tip: Vec<u16> = "orange".encode_utf16().chain(std::iter::once(0)).collect();
    data.szTip[..tip.len()].copy_from_slice(&tip);

    if !Shell_NotifyIconW(NIM_ADD, &data).as_bool() {
        anyhow::bail!("Shell_NotifyIcon refused to add the tray icon");
    }
    Ok(hwnd)
}

fn emit(event: TrayEvent) {
    if let Some(tx) = EVENTS.get() {
        let _ = tx.send(event);
    }
}

extern "system" fn tray_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    unsafe {
        match msg {
            WM_TRAY => {
                // The mouse message arrives in the low word of lparam.
                match (lparam.0 as u32) & 0xFFFF {
                    x if x == WM_LBUTTONUP => emit(TrayEvent::Show),
                    x if x == WM_RBUTTONUP => show_menu(hwnd),
                    _ => {}
                }
                LRESULT(0)
            }
            WM_COMMAND => {
                match wparam.0 & 0xFFFF {
                    ID_SHOW => emit(TrayEvent::Show),
                    ID_QUIT => emit(TrayEvent::Quit),
                    _ => {}
                }
                LRESULT(0)
            }
            WM_DESTROY => {
                let data = NOTIFYICONDATAW {
                    cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
                    hWnd: hwnd,
                    uID: 1,
                    ..Default::default()
                };
                let _ = Shell_NotifyIconW(NIM_DELETE, &data);
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
