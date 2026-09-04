use gstreamer as gst;
use gstreamer_video as gst_video;
use tiny_skia::{Color, FillRule, Paint, PathBuilder, Pixmap, PixmapPaint, Stroke, Transform};

use super::{Control, Hit, OverlayState};
use crate::text::{self, Weight};

// Design tokens, in logical pixels: what they measure on screen at 100%
// display scaling, whatever the stream resolution.
const MARGIN: f32 = 18.0;
const BUTTON: f32 = 40.0;
const ICON: f32 = 19.0;
const CHIP: f32 = 34.0;
const PAD: f32 = 13.0;
const TRACK: f32 = 110.0;
const LABEL: f32 = 13.0;
const CONTROL_RADIUS: f32 = 12.0;
/// The live dot. Small enough to read as an indicator rather than a button.
const DOT: f32 = 3.5;
/// Volume track height and knob radius. The knob is deliberately much bigger
/// than the track is tall: it is the part you aim at.
const TRACK_HEIGHT: f32 = 6.0;
const KNOB: f32 = 9.0;

// The app's palette, matching the client. Kept as floats because tiny-skia wants
// them that way; the hex on the right is the value on the identity board.
//
// The panel is darker than the client's card surface and very nearly opaque. A
// control panel has to read the same over a dark game and a white browser, and
// at 0.94 the same fill looked like two different greys depending on the frame
// behind it.
const SURFACE: (f32, f32, f32) = (0.078, 0.078, 0.086); // #141416
const BORDER: (f32, f32, f32) = (0.165, 0.165, 0.180); // #2a2a2e
const CREAM: (f32, f32, f32) = (0.902, 0.878, 0.820); // #e6e0d1
const ORANGE: (f32, f32, f32) = (1.0, 0.353, 0.122); // #ff5a1f
const DANGER: (f32, f32, f32) = (0.937, 0.267, 0.267); // #ef4444

