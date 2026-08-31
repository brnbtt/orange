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
use windows::Win32::Foundation::{
    GetLastError, SetLastError, COLORREF, ERROR_SUCCESS, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM,
};
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

/// Cosmetic only: the frame behind the video, visible for an instant before
/// the first frame arrives and in the letterbox bars.
const BACKGROUND: COLORREF = COLORREF(0x000b0b0b); // BGR

/// Drives cursor hiding. Windows only asks about the cursor when the mouse
/// moves, and the point is to hide it when the mouse has stopped.
const CURSOR_TIMER: usize = 1;
const REVEAL_MESSAGE: u32 = WM_APP + 1;
const ASPECT_MESSAGE: u32 = WM_APP + 2;
const REVEAL_MS: u32 = 180;

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

/// One native playback component used for both friend streams and the local
/// live monitor. The profile changes policy, never the media or interaction
/// implementation.
#[derive(Clone)]
pub struct PlaybackWindow {
    hwnd: isize,
    overlay: crate::overlay::SharedOverlay,
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
        let hwnd = spawn_window(title, envelope, profile, overlay.clone())?;
        Ok(Self { hwnd, overlay })
    }

    pub fn hwnd(&self) -> isize {
        self.hwnd
    }

    pub fn overlay(&self) -> &crate::overlay::SharedOverlay {
        &self.overlay
    }

    pub fn is_alive(&self) -> bool {
        is_alive(self.hwnd)
    }

    pub fn is_responsive(&self) -> bool {
        unsafe {
            SendMessageTimeoutW(
                HWND(self.hwnd as *mut _),
                WM_NULL,
                WPARAM(0),
                LPARAM(0),
                SMTO_ABORTIFHUNG,
                250,
                None,
            )
            .0 != 0
        }
    }

    pub fn reveal(&self) {
        reveal(self.hwnd);
    }

    pub fn set_source_size(&self, width: u32, height: u32) {
        set_video_aspect(self.hwnd, width, height);
    }
}

