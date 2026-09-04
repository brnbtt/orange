//! Window thumbnails.
//!
//! Captured with `PrintWindow`, which asks a window to draw itself into a
//! device context. `PW_RENDERFULLCONTENT` is what makes it work for modern
//! composited and hardware-accelerated windows; without it many apps come back
//! blank.
//!
//! This is a still, taken on demand. Live previews would mean DWM thumbnails,
//! which are composited by the system into a target region and do not
//! cooperate with GPUI's renderer.

use gpui::RenderImage;
use image::{Frame, RgbaImage};
use std::sync::Arc;
use windows::Win32::Foundation::{HWND, RECT};
use windows::Win32::Graphics::Gdi::{
    BitBlt, CreateCompatibleDC, CreateDIBSection, DeleteDC, DeleteObject, GetDC, ReleaseDC,
    SelectObject, BITMAPINFO, BITMAPINFOHEADER, BI_RGB, DIB_RGB_COLORS, HBITMAP, HGDIOBJ, SRCCOPY,
};
use windows::Win32::Storage::Xps::{PrintWindow, PRINT_WINDOW_FLAGS};
use windows::Win32::UI::WindowsAndMessaging::{
    GetSystemMetrics, GetWindowRect, SM_CXSCREEN, SM_CYSCREEN,
};

/// Render the full window including hardware-accelerated content.
const PW_RENDERFULLCONTENT: PRINT_WINDOW_FLAGS = PRINT_WINDOW_FLAGS(0x0000_0002);

/// Raw thumbnail pixels: width, height, BGRA bytes.
///
/// Deliberately not a `RenderImage`: capture runs on a background thread, and
/// keeping the result as plain data avoids requiring GPUI's image types to be
/// `Send`. The UI thread turns it into an image when it arrives.
pub type Thumbnail = (u32, u32, Vec<u8>);

pub fn screen_size() -> Option<(i32, i32)> {
    let width = unsafe { GetSystemMetrics(SM_CXSCREEN) };
    let height = unsafe { GetSystemMetrics(SM_CYSCREEN) };
    (width > 0 && height > 0).then_some((width, height))
}

/// Capture a window and scale it to fit within `max_w` x `max_h`.
///
/// Returns `None` for windows that refuse to draw, which is common enough
/// (elevated processes, some protected surfaces) that callers must have a
/// fallback rather than treating it as an error.
pub fn thumbnail(hwnd: isize, max_w: u32, max_h: u32) -> Option<Thumbnail> {
    let hwnd = HWND(hwnd as *mut _);

    let (width, height) = unsafe {
        let mut rect = RECT::default();
        GetWindowRect(hwnd, &mut rect).ok()?;
        (
            (rect.right - rect.left).max(0) as u32,
            (rect.bottom - rect.top).max(0) as u32,
        )
    };
    if width == 0 || height == 0 || width > 16_384 || height > 16_384 {
        return None;
    }

    let pixels = unsafe { capture_bgra(hwnd, width, height) }?;

    // The buffer is BGRA, which is the order GPUI's renderer expects, so it is
    // deliberately *not* converted to RGBA here.
    let image = RgbaImage::from_raw(width, height, pixels)?;

    let scale = (max_w as f32 / width as f32)
        .min(max_h as f32 / height as f32)
        .min(1.0);
    let (tw, th) = (
        ((width as f32 * scale) as u32).max(1),
        ((height as f32 * scale) as u32).max(1),
    );
    let scaled = image::imageops::resize(&image, tw, th, image::imageops::FilterType::Triangle);

    Some((tw, th, scaled.into_raw()))
}

