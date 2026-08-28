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
use windows::Win32::System::Threading::{
    OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_FORMAT,
    PROCESS_QUERY_LIMITED_INFORMATION,
};
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GetWindowRect, GetWindowTextLengthW, GetWindowTextW,
    GetWindowThreadProcessId, IsWindowVisible,
};

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

unsafe extern "system" fn enum_proc(hwnd: HWND, lparam: LPARAM) -> BOOL {
    let out = &mut *(lparam.0 as *mut Vec<CaptureTarget>);

    if !IsWindowVisible(hwnd).as_bool() || is_cloaked(hwnd) {
        return BOOL(1);
    }

    let mut rect = RECT::default();
    if GetWindowRect(hwnd, &mut rect).is_err() {
        return BOOL(1);
    }

    let target = CaptureTarget {
        hwnd: hwnd.0 as isize,
        pid: pid_of(hwnd),
        title: window_title(hwnd),
        process: process_name(hwnd),
        width: rect.right - rect.left,
        height: rect.bottom - rect.top,
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
