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
use std::sync::mpsc;
use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Dwm::{
    DwmSetWindowAttribute, DWMWA_WINDOW_CORNER_PREFERENCE, DWMWCP_ROUND,
};
use windows::Win32::Graphics::Gdi::{CreateSolidBrush, HBRUSH};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Input::KeyboardAndMouse::VK_ESCAPE;
use windows::Win32::UI::WindowsAndMessaging::*;

/// Cosmetic only: the frame behind the video, visible for an instant before
/// the first frame arrives and in the letterbox bars.
const BACKGROUND: COLORREF = COLORREF(0x00141414); // BGR

pub struct VideoWindow {
    pub hwnd: isize,
}

// The HWND is used from the GStreamer thread to hand to the sink. Win32 window
// handles are process-wide, so this is sound; only message-loop calls are
// thread-affine.
unsafe impl Send for VideoWindow {}

/// Create the viewer window and run its message loop on a dedicated thread.
pub fn spawn(title: &str, width: i32, height: i32) -> Result<VideoWindow> {
    let (tx, rx) = mpsc::channel::<Result<isize>>();
    let title: Vec<u16> = title.encode_utf16().chain(std::iter::once(0)).collect();

    std::thread::spawn(move || unsafe {
        match create_window(&title, width, height) {
            Ok(hwnd) => {
                if tx.send(Ok(hwnd.0 as isize)).is_err() {
                    return;
                }
                run_message_loop();
            }
            Err(err) => {
                let _ = tx.send(Err(err));
            }
        }
    });

    match rx.recv() {
        Ok(Ok(hwnd)) => Ok(VideoWindow { hwnd }),
        Ok(Err(err)) => Err(err),
        Err(_) => bail!("window thread died before it was ready"),
    }
}

unsafe fn create_window(title: &[u16], width: i32, height: i32) -> Result<HWND> {
    let instance = GetModuleHandleW(None)?;
    let class_name = w!("orange_viewer");

    let class = WNDCLASSW {
        style: CS_HREDRAW | CS_VREDRAW,
        lpfnWndProc: Some(wnd_proc),
        hInstance: instance.into(),
        lpszClassName: class_name,
        hCursor: LoadCursorW(None, IDC_ARROW)?,
        hbrBackground: HBRUSH(CreateSolidBrush(BACKGROUND).0),
        ..Default::default()
    };
    // A zero return can mean "already registered", which is fine.
    RegisterClassW(&class);

    // Centre on the primary monitor.
    let screen_w = GetSystemMetrics(SM_CXSCREEN);
    let screen_h = GetSystemMetrics(SM_CYSCREEN);
    let x = (screen_w - width) / 2;
    let y = (screen_h - height) / 2;

    let hwnd = CreateWindowExW(
        WINDOW_EX_STYLE::default(),
        class_name,
        PCWSTR(title.as_ptr()),
        // WS_POPUP: no title bar, no border. WS_THICKFRAME is kept so the
        // window can still be resized from its edges.
        WS_POPUP | WS_THICKFRAME | WS_MINIMIZEBOX,
        x,
        y,
        width,
        height,
        None,
        None,
        Some(instance.into()),
        None,
    )?;

    // Rounded corners, one call. Windows 11 only; older builds ignore it.
    let pref = DWMWCP_ROUND;
    let _ = DwmSetWindowAttribute(
        hwnd,
        DWMWA_WINDOW_CORNER_PREFERENCE,
        &pref as *const _ as *const _,
        std::mem::size_of_val(&pref) as u32,
    );

    let _ = ShowWindow(hwnd, SW_SHOW);
    Ok(hwnd)
}

unsafe fn run_message_loop() {
    let mut msg = MSG::default();
    while GetMessageW(&mut msg, None, 0, 0).as_bool() {
        let _ = TranslateMessage(&msg);
        DispatchMessageW(&msg);
    }
}

extern "system" fn wnd_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    unsafe {
        match msg {
            // With no title bar there is nothing to grab, so the whole surface
            // acts as one, except a margin at the edges left for resizing.
            WM_NCHITTEST => {
                let hit = DefWindowProcW(hwnd, msg, wparam, lparam);
                if hit.0 == HTCLIENT as isize {
                    return LRESULT(HTCAPTION as isize);
                }
                hit
            }
            WM_KEYDOWN if wparam.0 == VK_ESCAPE.0 as usize => {
                let _ = PostMessageW(Some(hwnd), WM_CLOSE, WPARAM(0), LPARAM(0));
                LRESULT(0)
            }
            // Double click toggles maximise, matching what people expect from
            // a borderless media window.
            WM_LBUTTONDBLCLK => {
                let mut placement = WINDOWPLACEMENT {
                    length: std::mem::size_of::<WINDOWPLACEMENT>() as u32,
                    ..Default::default()
                };
                let _ = GetWindowPlacement(hwnd, &mut placement);
                let cmd = if placement.showCmd == SW_MAXIMIZE.0 as u32 {
                    SW_RESTORE
                } else {
                    SW_MAXIMIZE
                };
                let _ = ShowWindow(hwnd, cmd);
                LRESULT(0)
            }
            WM_DESTROY => {
                PostQuitMessage(0);
                LRESULT(0)
            }
            _ => DefWindowProcW(hwnd, msg, wparam, lparam),
        }
    }
}

/// Whether the window still exists, so the viewer can exit when it is closed.
pub fn is_alive(hwnd: isize) -> bool {
    unsafe { IsWindow(Some(HWND(hwnd as *mut _))).as_bool() }
}

/// Keep the video sized to the window. Called on resize.
#[allow(dead_code)]
pub fn client_size(hwnd: isize) -> Option<(i32, i32)> {
    unsafe {
        let mut rect = RECT::default();
        GetClientRect(HWND(hwnd as *mut _), &mut rect).ok()?;
        Some((rect.right - rect.left, rect.bottom - rect.top))
    }
}

#[allow(dead_code)]
pub fn cursor_pos() -> Option<POINT> {
    unsafe {
        let mut p = POINT::default();
        GetCursorPos(&mut p).ok()?;
        Some(p)
    }
}
