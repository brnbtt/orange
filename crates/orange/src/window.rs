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
    DwmSetWindowAttribute, DWMWA_BORDER_COLOR, DWMWA_COLOR_NONE, DWMWA_WINDOW_CORNER_PREFERENCE,
    DWMWCP_DONOTROUND, DWMWCP_ROUND,
};
use windows::Win32::Graphics::Gdi::{
    CreateSolidBrush, EnumDisplaySettingsW, GetMonitorInfoW, MonitorFromWindow, ScreenToClient,
    DEVMODEW, ENUM_CURRENT_SETTINGS, HBRUSH, MONITORINFO, MONITORINFOEXW, MONITOR_DEFAULTTONEAREST,
    MONITOR_DEFAULTTOPRIMARY,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::HiDpi::{
    GetDpiForSystem, GetDpiForWindow, GetSystemMetricsForDpi, SetProcessDpiAwarenessContext,
    DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{ReleaseCapture, SetCapture, VK_ESCAPE, VK_F11};
use windows::Win32::UI::WindowsAndMessaging::*;

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

        let mut mode = DEVMODEW::default();
        mode.dmSize = std::mem::size_of::<DEVMODEW>() as u16;
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

/// Cosmetic only: the frame behind the video, visible for an instant before
/// the first frame arrives and in the letterbox bars.
const BACKGROUND: COLORREF = COLORREF(0x000b0b0b); // BGR

/// Drives cursor hiding. Windows only asks about the cursor when the mouse
/// moves, and the point is to hide it when the mouse has stopped.
const CURSOR_TIMER: usize = 1;
const REVEAL_MESSAGE: u32 = WM_APP + 1;
const ASPECT_MESSAGE: u32 = WM_APP + 2;
const REVEAL_MS: u32 = 180;

pub struct VideoWindow {
    pub hwnd: isize,
}

#[derive(Clone, Copy)]
enum WindowRole {
    Viewer { cascade: u32 },
    Monitor,
}

impl WindowRole {
    fn is_monitor(self) -> bool {
        matches!(self, Self::Monitor)
    }
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
    spawn_window(
        title,
        width,
        height,
        WindowRole::Viewer { cascade: 0 },
        overlay,
    )
}

pub fn spawn_cascaded(
    title: &str,
    width: i32,
    height: i32,
    cascade: u32,
    overlay: crate::overlay::SharedOverlay,
) -> Result<VideoWindow> {
    spawn_window(
        title,
        width,
        height,
        WindowRole::Viewer { cascade },
        overlay,
    )
}

pub fn spawn_monitor(title: &str, overlay: crate::overlay::SharedOverlay) -> Result<VideoWindow> {
    spawn_window(title, 480, 270, WindowRole::Monitor, overlay)
}

fn spawn_window(
    title: &str,
    width: i32,
    height: i32,
    role: WindowRole,
    overlay: crate::overlay::SharedOverlay,
) -> Result<VideoWindow> {
    let (tx, rx) = mpsc::channel::<Result<isize>>();
    let title: Vec<u16> = title.encode_utf16().chain(std::iter::once(0)).collect();

    std::thread::spawn(move || unsafe {
        match create_window(&title, width, height, role, overlay) {
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
    revealed: std::cell::Cell<bool>,
    role: WindowRole,
    /// Style and bounds to put back when leaving fullscreen. `Some` means we
    /// are currently fullscreen.
    restore: std::cell::Cell<Option<(WINDOW_STYLE, RECT)>>,
}

/// Reveal a prepared video window on its owning thread.
pub fn reveal(hwnd: isize) {
    unsafe {
        let _ = PostMessageW(
            Some(HWND(hwnd as *mut _)),
            REVEAL_MESSAGE,
            WPARAM(0),
            LPARAM(0),
        );
    }
}

pub fn set_video_aspect(hwnd: isize, width: u32, height: u32) {
    unsafe {
        let _ = PostMessageW(
            Some(HWND(hwnd as *mut _)),
            ASPECT_MESSAGE,
            WPARAM(width as usize),
            LPARAM(height as isize),
        );
    }
}

fn fit_aspect(
    max_width: i32,
    max_height: i32,
    source_width: u32,
    source_height: u32,
) -> (i32, i32) {
    let ratio = source_width as f32 / source_height as f32;
    if max_width as f32 / max_height as f32 > ratio {
        ((max_height as f32 * ratio).round() as i32, max_height)
    } else {
        (max_width, (max_width as f32 / ratio).round() as i32)
    }
}

unsafe fn resize_to_video_aspect(hwnd: HWND, width: u32, height: u32) {
    if width == 0 || height == 0 {
        return;
    }
    let Some(ctx) = context(hwnd) else { return };
    if ctx.restore.get().is_some() {
        return;
    }
    let mut rect = RECT::default();
    if GetWindowRect(hwnd, &mut rect).is_err() {
        return;
    }
    if ctx.role.is_monitor() {
        return; // fixed PiP shell; the sink letterboxes source content inside it
    }
    let old_w = rect.right - rect.left;
    let old_h = rect.bottom - rect.top;
    let (new_w, new_h) = fit_aspect(old_w, old_h, width, height);
    let x = rect.left + (old_w - new_w) / 2;
    let y = rect.top + (old_h - new_h) / 2;
    let _ = SetWindowPos(
        hwnd,
        None,
        x,
        y,
        new_w,
        new_h,
        SWP_NOZORDER | SWP_NOACTIVATE,
    );
}

#[cfg(test)]
mod tests {
    use super::fit_aspect;

    #[test]
    fn source_aspects_fit_inside_a_bounded_viewer_envelope() {
        assert_eq!(fit_aspect(720, 405, 1920, 1080), (720, 405));
        assert_eq!(fit_aspect(720, 405, 1980, 1793), (447, 405));
        assert_eq!(fit_aspect(720, 405, 1080, 1920), (228, 405));
        assert_eq!(fit_aspect(720, 405, 2560, 1080), (720, 304));
    }
}

unsafe fn constrain_sizing(hwnd: HWND, edge: usize, rect: &mut RECT) -> bool {
    let Some(ctx) = context(hwnd) else {
        return false;
    };
    if ctx.role.is_monitor() {
        return false;
    }
    let video = ctx.overlay.lock().ok().map(|state| state.video);
    let Some((width, height)) = video.filter(|(w, h)| *w > 0 && *h > 0) else {
        return false;
    };
    let ratio = width as f32 / height as f32;
    let current_w = rect.right - rect.left;
    let current_h = rect.bottom - rect.top;

    match edge as u32 {
        WMSZ_TOP => rect.right = rect.left + (current_h as f32 * ratio).round() as i32,
        WMSZ_BOTTOM => rect.right = rect.left + (current_h as f32 * ratio).round() as i32,
        WMSZ_TOPLEFT | WMSZ_TOPRIGHT => {
            rect.top = rect.bottom - (current_w as f32 / ratio).round() as i32;
        }
        WMSZ_LEFT | WMSZ_RIGHT | WMSZ_BOTTOMLEFT | WMSZ_BOTTOMRIGHT => {
            rect.bottom = rect.top + (current_w as f32 / ratio).round() as i32;
        }
        _ => return false,
    }
    true
}

/// Tell the overlay what display scaling it is being shown at.
unsafe fn sync_dpi(hwnd: HWND) {
    let dpi = GetDpiForWindow(hwnd);
    if dpi == 0 {
        return;
    }
    if let Some(ctx) = context(hwnd) {
        if let Ok(mut overlay) = ctx.overlay.lock() {
            overlay.dpi = dpi as f32 / 96.0;
        }
    }
}

unsafe fn set_corner_style(hwnd: HWND, fullscreen: bool) {
    let pref = if fullscreen {
        DWMWCP_DONOTROUND
    } else {
        DWMWCP_ROUND
    };
    let _ = DwmSetWindowAttribute(
        hwnd,
        DWMWA_WINDOW_CORNER_PREFERENCE,
        &pref as *const _ as *const _,
        std::mem::size_of_val(&pref) as u32,
    );
}

/// Fill the monitor the window is currently on, or go back to where it was.
///
/// Borderless already, so this is only a matter of dropping the resize frame
/// and taking the monitor's bounds - and of using *this* window's monitor
/// rather than the primary one, which is the part people notice.
unsafe fn toggle_fullscreen(hwnd: HWND) {
    let Some(ctx) = context(hwnd) else { return };
    if ctx.role.is_monitor() {
        return;
    }

    if let Some((style, bounds)) = ctx.restore.take() {
        SetWindowLongPtrW(hwnd, GWL_STYLE, style.0 as isize);
        let _ = SetWindowPos(
            hwnd,
            None,
            bounds.left,
            bounds.top,
            bounds.right - bounds.left,
            bounds.bottom - bounds.top,
            SWP_NOZORDER | SWP_FRAMECHANGED,
        );
    } else {
        let style = WINDOW_STYLE(GetWindowLongPtrW(hwnd, GWL_STYLE) as u32);
        let mut bounds = RECT::default();
        let _ = GetWindowRect(hwnd, &mut bounds);

        let monitor = MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST);
        let mut info = MONITORINFO {
            cbSize: std::mem::size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        if !GetMonitorInfoW(monitor, &mut info).as_bool() {
            return;
        }

        ctx.restore.set(Some((style, bounds)));
        SetWindowLongPtrW(hwnd, GWL_STYLE, (style & !WS_THICKFRAME).0 as isize);
        let screen = info.rcMonitor;
        let _ = SetWindowPos(
            hwnd,
            Some(HWND_TOP),
            screen.left,
            screen.top,
            screen.right - screen.left,
            screen.bottom - screen.top,
            SWP_FRAMECHANGED,
        );
    }

    if let Ok(mut overlay) = ctx.overlay.lock() {
        overlay.fullscreen = ctx.restore.get().is_some();
        overlay.wake();
    }
    set_corner_style(hwnd, ctx.restore.get().is_some());
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

/// Resize hit-testing for a frame whose client area fills the whole window.
///
/// Keeping `WS_THICKFRAME` gives Windows native resize and snap behaviour, but
/// `WM_NCCALCSIZE` removes its visible non-client strips. We therefore identify
/// the edges ourselves instead of relying on the frame that is no longer there.
unsafe fn resize_hit_test(hwnd: HWND, x: i32, y: i32) -> Option<LRESULT> {
    let Some(ctx) = context(hwnd) else {
        return None;
    };
    if ctx.role.is_monitor() || ctx.restore.get().is_some() {
        return None; // no resize edges in fullscreen
    }

    let mut rect = RECT::default();
    GetWindowRect(hwnd, &mut rect).ok()?;
    if x < rect.left || x >= rect.right || y < rect.top || y >= rect.bottom {
        return None;
    }
    let dpi = GetDpiForWindow(hwnd).max(96);
    let edge_x =
        GetSystemMetricsForDpi(SM_CXFRAME, dpi) + GetSystemMetricsForDpi(SM_CXPADDEDBORDER, dpi);
    let edge_y =
        GetSystemMetricsForDpi(SM_CYFRAME, dpi) + GetSystemMetricsForDpi(SM_CXPADDEDBORDER, dpi);
    let left = x < rect.left + edge_x;
    let right = x >= rect.right - edge_x;
    let top = y < rect.top + edge_y;
    let bottom = y >= rect.bottom - edge_y;

    let hit = match (left, right, top, bottom) {
        (true, _, true, _) => HTTOPLEFT,
        (_, true, true, _) => HTTOPRIGHT,
        (true, _, _, true) => HTBOTTOMLEFT,
        (_, true, _, true) => HTBOTTOMRIGHT,
        (true, _, _, _) => HTLEFT,
        (_, true, _, _) => HTRIGHT,
        (_, _, true, _) => HTTOP,
        (_, _, _, true) => HTBOTTOM,
        _ => return None,
    };
    Some(LRESULT(hit as isize))
}

unsafe fn context(hwnd: HWND) -> Option<&'static WindowContext> {
    let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *const WindowContext;
    ptr.as_ref()
}

unsafe fn create_window(
    title: &[u16],
    width: i32,
    height: i32,
    role: WindowRole,
    overlay: crate::overlay::SharedOverlay,
) -> Result<HWND> {
    let instance = GetModuleHandleW(None)?;
    let class_name = w!("orange_viewer");

    let class = WNDCLASSW {
        style: CS_HREDRAW | CS_VREDRAW,
        lpfnWndProc: Some(wnd_proc),
        hInstance: instance.into(),
        lpszClassName: class_name,
        hIcon: LoadIconW(Some(instance.into()), PCWSTR(1 as *const u16)).unwrap_or_default(),
        hCursor: LoadCursorW(None, IDC_ARROW)?,
        hbrBackground: HBRUSH(CreateSolidBrush(BACKGROUND).0),
        ..Default::default()
    };
    // A zero return can mean "already registered", which is fine.
    RegisterClassW(&class);

    // Callers pass logical sizes. Now that the process is DPI aware, scale
    // them so the window covers the same area of screen as before - the
    // difference being that it is now backed by real pixels rather than an
    // upscale of two thirds as many.
    let scale = GetDpiForSystem() as f32 / 96.0;
    let width = (width as f32 * scale).round() as i32;
    let height = (height as f32 * scale).round() as i32;

    let (x, y) = if role.is_monitor() {
        let monitor = MonitorFromWindow(HWND::default(), MONITOR_DEFAULTTOPRIMARY);
        let mut info = MONITORINFO {
            cbSize: std::mem::size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        GetMonitorInfoW(monitor, &mut info).ok()?;
        let margin = (24.0 * scale).round() as i32;
        (
            info.rcWork.right - width - margin,
            info.rcWork.bottom - height - margin,
        )
    } else {
        let screen_w = GetSystemMetrics(SM_CXSCREEN);
        let screen_h = GetSystemMetrics(SM_CYSCREEN);
        let WindowRole::Viewer { cascade } = role else {
            unreachable!()
        };
        let offset = (cascade.min(5) as f32 * 32.0 * scale).round() as i32;
        (
            ((screen_w - width) / 2 + offset).min((screen_w - width).max(0)),
            ((screen_h - height) / 2 + offset).min((screen_h - height).max(0)),
        )
    };

    let hwnd = CreateWindowExW(
        if role.is_monitor() {
            WS_EX_TOPMOST
        } else {
            WINDOW_EX_STYLE::default()
        },
        class_name,
        PCWSTR(title.as_ptr()),
        // WS_POPUP: no title bar, no border. WS_THICKFRAME is kept so the
        // window can still be resized from its edges.
        if role.is_monitor() {
            WS_POPUP | WS_MINIMIZEBOX
        } else {
            WS_POPUP | WS_THICKFRAME | WS_MINIMIZEBOX
        },
        x,
        y,
        width,
        height,
        None,
        None,
        Some(instance.into()),
        None,
    )?;

    // Rounded in a normal window, square and edge-to-edge in fullscreen.
    // Windows 11 only; older builds ignore it.
    set_corner_style(hwnd, false);
    // Windows 11 otherwise paints a one-pixel white activation border around
    // the custom frame, most visibly across the top over dark video.
    let border = DWMWA_COLOR_NONE;
    let _ = DwmSetWindowAttribute(
        hwnd,
        DWMWA_BORDER_COLOR,
        &border as *const _ as *const _,
        std::mem::size_of_val(&border) as u32,
    );

    // Leaked deliberately and reclaimed in WM_DESTROY.
    let ctx = Box::into_raw(Box::new(WindowContext {
        overlay,
        revealed: std::cell::Cell::new(false),
        role,
        restore: std::cell::Cell::new(None),
    }));
    SetWindowLongPtrW(hwnd, GWLP_USERDATA, ctx as isize);

    sync_dpi(hwnd);

    SetTimer(Some(hwnd), CURSOR_TIMER, 250, None);
    Ok(hwnd)
}

/// Whether the pointer is inside this window's client area.
///
/// Checked before hiding it, so a timer tick never blanks the cursor while it
/// is over somebody else's window.
unsafe fn cursor_inside(hwnd: HWND) -> bool {
    let mut point = POINT::default();
    if GetCursorPos(&mut point).is_err() {
        return false;
    }
    if ScreenToClient(hwnd, &mut point).as_bool() {
        let mut rect = RECT::default();
        if GetClientRect(hwnd, &mut rect).is_ok() {
            return point.x >= rect.left
                && point.x < rect.right
                && point.y >= rect.top
                && point.y < rect.bottom;
        }
    }
    false
}

/// Whether the controls are currently on screen. The cursor follows them.
unsafe fn controls_visible(hwnd: HWND) -> bool {
    context(hwnd)
        .and_then(|ctx| ctx.overlay.lock().ok().map(|o| o.visible()))
        .unwrap_or(true)
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
            ASPECT_MESSAGE => {
                resize_to_video_aspect(hwnd, wparam.0 as u32, lparam.0 as u32);
                LRESULT(0)
            }
            REVEAL_MESSAGE => {
                if let Some(ctx) = context(hwnd) {
                    if !ctx.revealed.replace(true) {
                        if AnimateWindow(hwnd, REVEAL_MS, AW_BLEND | AW_ACTIVATE).is_err() {
                            let _ = ShowWindow(hwnd, SW_SHOW);
                        }
                    }
                }
                LRESULT(0)
            }
            // Let video occupy the complete window while retaining
            // `WS_THICKFRAME` for native resizing and snap layouts.
            WM_NCCALCSIZE if wparam.0 != 0 => LRESULT(0),
            WM_SIZING => {
                let rect = &mut *(lparam.0 as *mut RECT);
                LRESULT(constrain_sizing(hwnd, wparam.0, rect) as isize)
            }
            // Hit testing does double duty. It fires on every mouse move, so
            // it wakes the controls and updates hover; and it decides whether
            // this point should drag the window or receive a normal click.
            //
            // Without the second part, HTCAPTION would swallow every click and
            // the controls would be impossible to press.
            WM_NCHITTEST => {
                let screen_x = (lparam.0 & 0xFFFF) as i16 as i32;
                let screen_y = ((lparam.0 >> 16) & 0xFFFF) as i16 as i32;
                if let Some(hit) = resize_hit_test(hwnd, screen_x, screen_y) {
                    return hit;
                }
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
                let mut close = false;
                let mut fullscreen = false;
                let mut volume_dragging = false;
                if let Some(ctx) = context(hwnd) {
                    if let Ok(mut overlay) = ctx.overlay.lock() {
                        let video = overlay.video;
                        if let Some((vx, vy)) = client_to_video(hwnd, x, y, video) {
                            overlay.on_click(vx, vy);
                            close = overlay.close_requested;
                            fullscreen = std::mem::take(&mut overlay.fullscreen_requested);
                            volume_dragging = overlay.volume_dragging();
                        }
                    }
                }
                // Outside the lock: toggling fullscreen takes it again.
                if close {
                    let _ = PostMessageW(Some(hwnd), WM_CLOSE, WPARAM(0), LPARAM(0));
                } else if fullscreen {
                    toggle_fullscreen(hwnd);
                } else if volume_dragging {
                    SetCapture(hwnd);
                }
                LRESULT(0)
            }
            WM_MOUSEMOVE => {
                let x = (lparam.0 & 0xFFFF) as i16 as f32;
                let y = ((lparam.0 >> 16) & 0xFFFF) as i16 as f32;
                if let Some(ctx) = context(hwnd) {
                    if let Ok(mut overlay) = ctx.overlay.lock() {
                        if overlay.volume_dragging() {
                            let video = overlay.video;
                            if let Some((vx, _)) = client_to_video(hwnd, x, y, video) {
                                overlay.drag_volume(vx);
                            }
                        }
                    }
                }
                LRESULT(0)
            }
            WM_LBUTTONUP => {
                let released = context(hwnd)
                    .and_then(|ctx| ctx.overlay.lock().ok().map(|mut o| o.end_volume_drag()))
                    .unwrap_or(false);
                if released {
                    let _ = ReleaseCapture();
                }
                LRESULT(0)
            }
            WM_CAPTURECHANGED | WM_CANCELMODE => {
                if let Some(ctx) = context(hwnd) {
                    if let Ok(mut overlay) = ctx.overlay.lock() {
                        overlay.end_volume_drag();
                    }
                }
                LRESULT(0)
            }
            // Keep the overlay's idea of the window in step, so the controls
            // stay the same size on screen as the window is resized.
            WM_SIZE => {
                let width = (lparam.0 & 0xFFFF) as u16 as u32;
                let height = ((lparam.0 >> 16) & 0xFFFF) as u16 as u32;
                if let Some(ctx) = context(hwnd) {
                    if let Ok(mut overlay) = ctx.overlay.lock() {
                        overlay.client = (width, height);
                    }
                }
                LRESULT(0)
            }
            WM_KEYDOWN if wparam.0 == VK_F11.0 as usize => {
                toggle_fullscreen(hwnd);
                LRESULT(0)
            }
            // Escape leaves fullscreen if we are in it, and only closes the
            // viewer otherwise. Quitting outright would be a nasty surprise.
            WM_KEYDOWN if wparam.0 == VK_ESCAPE.0 as usize => {
                let fullscreen = context(hwnd)
                    .map(|ctx| ctx.restore.get().is_some())
                    .unwrap_or(false);
                if fullscreen {
                    toggle_fullscreen(hwnd);
                } else {
                    let _ = PostMessageW(Some(hwnd), WM_CLOSE, WPARAM(0), LPARAM(0));
                }
                LRESULT(0)
            }
            // Double click toggles fullscreen, matching what people expect
            // from a video window.
            //
            // Both variants are needed: anywhere that is not a control hit
            // tests as HTCAPTION, and Windows then delivers double clicks as
            // non-client messages, where the default handling would maximise
            // rather than go fullscreen.
            WM_LBUTTONDBLCLK => {
                toggle_fullscreen(hwnd);
                LRESULT(0)
            }
            WM_NCLBUTTONDBLCLK if wparam.0 == HTCAPTION as usize => {
                toggle_fullscreen(hwnd);
                LRESULT(0)
            }
            // Dragged onto a monitor with different scaling. Windows hands us
            // the rectangle the window should occupy at the new scale; taking
            // it keeps the video at native resolution on both displays.
            WM_DPICHANGED => {
                let suggested = &*(lparam.0 as *const RECT);
                let _ = SetWindowPos(
                    hwnd,
                    None,
                    suggested.left,
                    suggested.top,
                    suggested.right - suggested.left,
                    suggested.bottom - suggested.top,
                    SWP_NOZORDER | SWP_NOACTIVATE,
                );
                sync_dpi(hwnd);
                LRESULT(0)
            }
            // The pointer hides along with the controls, and comes back with
            // them. Windows asks about the cursor on movement; the timer
            // handles the case where the mouse has simply stopped.
            WM_SETCURSOR if (lparam.0 & 0xFFFF) as u32 == HTCLIENT as u32 => {
                if controls_visible(hwnd) {
                    DefWindowProcW(hwnd, msg, wparam, lparam)
                } else {
                    let _ = SetCursor(None);
                    LRESULT(1)
                }
            }
            WM_TIMER if wparam.0 == CURSOR_TIMER => {
                if !controls_visible(hwnd) && cursor_inside(hwnd) {
                    let _ = SetCursor(None);
                }
                LRESULT(0)
            }
            WM_DESTROY => {
                let _ = KillTimer(Some(hwnd), CURSOR_TIMER);
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
