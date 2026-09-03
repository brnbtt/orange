//! Enumerating windows that can actually be captured.
//!
//! Two filters matter more than they look. Invisible windows are obvious, but
//! *cloaked* windows are the subtle one: suspended UWP apps stay "visible" to
//! the Win32 API while rendering nothing, so without the DWM cloak check the
//! picker fills up with ghost entries that would capture a black rectangle.

use anyhow::Result;
use windows::core::BOOL;
use windows::Win32::Foundation::{HWND, LPARAM, MAX_PATH, RECT};
use windows::Win32::Graphics::Dwm::{DwmGetWindowAttribute, DWMWA_CLOAKED};
use windows::Win32::Graphics::Gdi::{
    RedrawWindow, RDW_ALLCHILDREN, RDW_FRAME, RDW_INTERNALPAINT, RDW_INVALIDATE, RDW_UPDATENOW,
};
use windows::Win32::System::Threading::{
    OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_FORMAT, PROCESS_QUERY_LIMITED_INFORMATION,
};
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GetAncestor, GetClassNameW, GetClientRect, GetSystemMetrics, GetWindowLongW,
    GetWindowRect, GetWindowTextLengthW, GetWindowTextW, GetWindowThreadProcessId, IsWindowVisible,
    GA_ROOTOWNER, GWL_EXSTYLE, SM_CXSCREEN, SM_CYSCREEN, WS_EX_TOOLWINDOW,
};

/// Window classes that are part of the shell or overlays rather than
/// applications. These are visible, titled and owned by nothing, so no
/// generic rule excludes them - they have to be named.
const EXCLUDED_CLASSES: &[&str] = &[
    "Progman",       // the desktop itself, titled "Program Manager"
    "WorkerW",       // desktop wallpaper host
    "Shell_TrayWnd", // taskbar
    "Shell_SecondaryTrayWnd",
    "Windows.UI.Core.CoreWindow", // system UI surfaces
    "ApplicationFrameWindow_Hidden",
    "CEF-OSC-WIDGET", // NVIDIA GeForce overlay
    "XamlExplorerHostIslandWindow",
];

#[derive(Debug, Clone)]
pub struct CaptureTarget {
    pub hwnd: isize,
    pub pid: u32,
    pub title: String,
    pub process: String,
    pub width: i32,
    pub height: i32,
}

impl CaptureTarget {
    /// Rough heuristic to keep tool windows and tiny popups out of the picker.
    fn is_interesting(&self) -> bool {
        self.width >= 320 && self.height >= 240 && !self.title.is_empty()
    }
}

pub fn capture_dimensions(hwnd: isize) -> Option<(u32, u32)> {
    let (width, height) = if hwnd == 0 {
        unsafe { (GetSystemMetrics(SM_CXSCREEN), GetSystemMetrics(SM_CYSCREEN)) }
    } else {
        let mut rect = RECT::default();
        unsafe { GetClientRect(HWND(hwnd as *mut _), &mut rect).ok()? };
        (rect.right - rect.left, rect.bottom - rect.top)
    };
    Some((width.try_into().ok()?, height.try_into().ok()?))
        .filter(|(width, height)| *width > 0 && *height > 0)
}

unsafe fn window_title(hwnd: HWND) -> String {
    let len = GetWindowTextLengthW(hwnd);
    if len <= 0 {
        return String::new();
    }
    let mut buf = vec![0u16; len as usize + 1];
    let read = GetWindowTextW(hwnd, &mut buf);
    String::from_utf16_lossy(&buf[..read as usize])
}

unsafe fn pid_of(hwnd: HWND) -> u32 {
    let mut pid = 0u32;
    GetWindowThreadProcessId(hwnd, Some(&mut pid));
    pid
}

/// The process that owns a window, so audio capture can be scoped to just it.
///
/// This is what keeps Discord voice, music and notification sounds out of the
/// stream: `wasapi2src` can record a single process tree rather than the whole
/// output device.
pub fn pid_for_hwnd(hwnd: isize) -> Option<u32> {
    let pid = unsafe { pid_of(HWND(hwnd as *mut _)) };
    (pid != 0).then_some(pid)
}

