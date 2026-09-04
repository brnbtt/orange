//! Text for the video overlay.
//!
//! tiny-skia rasterises shapes but has no notion of text, and the overlay
//! needs some: a viewer cannot otherwise be told what they are actually
//! receiving, which is the one thing this project is about.
//!
//! Rather than convert glyph outlines to paths, `ab_glyph` is asked for
//! per-pixel coverage and that is blended straight into the pixmap. It is
//! less code, and the result is identical for the small sizes used here.
//!
//! Segoe UI is loaded from the system rather than embedded. This is a
//! Windows-only product, the font is already on every machine that can run
//! it, and it keeps the binary a megabyte smaller while matching the client.

use ab_glyph::{Font, FontVec, GlyphId, PxScale, ScaleFont};
use std::sync::OnceLock;
use tiny_skia::{Color, Pixmap};

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Weight {
    Regular,
    Semibold,
}

fn load(files: &[&str]) -> Option<FontVec> {
    let root = std::env::var("WINDIR").unwrap_or_else(|_| String::from("C:\\Windows"));
    files.iter().find_map(|name| {
        let data = std::fs::read(format!("{root}\\Fonts\\{name}")).ok()?;
        FontVec::try_from_vec(data).ok()
    })
}

fn font(weight: Weight) -> Option<&'static FontVec> {
    static REGULAR: OnceLock<Option<FontVec>> = OnceLock::new();
    static SEMIBOLD: OnceLock<Option<FontVec>> = OnceLock::new();
    match weight {
        Weight::Regular => REGULAR
            .get_or_init(|| load(&["segoeui.ttf", "arial.ttf", "tahoma.ttf"]))
            .as_ref(),
        Weight::Semibold => SEMIBOLD
            .get_or_init(|| load(&["segoeuisb.ttf", "segoeuib.ttf", "arialbd.ttf"]))
            .as_ref(),
    }
}

/// Whether any font was found. The status cluster collapses to just its
/// indicator when there is none, rather than leaving a gap where text would
/// have been.
pub fn available() -> bool {
    font(Weight::Regular).is_some()
}

/// Width of `text` if drawn at `size`, in the same units as `size`.
pub fn width(text: &str, size: f32, weight: Weight) -> f32 {
    let Some(font) = font(weight) else {
        return 0.0;
    };
    let scaled = font.as_scaled(PxScale::from(size));
    let mut total = 0.0;
    let mut previous: Option<GlyphId> = None;
    for ch in text.chars() {
        let id = scaled.glyph_id(ch);
        if let Some(p) = previous {
            total += scaled.kern(p, id);
        }
        total += scaled.h_advance(id);
        previous = Some(id);
    }
    total
}

/// Distance from the top of a line to the visual centre of short labels.
///
/// Cap height rather than the full ascent: centring on the ascent leaves
/// text sitting noticeably low, because most of it is empty space above the
/// capitals reserved for accents.
pub fn cap_height(size: f32) -> f32 {
    size * 0.71
}

/// Draw `text` with its baseline at `y`, left edge at `x`.
pub fn draw(
    pixmap: &mut Pixmap,
    x: f32,
    y: f32,
    text: &str,
    size: f32,
    weight: Weight,
    color: Color,
) {
    let Some(font) = font(weight) else {
        return;
    };
    let scaled = font.as_scaled(PxScale::from(size));
    let mut caret = x;
    let mut previous: Option<GlyphId> = None;

    for ch in text.chars() {
        let id = scaled.glyph_id(ch);
        if let Some(p) = previous {
            caret += scaled.kern(p, id);
        }
        let glyph = id.with_scale_and_position(size, ab_glyph::point(caret, y));
        if let Some(outlined) = font.outline_glyph(glyph) {
            let bounds = outlined.px_bounds();
            outlined.draw(|gx, gy, coverage| {
                blend(
                    pixmap,
                    bounds.min.x as i32 + gx as i32,
                    bounds.min.y as i32 + gy as i32,
                    color,
                    coverage,
                );
            });
        }
        caret += scaled.h_advance(id);
        previous = Some(id);
    }
}

/// Source-over blend of a single pixel.
///
/// tiny-skia's buffer is premultiplied, so the source has to be premultiplied
/// by the same alpha before it is mixed in.
fn blend(pixmap: &mut Pixmap, x: i32, y: i32, color: Color, coverage: f32) {
    let width = pixmap.width() as i32;
    let height = pixmap.height() as i32;
    if x < 0 || y < 0 || x >= width || y >= height {
        return;
    }
    let alpha = color.alpha() * coverage.clamp(0.0, 1.0);
    if alpha <= 0.0 {
        return;
    }

    let index = (y as usize * width as usize + x as usize) * 4;
    let data = pixmap.data_mut();
    let inverse = 1.0 - alpha;
    for (offset, channel) in [color.red(), color.green(), color.blue(), 1.0]
        .into_iter()
        .enumerate()
    {
        let source = channel * alpha;
        let destination = data[index + offset] as f32 / 255.0;
        data[index + offset] = ((source + destination * inverse) * 255.0).round() as u8;
    }
}