const ICON_SPEAKER_NONE: &str = "M155.51,24.81a8,8,0,0,0-8.42.88L77.25,80H32A16,16,0,0,0,16,96v64a16,16,0,0,0,16,16H77.25l69.84,54.31A8,8,0,0,0,160,224V32A8,8,0,0,0,155.51,24.81ZM32,96H72v64H32ZM144,207.64,88,164.09V91.91l56-43.55Z";
const ICON_SPEAKER_LOW: &str = "M155.51,24.81a8,8,0,0,0-8.42.88L77.25,80H32A16,16,0,0,0,16,96v64a16,16,0,0,0,16,16H77.25l69.84,54.31A8,8,0,0,0,160,224V32A8,8,0,0,0,155.51,24.81ZM32,96H72v64H32ZM144,207.64,88,164.09V91.91l56-43.55ZM208,128a39.93,39.93,0,0,1-10,26.46,8,8,0,0,1-12-10.58,24,24,0,0,0,0-31.72,8,8,0,1,1,12-10.58A40,40,0,0,1,208,128Z";
const ICON_SPEAKER_HIGH: &str = "M155.51,24.81a8,8,0,0,0-8.42.88L77.25,80H32A16,16,0,0,0,16,96v64a16,16,0,0,0,16,16H77.25l69.84,54.31A8,8,0,0,0,160,224V32A8,8,0,0,0,155.51,24.81ZM32,96H72v64H32ZM144,207.64,88,164.09V91.91l56-43.55Zm54-106.08a40,40,0,0,1,0,52.88,8,8,0,0,1-12-10.58,24,24,0,0,0,0-31.72,8,8,0,0,1,12-10.58ZM248,128a79.9,79.9,0,0,1-20.37,53.34,8,8,0,0,1-11.92-10.67,64,64,0,0,0,0-85.33,8,8,0,1,1,11.92-10.67A79.83,79.83,0,0,1,248,128Z";
const ICON_SPEAKER_SLASH: &str = "M53.92,34.62A8,8,0,1,0,42.08,45.38L73.55,80H32A16,16,0,0,0,16,96v64a16,16,0,0,0,16,16H77.25l69.84,54.31A8,8,0,0,0,160,224V175.09l42.08,46.29a8,8,0,1,0,11.84-10.76ZM32,96H72v64H32ZM144,207.64,88,164.09V95.89l56,61.6Zm42-63.77a24,24,0,0,0,0-31.72,8,8,0,1,1,12-10.57,40,40,0,0,1,0,52.88,8,8,0,0,1-12-10.59Zm-80.16-76a8,8,0,0,1,1.4-11.23l39.85-31A8,8,0,0,1,160,32v74.83a8,8,0,0,1-16,0V48.36l-26.94,21A8,8,0,0,1,105.84,67.91ZM248,128a79.9,79.9,0,0,1-20.37,53.34,8,8,0,0,1-11.92-10.67,64,64,0,0,0,0-85.33,8,8,0,1,1,11.92-10.67A79.83,79.83,0,0,1,248,128Z";
const ICON_X: &str = "M205.66,194.34a8,8,0,0,1-11.32,11.32L128,139.31,61.66,205.66a8,8,0,0,1-11.32-11.32L116.69,128,50.34,61.66A8,8,0,0,1,61.66,50.34L128,116.69l66.34-66.35a8,8,0,0,1,11.32,11.32L139.31,128Z";
const ICON_ARROWS_OUT: &str = "M216,48V96a8,8,0,0,1-16,0V67.31l-42.34,42.35a8,8,0,0,1-11.32-11.32L188.69,56H160a8,8,0,0,1,0-16h48A8,8,0,0,1,216,48ZM98.34,146.34,56,188.69V160a8,8,0,0,0-16,0v48a8,8,0,0,0,8,8H96a8,8,0,0,0,0-16H67.31l42.35-42.34a8,8,0,0,0-11.32-11.32ZM208,152a8,8,0,0,0-8,8v28.69l-42.34-42.35a8,8,0,0,0-11.32,11.32L188.69,200H160a8,8,0,0,0,0,16h48a8,8,0,0,0,8-8V160A8,8,0,0,0,208,152ZM67.31,56H96a8,8,0,0,0,0-16H48a8,8,0,0,0-8,8V96a8,8,0,0,0,16,0V67.31l42.34,42.35a8,8,0,0,0,11.32-11.32Z";
const ICON_ARROWS_IN: &str = "M144,104V64a8,8,0,0,1,16,0V84.69l42.34-42.35a8,8,0,0,1,11.32,11.32L171.31,96H192a8,8,0,0,1,0,16H152A8,8,0,0,1,144,104Zm-40,40H64a8,8,0,0,0,0,16H84.69L42.34,202.34a8,8,0,0,0,11.32,11.32L96,171.31V192a8,8,0,0,0,16,0V152A8,8,0,0,0,104,144Zm67.31,16H192a8,8,0,0,0,0-16H152a8,8,0,0,0-8,8v40a8,8,0,0,0,16,0V171.31l42.34,42.35a8,8,0,0,0,11.32-11.32ZM104,56a8,8,0,0,0-8,8V84.69L53.66,42.34A8,8,0,0,0,42.34,53.66L84.69,96H64a8,8,0,0,0,0,16h40a8,8,0,0,0,8-8V64A8,8,0,0,0,104,56Z";

// --- painting ---------------------------------------------------------------

fn rgba(c: (f32, f32, f32), a: f32) -> Color {
    Color::from_rgba(c.0, c.1, c.2, a.clamp(0.0, 1.0)).unwrap_or(Color::TRANSPARENT)
}

fn rounded_rect(x: f32, y: f32, w: f32, h: f32, r: f32) -> Option<tiny_skia::Path> {
    let r = r.min(w / 2.0).min(h / 2.0);
    let mut pb = PathBuilder::new();
    pb.move_to(x + r, y);
    pb.line_to(x + w - r, y);
    pb.quad_to(x + w, y, x + w, y + r);
    pb.line_to(x + w, y + h - r);
    pb.quad_to(x + w, y + h, x + w - r, y + h);
    pb.line_to(x + r, y + h);
    pb.quad_to(x, y + h, x, y + h - r);
    pb.line_to(x, y + r);
    pb.quad_to(x, y, x + r, y);
    pb.close();
    pb.finish()
}

fn fill(pixmap: &mut Pixmap, path: &tiny_skia::Path, color: Color) {
    let mut paint = Paint::default();
    paint.set_color(color);
    paint.anti_alias = true;
    pixmap.fill_path(path, &paint, FillRule::Winding, Transform::identity(), None);
}

fn fill_round(pixmap: &mut Pixmap, x: f32, y: f32, w: f32, h: f32, r: f32, color: Color) {
    if let Some(path) = rounded_rect(x, y, w, h, r) {
        fill(pixmap, &path, color);
    }
}