unsafe fn process_name(hwnd: HWND) -> String {
    let pid = pid_of(hwnd);
    if pid == 0 {
        return String::new();
    }
    // Deliberately best-effort: elevated processes will refuse to open, and
    // that should degrade to a blank name rather than dropping the window.
    let Ok(handle) = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) else {
        return String::new();
    };
    let mut buf = vec![0u16; MAX_PATH as usize];
    let mut size = buf.len() as u32;
    let ok = QueryFullProcessImageNameW(
        handle,
        PROCESS_NAME_FORMAT(0),
        windows::core::PWSTR(buf.as_mut_ptr()),
        &mut size,
    )
    .is_ok();
    let _ = windows::Win32::Foundation::CloseHandle(handle);
    if !ok {
        return String::new();
    }
    String::from_utf16_lossy(&buf[..size as usize])
        .rsplit('\\')
        .next()
        .unwrap_or_default()
        .to_string()
}

unsafe fn is_cloaked(hwnd: HWND) -> bool {
    let mut cloaked = 0u32;
    let res = DwmGetWindowAttribute(
        hwnd,
        DWMWA_CLOAKED,
        &mut cloaked as *mut _ as *mut _,
        std::mem::size_of::<u32>() as u32,
    );
    res.is_ok() && cloaked != 0
}

unsafe fn class_name(hwnd: HWND) -> String {
    let mut buf = [0u16; 128];
    let len = GetClassNameW(hwnd, &mut buf);
    String::from_utf16_lossy(&buf[..len.max(0) as usize])
}

/// Roughly the set a user would see in Alt+Tab.
///
/// Three rules do most of the work: tool windows are palettes and overlays,
/// not applications; a window that is not its own root owner is a dialog or
/// popup belonging to something else; and cloaked windows are suspended UWP
/// apps that render nothing.
unsafe fn is_app_window(hwnd: HWND) -> bool {
    if !IsWindowVisible(hwnd).as_bool() || is_cloaked(hwnd) {
        return false;
    }
    if GetAncestor(hwnd, GA_ROOTOWNER) != hwnd {
        return false;
    }
    let ex_style = GetWindowLongW(hwnd, GWL_EXSTYLE) as u32;
    if ex_style & WS_EX_TOOLWINDOW.0 != 0 {
        return false;
    }
    let class = class_name(hwnd);
    !EXCLUDED_CLASSES.iter().any(|c| *c == class)
}

unsafe extern "system" fn enum_proc(hwnd: HWND, lparam: LPARAM) -> BOOL {
    let out = &mut *(lparam.0 as *mut Vec<CaptureTarget>);

    if !is_app_window(hwnd) {
        return BOOL(1);
    }

    let mut rect = RECT::default();
    if GetWindowRect(hwnd, &mut rect).is_err() {
        return BOOL(1);
    }
    let mut client = RECT::default();
    let (width, height) = if GetClientRect(hwnd, &mut client).is_ok()
        && client.right > client.left
        && client.bottom > client.top
    {
        (client.right - client.left, client.bottom - client.top)
    } else {
        (rect.right - rect.left, rect.bottom - rect.top)
    };

    let target = CaptureTarget {
        hwnd: hwnd.0 as isize,
        pid: pid_of(hwnd),
        title: window_title(hwnd),
        process: process_name(hwnd),
        width,
        height,
    };

    if target.is_interesting() {
        out.push(target);
    }
    BOOL(1) // keep enumerating
}

pub fn list_windows() -> Result<Vec<CaptureTarget>> {
    let mut out: Vec<CaptureTarget> = Vec::new();
    unsafe {
        EnumWindows(
            Some(enum_proc),
            LPARAM(&mut out as *mut Vec<CaptureTarget> as isize),
        )?;
    }
    // Biggest first: the game is almost always the largest window on screen.
    out.sort_by_key(|t| std::cmp::Reverse(t.width as i64 * t.height as i64));
    Ok(out)
}

/// Ask an idle window to produce a fresh WGC frame for a late-joining viewer.
pub fn request_redraw(hwnd: isize) {
    if hwnd == 0 {
        return;
    }
    unsafe {
        let _ = RedrawWindow(
            Some(HWND(hwnd as *mut _)),
            None,
            None,
            RDW_INVALIDATE | RDW_INTERNALPAINT | RDW_FRAME | RDW_ALLCHILDREN | RDW_UPDATENOW,
        );
    }
}
