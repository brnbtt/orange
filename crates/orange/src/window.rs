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
use windows::Win32::Graphics::Gdi::{CreateSolidBrush, HBRUSH, ScreenToClient};
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
pub fn spawn(
    title: &str,
    width: i32,
    height: i32,
    overlay: crate::overlay::SharedOverlay,
) -> Result<VideoWindow> {
    let (tx, rx) = mpsc::channel::<Result<isize>>();
    let title: Vec<u16> = title.encode_utf16().chain(std::iter::once(0)).collect();

    std::thread::spawn(move || unsafe {
        match create_window(&title, width, height, overlay) {
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

/// Per-window state reachable from the window procedure.
struct WindowContext {
    overlay: crate::overlay::SharedOverlay,
}

/// Map a point in client coordinates to the video's coordinate space.
///
/// The sink letterboxes to preserve aspect ratio, so the video does not fill
/// the client area and a naive mapping would put the controls in the wrong
/// place on any window that is not exactly the video's aspect.
fn client_to_video(hwnd: HWND, cx: f32, cy: f32, video: (u32, u32)) -> Option<(f32, f32)> {
    let (vw, vh) = video;
    if vw == 0 || vh == 0 {
        return None;
    }
    let mut rect = RECT::default();
    unsafe { GetClientRect(hwnd, &mut rect).ok()? };
    let cw = (rect.right - rect.left) as f32;
    let ch = (rect.bottom - rect.top) as f32;
    if cw <= 0.0 || ch <= 0.0 {
        return None;
    }
    let scale = (cw / vw as f32).min(ch / vh as f32);
    let dw = vw as f32 * scale;
    let dh = vh as f32 * scale;
    let ox = (cw - dw) / 2.0;
    let oy = (ch - dh) / 2.0;
    Some(((cx - ox) / scale, (cy - oy) / scale))
}

unsafe fn context(hwnd: HWND) -> Option<&'static WindowContext> {
    let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *const WindowContext;
    ptr.as_ref()
}

unsafe fn create_window(
    title: &[u16],
    width: i32,
    height: i32,
    overlay: crate::overlay::SharedOverlay,
) -> Result<HWND> {
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

    // Leaked deliberately and reclaimed in WM_DESTROY.
    let ctx = Box::into_raw(Box::new(WindowContext { overlay }));
    SetWindowLongPtrW(hwnd, GWLP_USERDATA, ctx as isize);

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
            // Hit testing does double duty. It fires on every mouse move, so
            // it wakes the controls and updates hover; and it decides whether
            // this point should drag the window or receive a normal click.
            //
            // Without the second part, HTCAPTION would swallow every click and
            // the controls would be impossible to press.
            WM_NCHITTEST => {
                let hit = DefWindowProcW(hwnd, msg, wparam, lparam);
                if hit.0 != HTCLIENT as isize {
                    return hit; // resize borders keep their behaviour
                }

                let screen_x = (lparam.0 & 0xFFFF) as i16 as i32;
                let screen_y = ((lparam.0 >> 16) & 0xFFFF) as i16 as i32;
                let mut point = POINT {
                    x: screen_x,
                    y: screen_y,
                };
                let _ = ScreenToClient(hwnd, &mut point);

                let over_control = context(hwnd)
                    .and_then(|ctx| {
                        let mut overlay = ctx.overlay.lock().ok()?;
                        let video = overlay.video;
                        let (vx, vy) =
                            client_to_video(hwnd, point.x as f32, point.y as f32, video)?;
                        Some(overlay.on_mouse_move(vx, vy))
                    })
                    .unwrap_or(false);

                let hot = context(hwnd)
                    .and_then(|ctx| ctx.overlay.lock().ok().map(|o| o.hovered()))
                    .unwrap_or(false);

                let _ = over_control;
                if hot {
                    LRESULT(HTCLIENT as isize)
                } else {
                    LRESULT(HTCAPTION as isize)
                }
            }
            WM_LBUTTONDOWN => {
                let x = (lparam.0 & 0xFFFF) as i16 as f32;
                let y = ((lparam.0 >> 16) & 0xFFFF) as i16 as f32;
                if let Some(ctx) = context(hwnd) {
                    if let Ok(mut overlay) = ctx.overlay.lock() {
                        let video = overlay.video;
                        if let Some((vx, vy)) = client_to_video(hwnd, x, y, video) {
                            overlay.on_click(vx, vy);
                            if overlay.close_requested {
                                let _ =
                                    PostMessageW(Some(hwnd), WM_CLOSE, WPARAM(0), LPARAM(0));
                            }
                        }
                    }
                }
                LRESULT(0)
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
                let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut WindowContext;
                if !ptr.is_null() {
                    drop(Box::from_raw(ptr));
                    SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
                }
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
