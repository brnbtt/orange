use super::{
    fail_fast_native_cleanup, finish_window_startup, CleanupFailure, CleanupResult,
    NativeWindowState, PlaybackProfile, ShutdownEvent, WorkerFinish, ASPECT_MESSAGE,
    REVEAL_MESSAGE,
};
use anyhow::{bail, Result};
use std::io::Write;
use std::sync::{atomic::Ordering, mpsc, Arc, OnceLock};
use std::thread::JoinHandle;
use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{
    GetLastError, SetLastError, COLORREF, ERROR_SUCCESS, HINSTANCE, HWND, LPARAM, LRESULT, POINT,
    RECT, WAIT_FAILED, WAIT_OBJECT_0, WAIT_TIMEOUT, WPARAM,
};
use windows::Win32::Graphics::Dwm::{
    DwmSetWindowAttribute, DWMWA_BORDER_COLOR, DWMWA_COLOR_NONE, DWMWA_WINDOW_CORNER_PREFERENCE,
    DWMWCP_DONOTROUND, DWMWCP_ROUND,
};
use windows::Win32::Graphics::Gdi::{
    CreateSolidBrush, DeleteObject, GetMonitorInfoW, MonitorFromWindow, ScreenToClient, HGDIOBJ,
    MONITORINFO, MONITOR_DEFAULTTONEAREST, MONITOR_DEFAULTTOPRIMARY,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::HiDpi::{GetDpiForSystem, GetDpiForWindow, GetSystemMetricsForDpi};
use windows::Win32::UI::Input::KeyboardAndMouse::{ReleaseCapture, SetCapture, VK_ESCAPE, VK_F11};
use windows::Win32::UI::WindowsAndMessaging::*;

/// Cosmetic only: the frame behind the video, visible for an instant before
/// the first frame arrives and in the letterbox bars.
const BACKGROUND: COLORREF = COLORREF(0x000b0b0b); // BGR
#[derive(Clone, Copy)]
enum ClassError {
    BrushAllocation,
    Windows(u32),
}
type ClassResult = std::result::Result<(), ClassError>;
static VIEWER_CLASS_RESULT: OnceLock<ClassResult> = OnceLock::new();

/// Drives cursor hiding. Windows only asks about the cursor when the mouse
/// moves, and the point is to hide it when the mouse has stopped.
const CURSOR_TIMER: usize = 1;
const REVEAL_MS: u32 = 180;
const WINDOW_WAIT_MS: u32 = 100;
const MESSAGE_BATCH_LIMIT: usize = 64;
const DESTROY_ATTEMPTS: usize = 3;
const WAIT_FOR_MESSAGES: u32 = WAIT_OBJECT_0.0 + 1;

struct CompletionAck {
    completed: Option<mpsc::SyncSender<CleanupResult>>,
    result: CleanupResult,
}

impl CompletionAck {
    fn new(completed: mpsc::SyncSender<CleanupResult>) -> Self {
        Self {
            completed: Some(completed),
            result: Err(CleanupFailure::WorkerPanicked),
        }
    }

    fn finish(&mut self, result: CleanupResult) {
        self.result = result;
    }
}

impl Drop for CompletionAck {
    fn drop(&mut self) {
        if let Some(completed) = self.completed.take() {
            let _ = completed.send(self.result);
        }
    }
}

pub(super) struct SpawnedWindow {
    pub(super) shutdown: Arc<ShutdownEvent>,
    pub(super) completed: mpsc::Receiver<CleanupResult>,
    pub(super) worker: JoinHandle<()>,
}

pub(super) fn spawn_window(
    title: &str,
    envelope: (i32, i32),
    profile: PlaybackProfile,
    overlay: crate::overlay::SharedOverlay,
    native: Arc<NativeWindowState>,
) -> Result<SpawnedWindow> {
    let (tx, rx) = mpsc::sync_channel::<Result<isize>>(1);
    let (completion, completed) = mpsc::sync_channel::<CleanupResult>(1);
    let shutdown = Arc::new(ShutdownEvent::new()?);
    let shutdown_for_worker = shutdown.clone();
    let title: Vec<u16> = title.encode_utf16().chain(std::iter::once(0)).collect();

    let worker = std::thread::Builder::new()
        .name("orange-playback-window".to_string())
        .spawn(move || unsafe {
            let mut completion = CompletionAck::new(completion);
            let cleanup = match create_window(&title, envelope, profile, overlay, native.clone()) {
                Ok(hwnd) => {
                    native.install(hwnd.0 as isize);
                    if tx.send(Ok(hwnd.0 as isize)).is_err() {
                        destroy_window_for_owner(hwnd, &native)
                    } else {
                        run_message_loop(hwnd, &native, &shutdown_for_worker)
                    }
                }
                Err(err) => {
                    native.alive.store(false, Ordering::Release);
                    let _ = tx.send(Err(err));
                    Ok(())
                }
            };
            completion.finish(cleanup);
        })?;

    let mut worker = Some(worker);
    finish_window_startup(rx, &completed, &mut worker)?;
    Ok(SpawnedWindow {
        shutdown,
        completed,
        worker: worker.expect("successful startup retained its window worker"),
    })
}

/// Per-window state reachable from the window procedure.
struct WindowContext {
    overlay: crate::overlay::SharedOverlay,
    native: Arc<NativeWindowState>,
    revealed: std::cell::Cell<bool>,
    profile: PlaybackProfile,
    envelope: std::cell::Cell<(i32, i32)>,
    source: std::cell::Cell<(u32, u32)>,
    size_move_start: std::cell::Cell<(i32, i32)>,
    /// Style and bounds to put back when leaving fullscreen. `Some` means we
    /// are currently fullscreen.
    restore: std::cell::Cell<Option<(WINDOW_STYLE, RECT)>>,
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
    let Some((previous, fullscreen)) = with_context(hwnd, |ctx| {
        (
            ctx.source.replace((width, height)),
            ctx.restore.get().is_some(),
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
    let Some((bounds, profile)) = with_context(hwnd, |ctx| (ctx.envelope.get(), ctx.profile))
    else {
        return;
    };
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

        if with_context(hwnd, |ctx| ctx.restore.set(Some((style, bounds)))).is_none() {
            return;
        }
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

unsafe fn ensure_viewer_class(instance: HINSTANCE) -> Result<()> {
    let result = VIEWER_CLASS_RESULT.get_or_init(|| {
        let cursor = match LoadCursorW(None, IDC_ARROW) {
            Ok(cursor) => cursor,
            Err(error) => return Err(ClassError::Windows(error.code().0 as u32)),
        };
        let brush = CreateSolidBrush(BACKGROUND);
        if brush.0.is_null() {
            return Err(ClassError::BrushAllocation);
        }
        let class = WNDCLASSW {
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(wnd_proc),
            hInstance: instance,
            lpszClassName: w!("orange_viewer"),
            hIcon: LoadIconW(Some(instance), PCWSTR(std::ptr::with_exposed_provenance(1)))
                .unwrap_or_default(),
            hCursor: cursor,
            hbrBackground: brush,
            ..Default::default()
        };
        if RegisterClassW(&class) != 0 {
            return Ok(());
        }

        let error = GetLastError().0;
        let _ = DeleteObject(HGDIOBJ(brush.0));
        // An earlier successful result is the only proof that this class is
        // ours; an already-existing class observed here remains a failure.
        Err(ClassError::Windows(error))
    });
    match result {
        Ok(()) => Ok(()),
        Err(ClassError::BrushAllocation) => {
            bail!("failed to allocate viewer window class brush")
        }
        Err(ClassError::Windows(error)) => {
            bail!("failed to register viewer window class (Win32 error {error})")
        }
    }
}

unsafe fn create_window(
    title: &[u16],
    envelope: (i32, i32),
    profile: PlaybackProfile,
    overlay: crate::overlay::SharedOverlay,
    native: Arc<NativeWindowState>,
) -> Result<HWND> {
    let instance = GetModuleHandleW(None)?;
    let class_name = w!("orange_viewer");
    ensure_viewer_class(instance.into())?;

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
    let native_for_creation_cleanup = native.clone();
    let ctx = Box::into_raw(Box::new(WindowContext {
        overlay,
        native,
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
            if let Err(cleanup) = destroy_window_for_owner(hwnd, &native_for_creation_cleanup) {
                fail_fast_native_cleanup("window creation", WorkerFinish::Joined(Err(cleanup)));
            }
            bail!("failed to install window context (Win32 error {})", error.0);
        }
    } else {
        eprintln!("[window] replaced unexpected existing window context");
        // The previous value is opaque and no longer installed. It may leak,
        // but interpreting or freeing an unknown pointer would be unsound.
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

#[cfg(test)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MessageResult {
    Error,
    Quit,
    Dispatch,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WaitResult {
    Shutdown,
    Messages,
    Timeout,
    Failed,
    Unexpected(u32),
}

fn classify_wait_result(result: u32) -> WaitResult {
    match result {
        result if result == WAIT_OBJECT_0.0 => WaitResult::Shutdown,
        WAIT_FOR_MESSAGES => WaitResult::Messages,
        result if result == WAIT_TIMEOUT.0 => WaitResult::Timeout,
        result if result == WAIT_FAILED.0 => WaitResult::Failed,
        result => WaitResult::Unexpected(result),
    }
}

#[derive(Default)]
struct WindowLoopState {
    playback_dead: bool,
    shutdown_requested: bool,
}

impl WindowLoopState {
    fn observe_message_loop_end(&mut self) {
        self.playback_dead = true;
    }

    fn observe_wait_failure(&mut self) {
        self.playback_dead = true;
    }

    fn observe_shutdown(&mut self) {
        self.shutdown_requested = true;
    }

    fn should_destroy(&self) -> bool {
        self.shutdown_requested
    }

    fn playback_dead(&self) -> bool {
        self.playback_dead
    }
}

fn should_drain_another_message(processed: usize) -> bool {
    processed < MESSAGE_BATCH_LIMIT
}

unsafe fn drain_window_messages() -> bool {
    let mut msg = MSG::default();
    let mut processed = 0;
    while should_drain_another_message(processed)
        && PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool()
    {
        processed += 1;
        if msg.message == WM_QUIT {
            return false;
        }
        let _ = TranslateMessage(&msg);
        DispatchMessageW(&msg);
    }
    true
}

unsafe fn run_message_loop(
    hwnd: HWND,
    native: &NativeWindowState,
    shutdown: &ShutdownEvent,
) -> CleanupResult {
    let mut state = WindowLoopState::default();
    loop {
        if shutdown.is_requested() {
            state.observe_shutdown();
            break;
        }
        let wait = MsgWaitForMultipleObjectsEx(
            Some(&[shutdown.handle]),
            WINDOW_WAIT_MS,
            QS_ALLINPUT,
            MWMO_INPUTAVAILABLE,
        );
        match classify_wait_result(wait.0) {
            WaitResult::Shutdown => {
                state.observe_shutdown();
                break;
            }
            WaitResult::Messages => {
                if !drain_window_messages() {
                    state.observe_message_loop_end();
                    native.alive.store(false, Ordering::Release);
                }
            }
            WaitResult::Timeout => continue,
            WaitResult::Failed => {
                let error = GetLastError().0;
                let _ = writeln!(
                    std::io::stderr().lock(),
                    "[window] MsgWaitForMultipleObjectsEx failed (Win32 error {error})"
                );
                state.observe_wait_failure();
                native.alive.store(false, Ordering::Release);
                shutdown.wait_for_request();
                state.observe_shutdown();
                break;
            }
            WaitResult::Unexpected(result) => {
                let _ = writeln!(
                    std::io::stderr().lock(),
                    "[window] unexpected message wait result {result}"
                );
                state.observe_wait_failure();
                native.alive.store(false, Ordering::Release);
                shutdown.wait_for_request();
                state.observe_shutdown();
                break;
            }
        }
    }
    if state.playback_dead() {
        native.alive.store(false, Ordering::Release);
    }
    if state.should_destroy() {
        destroy_window_for_owner(hwnd, native)
    } else {
        Err(CleanupFailure::WorkerPanicked)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CleanupDecision {
    Complete,
    Retry,
    Abort,
}

fn cleanup_decision(
    window_exists: bool,
    destroy_succeeded: bool,
    attempt: usize,
) -> CleanupDecision {
    if !window_exists || destroy_succeeded {
        CleanupDecision::Complete
    } else if attempt + 1 < DESTROY_ATTEMPTS {
        CleanupDecision::Retry
    } else {
        CleanupDecision::Abort
    }
}

fn complete_destroyed_window(native: &NativeWindowState, hwnd: isize) -> CleanupResult {
    native.alive.store(false, Ordering::Release);
    native.invalidate(hwnd);
    let context_error = native.context_cleanup_error.load(Ordering::Acquire);
    if context_error == ERROR_SUCCESS.0 {
        Ok(())
    } else {
        Err(CleanupFailure::ContextCleanupFailed(context_error))
    }
}

unsafe fn destroy_window_for_owner(hwnd: HWND, native: &NativeWindowState) -> CleanupResult {
    let mut last_error = ERROR_SUCCESS.0;
    for attempt in 0..DESTROY_ATTEMPTS {
        let window_exists = IsWindow(Some(hwnd)).as_bool();
        let destroy_succeeded = if window_exists {
            SetLastError(ERROR_SUCCESS);
            match DestroyWindow(hwnd) {
                Ok(()) => true,
                Err(_) => {
                    last_error = GetLastError().0;
                    false
                }
            }
        } else {
            false
        };
        match cleanup_decision(window_exists, destroy_succeeded, attempt) {
            CleanupDecision::Complete => {
                return complete_destroyed_window(native, hwnd.0 as isize);
            }
            CleanupDecision::Retry => std::thread::yield_now(),
            CleanupDecision::Abort => {
                let _ = writeln!(
                    std::io::stderr().lock(),
                    "[window] failed to destroy playback window after {DESTROY_ATTEMPTS} attempts (Win32 error {last_error})"
                );
                return Err(CleanupFailure::DestroyFailed(last_error));
            }
        }
    }
    Err(CleanupFailure::DestroyFailed(last_error))
}

#[cfg(test)]
fn message_result(result: i32) -> MessageResult {
    if result > 0 {
        MessageResult::Dispatch
    } else if result == 0 {
        MessageResult::Quit
    } else {
        MessageResult::Error
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
                    if !ctx.native.alive.load(Ordering::Acquire) {
                        return None;
                    }
                    (!ctx.revealed.replace(true)).then(|| ctx.profile.activates_on_reveal())
                })
                .flatten();
                if let Some(activate) = activate {
                    let flags = if activate {
                        AW_BLEND | AW_ACTIVATE
                    } else {
                        AW_BLEND
                    };
                    if AnimateWindow(hwnd, REVEAL_MS, flags).is_err() {
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
                        with_context(hwnd, |_| ())?;
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
                            if with_context(hwnd, |_| ()).is_some() {
                                overlay.on_click(vx, vy);
                                close = overlay.close_requested;
                                fullscreen = std::mem::take(&mut overlay.fullscreen_requested);
                                volume_dragging = overlay.volume_dragging();
                            }
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
                                if with_context(hwnd, |_| ()).is_some() {
                                    overlay.drag_volume(vx);
                                }
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
            WM_CLOSE => {
                let _ = with_context(hwnd, |ctx| ctx.native.alive.store(false, Ordering::Release));
                let _ = ShowWindow(hwnd, SW_HIDE);
                LRESULT(0)
            }
            WM_DESTROY => {
                let native = with_context(hwnd, |ctx| ctx.native.clone());
                if let Some(native) = &native {
                    native.alive.store(false, Ordering::Release);
                    native.invalidate(hwnd.0 as isize);
                }
                let _ = KillTimer(Some(hwnd), CURSOR_TIMER);
                SetLastError(ERROR_SUCCESS);
                let ptr = SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0) as *mut WindowContext;
                if ptr.is_null() {
                    let error = GetLastError();
                    if error != ERROR_SUCCESS {
                        if let Some(native) = &native {
                            native
                                .context_cleanup_error
                                .store(error.0, Ordering::Release);
                        }
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

#[cfg(test)]
mod tests {
    use super::{
        aspect_locked_size, classify_wait_result, cleanup_decision, complete_destroyed_window,
        fit_aspect, message_result, should_drain_another_message, CleanupDecision, CompletionAck,
        MessageResult, NativeWindowState, WaitResult, WindowLoopState, MESSAGE_BATCH_LIMIT,
        WAIT_FOR_MESSAGES, WINDOW_WAIT_MS,
    };
    use crate::window::{
        join_after_worker_completion, CleanupFailure, CleanupResult, WorkerFinish,
    };
    use std::sync::atomic::Ordering;
    use std::sync::mpsc;
    use std::time::Duration;
    use windows::Win32::Foundation::{WAIT_FAILED, WAIT_OBJECT_0};
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

    #[test]
    fn message_result_preserves_get_message_tri_state() {
        assert_eq!(message_result(-42), MessageResult::Error);
        assert_eq!(message_result(-1), MessageResult::Error);
        assert_eq!(message_result(0), MessageResult::Quit);
        assert_eq!(message_result(1), MessageResult::Dispatch);
        assert_eq!(message_result(42), MessageResult::Dispatch);
    }

    #[test]
    fn panicking_worker_acknowledges_before_bounded_join() {
        let (completed, completion) = mpsc::sync_channel::<CleanupResult>(1);
        let worker = std::thread::spawn(move || {
            let _completion = CompletionAck::new(completed);
            panic!("simulated worker panic");
        });
        let mut worker = Some(worker);
        let mut cleanup = None;

        assert_eq!(
            join_after_worker_completion(
                &completion,
                &mut cleanup,
                &mut worker,
                std::time::Instant::now() + Duration::from_secs(5),
                "panic test"
            ),
            WorkerFinish::Joined(Err(CleanupFailure::WorkerPanicked))
        );
        assert!(worker.is_none());
    }

    #[test]
    fn quit_and_wait_failure_reserve_hwnd_until_owner_shutdown() {
        let mut state = WindowLoopState::default();

        state.observe_message_loop_end();
        assert!(state.playback_dead());
        assert!(!state.should_destroy());
        state.observe_wait_failure();
        assert!(state.playback_dead());
        assert!(!state.should_destroy());
        state.observe_shutdown();
        assert!(state.should_destroy());
    }

    #[test]
    fn wait_results_distinguish_shutdown_messages_and_failure() {
        assert_eq!(classify_wait_result(WAIT_OBJECT_0.0), WaitResult::Shutdown);
        assert_eq!(
            classify_wait_result(WAIT_FOR_MESSAGES),
            WaitResult::Messages
        );
        assert_eq!(classify_wait_result(WAIT_FAILED.0), WaitResult::Failed);
        assert_eq!(
            classify_wait_result(windows::Win32::Foundation::WAIT_TIMEOUT.0),
            WaitResult::Timeout
        );
        assert_eq!(classify_wait_result(42), WaitResult::Unexpected(42));
        assert_eq!(WINDOW_WAIT_MS, 100);
    }

    #[test]
    fn message_drain_is_bounded_before_shutdown_recheck() {
        assert!(should_drain_another_message(MESSAGE_BATCH_LIMIT - 1));
        assert!(!should_drain_another_message(MESSAGE_BATCH_LIMIT));
    }

    #[test]
    fn cleanup_policy_requires_destroyed_or_absent_window() {
        assert_eq!(cleanup_decision(false, false, 0), CleanupDecision::Complete);
        assert_eq!(cleanup_decision(true, true, 0), CleanupDecision::Complete);
        assert_eq!(cleanup_decision(true, false, 0), CleanupDecision::Retry);
        assert_eq!(cleanup_decision(true, false, 2), CleanupDecision::Abort);
    }

    #[test]
    fn context_cleanup_failure_is_not_reported_as_native_success() {
        let native = NativeWindowState::new();
        native.install(1234);
        native.context_cleanup_error.store(5, Ordering::Release);

        assert_eq!(
            complete_destroyed_window(&native, 1234),
            Err(CleanupFailure::ContextCleanupFailed(5))
        );
        assert_eq!(native.with_hwnd(|hwnd| hwnd), None);
    }
}