fn stroke(pixmap: &mut Pixmap, path: &tiny_skia::Path, color: Color, width: f32) {
    let mut paint = Paint::default();
    paint.set_color(color);
    paint.anti_alias = true;
    let stroke = Stroke {
        width,
        line_cap: tiny_skia::LineCap::Round,
        line_join: tiny_skia::LineJoin::Round,
        ..Default::default()
    };
    pixmap.stroke_path(path, &paint, &stroke, Transform::identity(), None);
}

fn circle(pixmap: &mut Pixmap, cx: f32, cy: f32, r: f32, color: Color) {
    let mut pb = PathBuilder::new();
    pb.push_circle(cx, cy, r);
    if let Some(path) = pb.finish() {
        fill(pixmap, &path, color);
    }
}

fn status_text_origin(height: f32, gap: f32, scale: f32) -> f32 {
    (height + gap) * scale
}

fn audio_track_width(logical_width: f32) -> f32 {
    (logical_width - (MARGIN * 2.0 + BUTTON + 12.0 + PAD)).clamp(0.0, TRACK)
}

fn audio_gap_width(open: bool, gap: f32, track: f32, logical_width: f32) -> f32 {
    if open {
        gap + track + PAD
    } else {
        (logical_width - MARGIN * 2.0 - BUTTON * 2.0).clamp(0.0, gap + 24.0)
    }
}

fn status_expanded(hovered: bool) -> bool {
    hovered
}

fn status_max_width(logical_width: f32) -> f32 {
    (logical_width - MARGIN * 2.0 - BUTTON - 8.0).max(0.0)
}

/// The live dot's radius inside a status chip.
///
/// Proportional to the chip rather than fixed, because the two chips are
/// different heights - the host's live monitor is shorter than a viewer's -
/// and one radius looked heavy in one and lost in the other.
fn dot_radius(chip_height: f32) -> f32 {
    (chip_height * 0.12).clamp(3.0, DOT + 1.5)
}

/// The panel every cluster sits on.
///
/// Dark enough that white sits on it cleanly over white video - the scrim has
/// to survive the worst case, not the average one - but small enough that
/// being nearly opaque hides almost nothing.
fn panel(pixmap: &mut Pixmap, x: f32, y: f32, w: f32, h: f32, r: f32, alpha: f32) {
    fill_round(pixmap, x, y, w, h, r, rgba(SURFACE, 0.97 * alpha));
    if let Some(path) = rounded_rect(x + 0.5, y + 0.5, w - 1.0, h - 1.0, r) {
        stroke(pixmap, &path, rgba(BORDER, 0.92 * alpha), 1.0);
    }
}

// --- icons ------------------------------------------------------------------

fn draw_svg_icon(pixmap: &mut Pixmap, x: f32, y: f32, s: f32, path: &str, color: Color) {
    let dimension = s.ceil().max(1.0) as u32;
    let to_byte = |channel: f32| (channel * 255.0).round().clamp(0.0, 255.0) as u8;
    let svg = format!(
        r##"<svg xmlns="http://www.w3.org/2000/svg" width="{dimension}" height="{dimension}" viewBox="0 0 256 256" fill="#{:02x}{:02x}{:02x}" fill-opacity="{}"><path d="{path}"/></svg>"##,
        to_byte(color.red()),
        to_byte(color.green()),
        to_byte(color.blue()),
        color.alpha()
    );
    let Ok(tree) = resvg::usvg::Tree::from_str(&svg, &resvg::usvg::Options::default()) else {
        return;
    };
    let Some(mut icon) = Pixmap::new(dimension, dimension) else {
        return;
    };
    resvg::render(&tree, Transform::identity(), &mut icon.as_mut());
    pixmap.draw_pixmap(
        x.round() as i32,
        y.round() as i32,
        icon.as_ref(),
        &PixmapPaint::default(),
        Transform::identity(),
        None,
    );
}

/// The speaker glyph that matches what you would actually hear.
///
/// Four states, not two. A single "on" icon means the only way to tell 10%
/// from 100% is to read the slider, which is exactly the thing the icon is
/// there to save you from - and at 20% the difference between "quiet" and
/// "muted" is the one distinction worth drawing.
fn speaker_icon(muted: bool, level: f32) -> &'static str {
    if muted || level <= 0.0 {
        // Muted and turned-to-zero are different acts, so they get different
        // glyphs: the slash is something you did, the silent cone is where
        // the slider is.
        return if muted {
            ICON_SPEAKER_SLASH
        } else {
            ICON_SPEAKER_NONE
        };
    }
    if level < 0.5 {
        ICON_SPEAKER_LOW
    } else {
        ICON_SPEAKER_HIGH
    }
}