fn spawn_window(
    title: &str,
    envelope: (i32, i32),
    profile: PlaybackProfile,
    overlay: crate::overlay::SharedOverlay,
) -> Result<isize> {
    let (tx, rx) = mpsc::channel::<Result<isize>>();
    let title: Vec<u16> = title.encode_utf16().chain(std::iter::once(0)).collect();

    std::thread::spawn(move || unsafe {
        match create_window(&title, envelope, profile, overlay) {
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
        Ok(Ok(hwnd)) => Ok(hwnd),
        Ok(Err(err)) => Err(err),
        Err(_) => bail!("window thread died before it was ready"),
    }
}

/// Per-window state reachable from the window procedure.
struct WindowContext {
    overlay: crate::overlay::SharedOverlay,
    revealed: std::cell::Cell<bool>,
    profile: PlaybackProfile,
    envelope: std::cell::Cell<(i32, i32)>,
    source: std::cell::Cell<(u32, u32)>,
    size_move_start: std::cell::Cell<(i32, i32)>,
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
    if max_width <= 0 || max_height <= 0 || source_width == 0 || source_height == 0 {
        return (0, 0);
    }

    let max_width = i64::from(max_width);
    let max_height = i64::from(max_height);
    let source_width = i64::from(source_width);
    let source_height = i64::from(source_height);
    if max_width * source_height > max_height * source_width {
        (
            ((max_height * source_width + source_height / 2) / source_height) as i32,
            max_height as i32,
        )
    } else {
        (
            max_width as i32,
            ((max_width * source_height + source_width / 2) / source_width) as i32,
        )
    }
}

fn aspect_locked_size(
    edge: u32,
    current_width: i32,
    current_height: i32,
    source: (u32, u32),
) -> Option<(i32, i32)> {
    let (source_width, source_height) = source;
    if current_width <= 0 || current_height <= 0 || source_width == 0 || source_height == 0 {
        return None;
    }
    let ratio = source_width as f32 / source_height as f32;
    match edge {
        WMSZ_TOP | WMSZ_BOTTOM => Some((
            (current_height as f32 * ratio).round() as i32,
            current_height,
        )),
        WMSZ_TOPLEFT | WMSZ_TOPRIGHT | WMSZ_LEFT | WMSZ_RIGHT | WMSZ_BOTTOMLEFT
        | WMSZ_BOTTOMRIGHT => Some((current_width, (current_width as f32 / ratio).round() as i32)),
        _ => None,
    }
}

unsafe fn resize_to_video_aspect(hwnd: HWND, width: u32, height: u32) {
    if width == 0 || height == 0 {
        return;
    }
    let Some((previous, fullscreen, bounds, profile)) = with_context(hwnd, |ctx| {
        (
            ctx.source.replace((width, height)),
            ctx.restore.get().is_some(),
            ctx.envelope.get(),
            ctx.profile,
        )
    }) else {
        return;
    };
    if fullscreen {
        return;
    }
    let mut rect = RECT::default();
    if GetWindowRect(hwnd, &mut rect).is_err() {
        return;
    }
    let old_w = rect.right - rect.left;
    let old_h = rect.bottom - rect.top;
    let first_size = previous == (0, 0);
    let (new_w, new_h) = fit_aspect(bounds.0, bounds.1, width, height);
    let (x, y) = if first_size {
        playback_position(profile, new_w, new_h)
    } else if profile == PlaybackProfile::LiveMonitor {
        (rect.right - new_w, rect.bottom - new_h)
    } else {
        (
            rect.left + (old_w - new_w) / 2,
            rect.top + (old_h - new_h) / 2,
        )
    };
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
#[allow(clippy::items_after_test_module)]
mod tests {
    use super::{aspect_locked_size, fit_aspect};
    use windows::Win32::UI::WindowsAndMessaging::{
        WMSZ_BOTTOM, WMSZ_BOTTOMLEFT, WMSZ_BOTTOMRIGHT, WMSZ_LEFT, WMSZ_RIGHT, WMSZ_TOP,
        WMSZ_TOPLEFT, WMSZ_TOPRIGHT,
    };

    #[test]
    fn source_aspects_fit_inside_profile_envelopes() {
        assert_eq!(fit_aspect(1280, 720, 1920, 1080), (1280, 720));
        assert_eq!(fit_aspect(1280, 720, 2560, 1080), (1280, 540));
        assert_eq!(fit_aspect(1280, 720, 1080, 1080), (720, 720));
        assert_eq!(fit_aspect(1280, 720, 1980, 1793), (795, 720));
        assert_eq!(fit_aspect(1280, 720, 1080, 1920), (405, 720));

        assert_eq!(fit_aspect(480, 270, 1920, 1080), (480, 270));
        assert_eq!(fit_aspect(480, 270, 2560, 1080), (480, 203));
        assert_eq!(fit_aspect(480, 270, 1080, 1080), (270, 270));
        assert_eq!(fit_aspect(480, 270, 1980, 1793), (298, 270));
        assert_eq!(fit_aspect(480, 270, 1080, 1920), (152, 270));
    }

    #[test]
    fn fitted_sizes_preserve_source_ratio_with_rounding() {
        for source in [
            (1920, 1080),
            (2560, 1080),
            (1080, 1080),
            (1980, 1793),
            (1080, 1920),
        ] {
            let (width, height) = fit_aspect(937, 611, source.0, source.1);
            let actual = width as f64 / height as f64;
            let expected = source.0 as f64 / source.1 as f64;
            assert!((actual - expected).abs() <= 1.0 / height as f64);
        }
    }

    #[test]
    fn edge_resizing_preserves_every_source_ratio() {
        for source in [
            (1920, 1080),
            (2560, 1080),
            (1080, 1080),
            (1980, 1793),
            (1080, 1920),
        ] {
            for edge in [
                WMSZ_LEFT,
                WMSZ_RIGHT,
                WMSZ_TOP,
                WMSZ_BOTTOM,
                WMSZ_TOPLEFT,
                WMSZ_TOPRIGHT,
                WMSZ_BOTTOMLEFT,
                WMSZ_BOTTOMRIGHT,
            ] {
                let (width, height) = aspect_locked_size(edge, 937, 611, source).unwrap();
                let actual = width as f64 / height as f64;
                let expected = source.0 as f64 / source.1 as f64;
                assert!((actual - expected).abs() <= 1.0 / height as f64);
            }
        }
    }
}

unsafe fn constrain_sizing(hwnd: HWND, edge: usize, rect: &mut RECT) -> bool {
    let Some((width, height)) = with_context(hwnd, |ctx| ctx.source.get())
        .filter(|(width, height)| *width > 0 && *height > 0)
    else {
        return false;
    };
    let current_w = rect.right - rect.left;
    let current_h = rect.bottom - rect.top;
    let Some((locked_w, locked_h)) =
        aspect_locked_size(edge as u32, current_w, current_h, (width, height))
    else {
        return false;
    };

    match edge as u32 {
        WMSZ_TOP | WMSZ_BOTTOM => rect.right = rect.left + locked_w,
        WMSZ_TOPLEFT | WMSZ_TOPRIGHT => {
            rect.top = rect.bottom - locked_h;
        }
        WMSZ_LEFT | WMSZ_RIGHT | WMSZ_BOTTOMLEFT | WMSZ_BOTTOMRIGHT => {
            rect.bottom = rect.top + locked_h;
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
    let Some(overlay) = with_context(hwnd, |ctx| ctx.overlay.clone()) else {
        return;
    };
    if let Ok(mut overlay) = overlay.lock() {
        overlay.dpi = dpi as f32 / 96.0;
    };
}

unsafe fn sync_client_size(hwnd: HWND) {
    let mut rect = RECT::default();
    if GetClientRect(hwnd, &mut rect).is_err() {
        return;
    }
    let Some(overlay) = with_context(hwnd, |ctx| ctx.overlay.clone()) else {
        return;
    };
    if let Ok(mut overlay) = overlay.lock() {
        overlay.client = (
            (rect.right - rect.left).max(0) as u32,
            (rect.bottom - rect.top).max(0) as u32,
        );
    };
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
    let Some(restore) = with_context(hwnd, |ctx| ctx.restore.take()) else {
        return;
    };
    let was_fullscreen = restore.is_some();

    if let Some((style, bounds)) = restore {
        SetWindowLongPtrW(hwnd, GWL_STYLE, style.0 as isize);
        if with_context(hwnd, |_| ()).is_none() {
            return;
        }
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
        if with_context(hwnd, |_| ()).is_none() {
            return;
        }
        let mut bounds = RECT::default();
        let _ = GetWindowRect(hwnd, &mut bounds);
        if with_context(hwnd, |_| ()).is_none() {
            return;
        }

        let monitor = MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST);
        let mut info = MONITORINFO {
            cbSize: std::mem::size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        if !GetMonitorInfoW(monitor, &mut info).as_bool() {
            return;
        }

        if with_context(hwnd, |ctx| ctx.restore.set(Some((style, bounds)))).is_none() {
            return;
        }
        SetWindowLongPtrW(hwnd, GWL_STYLE, (style & !WS_THICKFRAME).0 as isize);
        if with_context(hwnd, |_| ()).is_none() {
            return;
        }
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

    let Some((overlay, fullscreen)) = with_context(hwnd, |ctx| {
        (ctx.overlay.clone(), ctx.restore.get().is_some())
    }) else {
        return;
    };
    if let Ok(mut overlay) = overlay.lock() {
        overlay.fullscreen = fullscreen;
        overlay.wake();
    }
    set_corner_style(hwnd, fullscreen);
    if was_fullscreen {
        let Some((width, height)) = with_context(hwnd, |ctx| ctx.source.get()) else {
            return;
        };
        resize_to_video_aspect(hwnd, width, height);
    }
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
    if with_context(hwnd, |ctx| ctx.restore.get().is_some())? {
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

/// Runs `action` synchronously with this window's installed context.
///
/// # Safety
///
/// `hwnd` must be accessed synchronously on its owning window thread, and
/// `action` must not destroy the window or call APIs that can dispatch window
/// messages. `GWLP_USERDATA` must be null or the pointer installed by
/// `Box::into_raw` for this HWND. A non-null pointer must remain properly
/// aligned, initialized, and dereferenceable for one `WindowContext` throughout
/// `action`, with no overlapping mutable access except through `UnsafeCell`
/// interior mutability. `WM_DESTROY` removes the pointer before freeing it.
unsafe fn with_context<R>(
    hwnd: HWND,
    action: impl for<'a> FnOnce(&'a WindowContext) -> R,
) -> Option<R> {
    let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *const WindowContext;
    // SAFETY: The caller upholds the documented validity, aliasing, thread, and
    // no-reentrant-destruction requirements for the synchronous action.
    unsafe { ptr.as_ref() }.map(action)
}

unsafe fn primary_work_area() -> Result<RECT> {
    let monitor = MonitorFromWindow(HWND::default(), MONITOR_DEFAULTTOPRIMARY);
    let mut info = MONITORINFO {
        cbSize: std::mem::size_of::<MONITORINFO>() as u32,
        ..Default::default()
    };
    GetMonitorInfoW(monitor, &mut info).ok()?;
    Ok(info.rcWork)
}

unsafe fn playback_position(profile: PlaybackProfile, width: i32, height: i32) -> (i32, i32) {
    let work = primary_work_area().unwrap_or(RECT {
        left: 0,
        top: 0,
        right: GetSystemMetrics(SM_CXSCREEN),
        bottom: GetSystemMetrics(SM_CYSCREEN),
    });
    let scale = GetDpiForSystem() as f32 / 96.0;
    match profile {
        PlaybackProfile::LiveMonitor => {
            let margin = (24.0 * scale).round() as i32;
            (work.right - width - margin, work.bottom - height - margin)
        }
        PlaybackProfile::FriendViewer { cascade } => {
            let offset = (cascade.min(5) as f32 * 32.0 * scale).round() as i32;
            let max_x = (work.right - width).max(work.left);
            let max_y = (work.bottom - height).max(work.top);
            (
                ((work.left + work.right - width) / 2 + offset).clamp(work.left, max_x),
                ((work.top + work.bottom - height) / 2 + offset).clamp(work.top, max_y),
            )
        }
    }
}

unsafe fn create_window(
    title: &[u16],
    envelope: (i32, i32),
    profile: PlaybackProfile,
    overlay: crate::overlay::SharedOverlay,
) -> Result<HWND> {
    let instance = GetModuleHandleW(None)?;
    let class_name = w!("orange_viewer");

    let class = WNDCLASSW {
        style: CS_HREDRAW | CS_VREDRAW,
        lpfnWndProc: Some(wnd_proc),
        hInstance: instance.into(),
        lpszClassName: class_name,
        hIcon: LoadIconW(
            Some(instance.into()),
            PCWSTR(std::ptr::with_exposed_provenance(1)),
        )
        .unwrap_or_default(),
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
    let work = primary_work_area()?;
    let width = (envelope.0 as f32 * scale)
        .round()
        .min((work.right - work.left) as f32) as i32;
    let height = (envelope.1 as f32 * scale)
        .round()
        .min((work.bottom - work.top) as f32) as i32;
    let envelope = (width, height);
    let (x, y) = playback_position(profile, width, height);

    let hwnd = CreateWindowExW(
        if profile.always_on_top() {
            WS_EX_TOPMOST
        } else {
            WINDOW_EX_STYLE::default()
        },
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

    // Ownership transfers to GWLP_USERDATA and is reclaimed in WM_DESTROY.
    let ctx = Box::into_raw(Box::new(WindowContext {
        overlay,
        revealed: std::cell::Cell::new(false),
        profile,
        envelope: std::cell::Cell::new(envelope),
        source: std::cell::Cell::new((0, 0)),
        size_move_start: std::cell::Cell::new((0, 0)),
        restore: std::cell::Cell::new(None),
    }));
    SetLastError(ERROR_SUCCESS);
    let previous = SetWindowLongPtrW(hwnd, GWLP_USERDATA, ctx as isize);
    if previous == 0 {
        let error = GetLastError();
        if error != ERROR_SUCCESS {
            // SAFETY: Installation failed, so the fresh allocation was never
            // transferred to the HWND and remains exclusively owned here.
            drop(Box::from_raw(ctx));
            let _ = DestroyWindow(hwnd);
            bail!("failed to install window context (Win32 error {})", error.0);
        }
    } else {
        eprintln!("[window] replaced unexpected existing window context");
        // SAFETY: A non-null value in our private GWLP_USERDATA slot is an
        // installed Box<WindowContext>. The successful replacement removed it
        // from the HWND, and creation has no active context borrow.
        drop(Box::from_raw(previous as *mut WindowContext));
    }

    sync_dpi(hwnd);
    sync_client_size(hwnd);

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
    let Some(overlay) = with_context(hwnd, |ctx| ctx.overlay.clone()) else {
        return true;
    };
    overlay.lock().ok().map(|o| o.visible()).unwrap_or(true)
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
                let activate = with_context(hwnd, |ctx| {
                    (!ctx.revealed.replace(true)).then(|| ctx.profile.activates_on_reveal())
                })
                .flatten();
                if let Some(activate) = activate {
                    let flags = if activate {
                        AW_BLEND | AW_ACTIVATE
                    } else {
                        AW_BLEND
                    };
                    if AnimateWindow(hwnd, REVEAL_MS, flags).is_err()
                        && with_context(hwnd, |_| ()).is_some()
                    {
                        let _ =
                            ShowWindow(hwnd, if activate { SW_SHOW } else { SW_SHOWNOACTIVATE });
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
            WM_ENTERSIZEMOVE => {
                let mut rect = RECT::default();
                if GetWindowRect(hwnd, &mut rect).is_ok() {
                    let _ = with_context(hwnd, |ctx| {
                        ctx.size_move_start
                            .set((rect.right - rect.left, rect.bottom - rect.top));
                    });
                }
                LRESULT(0)
            }
            WM_EXITSIZEMOVE => {
                let mut rect = RECT::default();
                if GetWindowRect(hwnd, &mut rect).is_ok() {
                    let _ = with_context(hwnd, |ctx| {
                        let size = (rect.right - rect.left, rect.bottom - rect.top);
                        if size != ctx.size_move_start.get() {
                            ctx.envelope.set(size);
                        }
                    });
                }
                LRESULT(0)
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

                let over_control = with_context(hwnd, |ctx| ctx.overlay.clone())
                    .and_then(|overlay| {
                        let mut overlay = overlay.lock().ok()?;
                        let video = overlay.video;
                        let (vx, vy) =
                            client_to_video(hwnd, point.x as f32, point.y as f32, video)?;
                        Some(overlay.on_mouse_move(vx, vy))
                    })
                    .unwrap_or(false);

                let hot = with_context(hwnd, |ctx| ctx.overlay.clone())
                    .and_then(|overlay| overlay.lock().ok().map(|o| o.hovered()))
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
                if let Some(overlay) = with_context(hwnd, |ctx| ctx.overlay.clone()) {
                    if let Ok(mut overlay) = overlay.lock() {
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
                if let Some(overlay) = with_context(hwnd, |ctx| ctx.overlay.clone()) {
                    if let Ok(mut overlay) = overlay.lock() {
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
                let released = with_context(hwnd, |ctx| ctx.overlay.clone())
                    .and_then(|overlay| overlay.lock().ok().map(|mut o| o.end_volume_drag()))
                    .unwrap_or(false);
                if released {
                    let _ = ReleaseCapture();
                }
                LRESULT(0)
            }
            WM_CAPTURECHANGED | WM_CANCELMODE => {
                if let Some(overlay) = with_context(hwnd, |ctx| ctx.overlay.clone()) {
                    if let Ok(mut overlay) = overlay.lock() {
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
                if let Some(overlay) = with_context(hwnd, |ctx| ctx.overlay.clone()) {
                    if let Ok(mut overlay) = overlay.lock() {
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
                let fullscreen =
                    with_context(hwnd, |ctx| ctx.restore.get().is_some()).unwrap_or(false);
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
                let _ = with_context(hwnd, |ctx| {
                    if ctx.restore.get().is_none() {
                        ctx.envelope.set((
                            suggested.right - suggested.left,
                            suggested.bottom - suggested.top,
                        ));
                    }
                });
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
            WM_SETCURSOR if (lparam.0 & 0xFFFF) as u32 == HTCLIENT => {
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
                SetLastError(ERROR_SUCCESS);
                let ptr = SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0) as *mut WindowContext;
                if ptr.is_null() {
                    let error = GetLastError();
                    if error != ERROR_SUCCESS {
                        eprintln!(
                            "[window] failed to clear window context (Win32 error {})",
                            error.0
                        );
                    }
                } else {
                    // SAFETY: A non-null return means the clear succeeded and
                    // transferred the installed Box back from GWLP_USERDATA.
                    drop(Box::from_raw(ptr));
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