/// Capture the whole primary screen, for the "Entire screen" option.
pub fn screen_thumbnail(max_w: u32, max_h: u32) -> Option<Thumbnail> {
    unsafe {
        let width = GetSystemMetrics(SM_CXSCREEN).max(0) as u32;
        let height = GetSystemMetrics(SM_CYSCREEN).max(0) as u32;
        if width == 0 || height == 0 {
            return None;
        }

        let screen_dc = GetDC(None);
        let mem_dc = CreateCompatibleDC(Some(screen_dc));
        let info = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: width as i32,
                biHeight: -(height as i32),
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut bits: *mut std::ffi::c_void = std::ptr::null_mut();
        let bitmap = match CreateDIBSection(Some(mem_dc), &info, DIB_RGB_COLORS, &mut bits, None, 0)
        {
            Ok(bitmap) => bitmap,
            Err(_) => {
                let _ = DeleteDC(mem_dc);
                ReleaseDC(None, screen_dc);
                return None;
            }
        };
        let previous = SelectObject(mem_dc, HGDIOBJ(bitmap.0));

        // BitBlt from the screen rather than PrintWindow: there is no single
        // window to ask, and this composites whatever is actually on display.
        let ok = BitBlt(
            mem_dc,
            0,
            0,
            width as i32,
            height as i32,
            Some(screen_dc),
            0,
            0,
            SRCCOPY,
        )
        .is_ok();

        let result = if ok && !bits.is_null() {
            let len = (width * height * 4) as usize;
            let mut buffer = vec![0u8; len];
            std::ptr::copy_nonoverlapping(bits as *const u8, buffer.as_mut_ptr(), len);
            for chunk in buffer.as_chunks_mut::<4>().0 {
                chunk[3] = 255;
            }
            Some(buffer)
        } else {
            None
        };

        SelectObject(mem_dc, previous);
        let _ = DeleteObject(HGDIOBJ(bitmap.0));
        let _ = DeleteDC(mem_dc);
        ReleaseDC(None, screen_dc);

        let image = RgbaImage::from_raw(width, height, result?)?;
        let scale = (max_w as f32 / width as f32)
            .min(max_h as f32 / height as f32)
            .min(1.0);
        let (tw, th) = (
            ((width as f32 * scale) as u32).max(1),
            ((height as f32 * scale) as u32).max(1),
        );
        let scaled = image::imageops::resize(&image, tw, th, image::imageops::FilterType::Triangle);
        Some((tw, th, scaled.into_raw()))
    }
}

/// Turn captured pixels into something GPUI can draw. Must run on the UI thread.
pub fn to_image(thumb: Thumbnail) -> Option<Arc<RenderImage>> {
    let (w, h, bytes) = thumb;
    let image = RgbaImage::from_raw(w, h, bytes)?;
    Some(Arc::new(RenderImage::new(vec![Frame::new(image)])))
}

/// Draw the window into a top-down 32-bit DIB and return its pixels.
unsafe fn capture_bgra(hwnd: HWND, width: u32, height: u32) -> Option<Vec<u8>> {
    let screen_dc = GetDC(None);
    let mem_dc = CreateCompatibleDC(Some(screen_dc));

    let info = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: width as i32,
            // Negative height requests top-down rows, matching image layout.
            biHeight: -(height as i32),
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            ..Default::default()
        },
        ..Default::default()
    };

    let mut bits: *mut std::ffi::c_void = std::ptr::null_mut();
    let bitmap: HBITMAP =
        match CreateDIBSection(Some(mem_dc), &info, DIB_RGB_COLORS, &mut bits, None, 0) {
            Ok(bitmap) => bitmap,
            Err(_) => {
                let _ = DeleteDC(mem_dc);
                ReleaseDC(None, screen_dc);
                return None;
            }
        };

    let previous = SelectObject(mem_dc, HGDIOBJ(bitmap.0));
    let drawn = PrintWindow(hwnd, mem_dc, PW_RENDERFULLCONTENT).as_bool();

    let result = if drawn && !bits.is_null() {
        let len = (width * height * 4) as usize;
        let mut buffer = vec![0u8; len];
        std::ptr::copy_nonoverlapping(bits as *const u8, buffer.as_mut_ptr(), len);
        // Many windows draw with a zero alpha channel, which would render the
        // thumbnail invisible. Opaque is the only sensible interpretation.
        for chunk in buffer.as_chunks_mut::<4>().0 {
            chunk[3] = 255;
        }
        Some(buffer)
    } else {
        None
    };

    SelectObject(mem_dc, previous);
    let _ = DeleteObject(HGDIOBJ(bitmap.0));
    let _ = DeleteDC(mem_dc);
    ReleaseDC(None, screen_dc);
    result
}