fn speaker(pixmap: &mut Pixmap, x: f32, y: f32, s: f32, muted: bool, level: f32, color: Color) {
    draw_svg_icon(pixmap, x, y, s, speaker_icon(muted, level), color);
}

fn cross(pixmap: &mut Pixmap, x: f32, y: f32, s: f32, color: Color) {
    draw_svg_icon(pixmap, x, y, s, ICON_X, color);
}

/// The live dot.
///
/// A filled circle and nothing else. This was briefly a crescent inside
/// viewfinder brackets, which at twenty pixels read as a small orange smudge
/// competing with the one word the chip exists to say.
fn live_mark(pixmap: &mut Pixmap, cx: f32, cy: f32, radius: f32, color: Color) {
    circle(pixmap, cx, cy, radius, color);
}

fn expand(pixmap: &mut Pixmap, x: f32, y: f32, s: f32, exiting: bool, color: Color) {
    draw_svg_icon(
        pixmap,
        x,
        y,
        s,
        if exiting {
            ICON_ARROWS_IN
        } else {
            ICON_ARROWS_OUT
        },
        color,
    );
}

// --- layout -----------------------------------------------------------------

/// A rasterised cluster and where it belongs, in video coordinates.
struct Panel {
    pixmap: Pixmap,
    x: f32,
    y: f32,
    render_w: f32,
    render_h: f32,
}

/// Build one cluster: allocate a pixmap of the right size, let `paint` fill
/// it, and record where it goes.
fn cluster(
    x: f32,
    y: f32,
    render_w: f32,
    render_h: f32,
    raster_w: f32,
    raster_h: f32,
    paint: impl FnOnce(&mut Pixmap),
) -> Option<Panel> {
    let mut pixmap = Pixmap::new(
        raster_w.ceil().max(1.0) as u32,
        raster_h.ceil().max(1.0) as u32,
    )?;
    paint(&mut pixmap);
    Some(Panel {
        pixmap,
        x,
        y,
        render_w,
        render_h,
    })
}

/// Rasterise the controls and wrap them as an overlay composition.
///
/// Hidden controls become one transparent pixel. The element's `draw` signal
/// requires a composition object even when there is nothing visible.
pub(super) fn render(state: &mut OverlayState) -> Option<gst_video::VideoOverlayComposition> {
    let (vw, vh) = state.video;
    if vw == 0 || vh == 0 {
        state.hits.clear();
        return transparent_composition();
    }

    let signature = state.signature();
    if let Some((cached, composition)) = &state.cache {
        if *cached == signature {
            return Some(composition.clone());
        }
    }

    // The element's `draw` signal requires a composition return value. `None`
    // aborts inside GLib once the controls time out, so hidden means one fully
    // transparent pixel rather than no object at all.
    let persistent_live = state.profile.persistent_live_status();
    if !state.visible() && !persistent_live {
        let composition = transparent_composition()?;
        state.cache = Some((signature, composition.clone()));
        return Some(composition);
    }

    let alpha = state.opacity();
    let status_alpha = if persistent_live { 1.0 } else { alpha };
    // Placement is in video coordinates; painting is at the output's physical
    // DPI. If both use video scale, tiny-skia's antialiasing is filtered again
    // when the sink fits the stream to the window, which softens every icon.
    let render_scale = state.scale();
    let raster_scale = state.dpi.max(1.0);
    let (fw, fh) = (vw as f32, vh as f32);
    let logical_width = fw / render_scale;

    let ink = rgba(CREAM, alpha);
    let status_ink = rgba(CREAM, status_alpha);
    let mut hits: Vec<Hit> = Vec::new();
    let mut panels: Vec<Panel> = Vec::new();

    let hot_alpha = |control: Control, base: f32| {
        if state.hot == Some(control) {
            1.0
        } else {
            base
        }
    };

    // --- status, top-left ---------------------------------------------------
    // A compact stream mark at rest; hovering expands into real receive data.
    {
        let expanded = status_expanded(state.hot == Some(Control::Stats));
        let pulse = state.live_pulse();
        let received = state.quality_label();
        let quality = if persistent_live {
            String::from("LIVE")
        } else {
            received.clone()
        };
        let mut detail = if expanded {
            let detail = state.detail_label();
            if persistent_live {
                Some(match detail {
                    Some(detail) => format!("{received}  \u{00b7}  {detail}"),
                    None => received.clone(),
                })
            } else {
                detail
            }
        } else {
            None
        };
        // Logical dimensions first; each is independently converted for the
        // destination rectangle and for the source pixmap.
        let h = if persistent_live { 28.0 } else { CHIP };
        let leading = h;
        let has_text = text::available();
        let show_label = (expanded || persistent_live) && has_text;
        let label_gap = if persistent_live { 4.0 } else { 8.0 };
        let end_pad = if persistent_live { 10.0 } else { PAD };
        let max_w = status_max_width(logical_width).max(h);
        let label_size = LABEL * raster_scale;
        let measure = |detail: Option<&String>| {
            let mut width = text::width(&quality, label_size, Weight::Semibold) / raster_scale;
            if let Some(detail) = detail {
                width += text::width(
                    &format!("  \u{00b7}  {detail}"),
                    label_size,
                    Weight::Regular,
                ) / raster_scale;
            }
            width
        };
        let mut text_w = measure(detail.as_ref());
        if expanded && persistent_live && leading + label_gap + text_w + end_pad > max_w {
            detail = Some(received);
            text_w = measure(detail.as_ref());
        }
        if expanded && leading + label_gap + text_w + end_pad > max_w {
            detail = None;
            text_w = measure(None);
        }
        let w = if show_label {
            (leading + label_gap + text_w + end_pad).min(max_w)
        } else {
            h
        };
        // Centred on the close button's axis rather than sharing its top
        // edge: the chip is shorter, and aligning tops leaves it looking
        // like it slipped.
        let x = MARGIN * render_scale;
        let y = (MARGIN + (BUTTON - h) / 2.0) * render_scale;

        if let Some(p) = cluster(
            x,
            y,
            w * render_scale,
            h * render_scale,
            w * raster_scale,
            h * raster_scale,
            |pixmap| {
                let (raster_w, raster_h) = (w * raster_scale, h * raster_scale);
                panel(
                    pixmap,
                    0.0,
                    0.0,
                    raster_w,
                    raster_h,
                    if persistent_live {
                        10.0
                    } else {
                        CONTROL_RADIUS
                    } * raster_scale,
                    status_alpha,
                );
                // Centred in the leading box, so the label always clears it
                // whatever height the chip is.
                let lead = leading * raster_scale;
                live_mark(
                    pixmap,
                    lead / 2.0,
                    raster_h / 2.0,
                    dot_radius(h) * raster_scale,
                    rgba(ORANGE, status_alpha * pulse),
                );
                if !show_label {
                    return;
                }
                let baseline = raster_h / 2.0 + text::cap_height(label_size) / 2.0;
                let mut caret = status_text_origin(leading, label_gap, raster_scale);
                text::draw(
                    pixmap,
                    caret,
                    baseline,
                    &quality,
                    label_size,
                    Weight::Semibold,
                    if persistent_live {
                        rgba(ORANGE, status_alpha)
                    } else {
                        status_ink
                    },
                );
                if let Some(detail) = &detail {
                    caret += text::width(&quality, label_size, Weight::Semibold);
                    let joined = format!("  \u{00b7}  {detail}");
                    text::draw(
                        pixmap,
                        caret,
                        baseline,
                        &joined,
                        label_size,
                        Weight::Regular,
                        rgba(CREAM, 0.62 * status_alpha),
                    );
                }
            },
        ) {
            panels.push(p);
            hits.push(Hit {
                control: Control::Stats,
                x,
                y,
                w: w * render_scale,
                h: h * render_scale,
            });
        }
    }

    // --- close, top-right ---------------------------------------------------
    // Where a window's close button would be, since this frame has no title
    // bar of its own. Tinted red on hover: it ends the session.
    if alpha > 0.0 {
        let (w, h) = (BUTTON * render_scale, BUTTON * render_scale);
        let (x, y) = (fw - MARGIN * render_scale - w, MARGIN * render_scale);
        let hovered = state.hot == Some(Control::Close);
        if let Some(p) = cluster(
            x,
            y,
            w,
            h,
            BUTTON * raster_scale,
            BUTTON * raster_scale,
            |pixmap| {
                let w = BUTTON * raster_scale;
                let h = w;
                let icon = ICON * raster_scale;
                let control_radius = CONTROL_RADIUS * raster_scale;
                panel(pixmap, 0.0, 0.0, w, h, control_radius, alpha);
                if hovered {
                    fill_round(
                        pixmap,
                        0.0,
                        0.0,
                        w,
                        h,
                        control_radius,
                        rgba(DANGER, 0.22 * alpha),
                    );
                }
                let color = if hovered { rgba(DANGER, alpha) } else { ink };
                cross(pixmap, (w - icon) / 2.0, (h - icon) / 2.0, icon, color);
            },
        ) {
            panels.push(p);
            hits.push(Hit {
                control: Control::Close,
                x,
                y,
                w,
                h,
            });
        }
    }

    // --- audio, bottom-left -------------------------------------------------
    // A speaker on its own until pointed at, then the slider grows out of it
    // to the right. The volume control is the one thing a viewer actually
    // reaches for, so it gets the largest target of the four.
    if alpha > 0.0 {
        let track = audio_track_width(logical_width);
        let open = state.audio_open() && track >= 36.0;
        let gap = 12.0;
        let h = BUTTON;
        // The slider grows out to the right of the speaker, which stays
        // exactly where it was. Recentring the icon in a wider pill would
        // make it jump sideways under the cursor that just opened it.
        let w = if open {
            BUTTON + gap + track + PAD
        } else {
            BUTTON
        };
        let x = MARGIN * render_scale;
        let y = fh - (MARGIN + h) * render_scale;

        let level = if state.muted {
            0.0
        } else {
            state.volume as f32
        };
        let muted = state.muted;

        if let Some(p) = cluster(
            x,
            y,
            w * render_scale,
            h * render_scale,
            w * raster_scale,
            h * raster_scale,
            |pixmap| {
                let w = w * raster_scale;
                let h = h * raster_scale;
                let button = BUTTON * raster_scale;
                let gap = gap * raster_scale;
                let track = track * raster_scale;
                let icon = ICON * raster_scale;
                let control_radius = CONTROL_RADIUS * raster_scale;
                panel(
                    pixmap,
                    0.0,
                    0.0,
                    w,
                    h,
                    if open { h * 0.36 } else { control_radius },
                    alpha,
                );
                speaker(
                    pixmap,
                    (button - icon) / 2.0,
                    (h - icon) / 2.0,
                    icon,
                    muted,
                    level,
                    rgba(CREAM, hot_alpha(Control::Mute, 0.85) * alpha),
                );
                if !open {
                    return;
                }
                let tx = button + gap;
                let th = TRACK_HEIGHT * raster_scale;
                let ty = h / 2.0 - th / 2.0;
                fill_round(
                    pixmap,
                    tx,
                    ty,
                    track,
                    th,
                    th / 2.0,
                    rgba(CREAM, 0.28 * alpha),
                );
                if level > 0.0 {
                    fill_round(
                        pixmap,
                        tx,
                        ty,
                        track * level,
                        th,
                        th / 2.0,
                        rgba(ORANGE, alpha),
                    );
                }
                circle(
                    pixmap,
                    tx + track * level,
                    h / 2.0,
                    KNOB * raster_scale,
                    rgba(CREAM, alpha),
                );
            },
        ) {
            panels.push(p);
            hits.push(Hit {
                control: Control::Mute,
                x,
                y,
                w: BUTTON * render_scale,
                h: h * render_scale,
            });
            if open {
                hits.push(Hit {
                    control: Control::VolumeTrack,
                    x: x + (BUTTON + gap) * render_scale,
                    y,
                    w: track * render_scale,
                    h: h * render_scale,
                });
            }
            let gap_width = audio_gap_width(open, gap, track, logical_width);
            if gap_width > 0.0 {
                hits.push(Hit {
                    control: Control::AudioGap,
                    x: x + BUTTON * render_scale,
                    y,
                    w: gap_width * render_scale,
                    h: h * render_scale,
                });
            }
        }
    }

    // --- view, bottom-right -------------------------------------------------
    // Fullscreen, where every video player puts it.
    let audio_displaces_fullscreen =
        state.audio_open() && logical_width < MARGIN * 2.0 + BUTTON * 2.0 + 12.0 + PAD + TRACK;
    if alpha > 0.0 && !audio_displaces_fullscreen {
        let (w, h) = (BUTTON * render_scale, BUTTON * render_scale);
        let x = fw - (MARGIN + BUTTON) * render_scale;
        let y = fh - (MARGIN + BUTTON) * render_scale;
        let exiting = state.fullscreen;
        let hovered = state.hot == Some(Control::Fullscreen);
        if let Some(p) = cluster(
            x,
            y,
            w,
            h,
            BUTTON * raster_scale,
            BUTTON * raster_scale,
            |pixmap| {
                let w = BUTTON * raster_scale;
                let h = w;
                let icon = ICON * raster_scale;
                let control_radius = CONTROL_RADIUS * raster_scale;
                panel(pixmap, 0.0, 0.0, w, h, control_radius, alpha);
                if hovered {
                    fill_round(
                        pixmap,
                        0.0,
                        0.0,
                        w,
                        h,
                        control_radius,
                        rgba(CREAM, 0.10 * alpha),
                    );
                }
                expand(
                    pixmap,
                    (w - icon) / 2.0,
                    (h - icon) / 2.0,
                    icon,
                    exiting,
                    rgba(CREAM, hot_alpha(Control::Fullscreen, 0.85) * alpha),
                );
            },
        ) {
            panels.push(p);
            hits.push(Hit {
                control: Control::Fullscreen,
                x,
                y,
                w,
                h,
            });
        }
    }

    if !state.visible() && persistent_live {
        hits.extend(
            state
                .hits
                .iter()
                .copied()
                .filter(|hit| hit.control != Control::Stats),
        );
    }
    state.hits = hits;

    let composition = to_composition(panels)?;
    state.cache = Some((signature, composition.clone()));
    Some(composition)
}

pub(super) fn transparent_composition() -> Option<gst_video::VideoOverlayComposition> {
    let pixmap = Pixmap::new(1, 1)?;
    to_composition(vec![Panel {
        pixmap,
        x: 0.0,
        y: 0.0,
        render_w: 1.0,
        render_h: 1.0,
    }])
}

/// Wrap the rasterised clusters as something the sink can composite.
///
/// Two conversions matter. tiny-skia produces premultiplied alpha, which
/// GStreamer wants flagged explicitly or anti-aliased edges come out wrong.
/// And it lays pixels out as RGBA, whereas an overlay composition on a
/// little-endian machine must be BGRA - `GST_VIDEO_OVERLAY_COMPOSITION_FORMAT_RGB`
/// is an alias for it. Getting that wrong makes `new_raw` return NULL, which
/// aborts the process from inside a C callback with no usable message.
fn to_composition(panels: Vec<Panel>) -> Option<gst_video::VideoOverlayComposition> {
    let mut rectangles = Vec::with_capacity(panels.len());

    for panel in panels {
        let (source_w, source_h) = (panel.pixmap.width(), panel.pixmap.height());
        // Round both destination edges rather than the origin and width
        // independently, or fractional scales can shift the far edge a pixel.
        let left = panel.x.round() as i32;
        let top = panel.y.round() as i32;
        let right = (panel.x + panel.render_w).round() as i32;
        let bottom = (panel.y + panel.render_h).round() as i32;
        let render_w = (right - left).max(1) as u32;
        let render_h = (bottom - top).max(1) as u32;
        let mut data = panel.pixmap.take();
        for pixel in data.as_chunks_mut::<4>().0 {
            pixel.swap(0, 2);
        }

        let mut buffer = gst::Buffer::from_mut_slice(data);
        {
            let buffer = buffer.get_mut()?;
            gst_video::VideoMeta::add(
                buffer,
                gst_video::VideoFrameFlags::empty(),
                gst_video::VideoFormat::Bgra,
                source_w,
                source_h,
            )
            .ok()?;
        }

        rectangles.push(gst_video::VideoOverlayRectangle::new_raw(
            &buffer,
            left,
            top,
            render_w,
            render_h,
            gst_video::VideoOverlayFormatFlags::PREMULTIPLIED_ALPHA,
        ));
    }

    gst_video::VideoOverlayComposition::new(rectangles.iter()).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::connection::{ConnectionEvent, ConnectionTracker};
    use crate::overlay::HIDE_AFTER;
    use std::sync::Arc;
    use std::time::Instant;

    #[test]
    fn status_text_origin_scales_logical_height_once() {
        assert_eq!(status_text_origin(30.0, 4.0, 1.5), 51.0);
        assert_eq!(status_text_origin(34.0, 8.0, 2.0), 84.0);
    }

    #[test]
    fn narrow_audio_layout_reserves_room_for_a_useful_slider() {
        assert_eq!(audio_track_width(480.0), 110.0);
        assert_eq!(audio_track_width(152.0), 51.0);
    }

    #[test]
    fn closed_audio_control_has_no_invisible_slider_hit_region() {
        assert_eq!(audio_gap_width(false, 12.0, 110.0, 152.0), 36.0);
        assert_eq!(audio_gap_width(true, 12.0, 51.0, 152.0), 76.0);
    }

    #[test]
    fn faded_controls_keep_their_hit_geometry() {
        gst::init().unwrap();
        let mut state =
            OverlayState::new(crate::window::PlaybackProfile::FriendViewer { cascade: 0 });
        state.video = (1920, 1080);
        state.client = (1280, 720);
        state.pinned = true;
        render(&mut state).unwrap();
        assert!(!state.hits.is_empty());

        state.pinned = false;
        state.shown_at = Instant::now() - HIDE_AFTER * 2;
        render(&mut state).unwrap();

        assert!(!state.hits.is_empty());
    }

    #[test]
    fn live_status_expands_on_hover() {
        assert!(status_expanded(true));
        assert!(!status_expanded(false));
    }

    #[test]
    fn the_dot_scales_with_the_chip_it_sits_in() {
        // Readable in the host's short chip, still an indicator in the
        // viewer's taller one, and never wider than its leading box.
        for height in [28.0_f32, 34.0] {
            let radius = dot_radius(height);
            assert!(radius >= 3.0, "the dot vanishes in a {height}px chip");
            assert!(
                radius * 2.0 < height / 2.0,
                "the dot reads as a button in a {height}px chip"
            );
        }
    }

    #[test]
    fn live_monitor_keeps_all_control_hits_while_faded() {
        gst::init().unwrap();
        let mut state = OverlayState::new(crate::window::PlaybackProfile::LiveMonitor);
        state.video = (1406, 1541);
        state.client = (246, 270);
        state.pinned = true;
        render(&mut state).unwrap();
        assert!(state.hits.iter().any(|hit| hit.control == Control::Close));
        assert!(state.hits.iter().any(|hit| hit.control == Control::Mute));
        assert!(state
            .hits
            .iter()
            .any(|hit| hit.control == Control::Fullscreen));

        state.pinned = false;
        state.shown_at = Instant::now() - HIDE_AFTER * 2;
        render(&mut state).unwrap();

        assert!(state.hits.iter().any(|hit| hit.control == Control::Close));
        assert!(state.hits.iter().any(|hit| hit.control == Control::Mute));
        assert!(state
            .hits
            .iter()
            .any(|hit| hit.control == Control::Fullscreen));
    }

    #[test]
    fn status_width_stops_before_the_close_control() {
        assert_eq!(status_max_width(480.0), 396.0);
        assert_eq!(status_max_width(152.0), 68.0);
    }

    #[test]
    fn canonical_overlay_icons_render_visible_pixels() {
        for path in [
            ICON_SPEAKER_HIGH,
            ICON_SPEAKER_SLASH,
            ICON_X,
            ICON_ARROWS_OUT,
            ICON_ARROWS_IN,
        ] {
            let mut pixmap = Pixmap::new(24, 24).unwrap();
            draw_svg_icon(&mut pixmap, 2.0, 2.0, 20.0, path, rgba(CREAM, 1.0));
            assert!(pixmap
                .data()
                .as_chunks::<4>()
                .0
                .iter()
                .any(|pixel| pixel[3] > 0));
        }
    }

    #[test]
    fn native_surface_and_video_overlay_derive_connection_copy_from_one_tracker() {
        let connection = Arc::new(ConnectionTracker::default());
        let mut state = OverlayState::with_connection(
            crate::window::PlaybackProfile::FriendViewer { cascade: 0 },
            connection.clone(),
        );
        state.video = (1920, 1080);
        state.fps = Some(60.0);
        assert_eq!(state.quality_label(), "1080p60");

        connection.begin();
        connection.advance(ConnectionEvent::IceChecking);
        assert_eq!(state.quality_label(), "Finding a direct route");
        assert_eq!(
            state.detail_label().as_deref(),
            Some("ICE is checking available network paths")
        );

        connection.advance(ConnectionEvent::FirstVideoFrame);
        assert_eq!(state.quality_label(), "1080p60");
    }
}
