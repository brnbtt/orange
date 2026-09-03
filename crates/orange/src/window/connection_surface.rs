use crate::connection::ConnectionStage;
use crate::text::{self, Weight};
use tiny_skia::{Color, FillRule, Paint, PathBuilder, Pixmap, Rect, Stroke, Transform};

const MARGIN: f32 = 18.0;
const BUTTON: f32 = 40.0;

fn surface_scale(width: u32, height: u32, dpi: f32) -> f32 {
    dpi.clamp(0.75, 4.0)
        .min((width as f32 / 480.0).max(0.65))
        .min((height as f32 / 270.0).max(0.65))
}

pub(super) fn close_hit_test(x: f32, y: f32, width: u32, height: u32, dpi: f32) -> bool {
    let scale = surface_scale(width, height, dpi);
    let left = width as f32 - (MARGIN + BUTTON) * scale;
    let top = MARGIN * scale;
    x >= left && x <= left + BUTTON * scale && y >= top && y <= top + BUTTON * scale
}

pub(super) fn render(width: u32, height: u32, dpi: f32, stage: ConnectionStage) -> Option<Pixmap> {
    if width == 0 || height == 0 || stage.is_connected() {
        return None;
    }

    let mut pixmap = Pixmap::new(width, height)?;
    pixmap.fill(Color::from_rgba8(7, 7, 8, 255));
    let scale = surface_scale(width, height, dpi);
    let center_x = width as f32 / 2.0;
    let center_y = height as f32 / 2.0;
    let copy = stage.copy();

    let mut accent = Paint::default();
    accent.set_color(Color::from_rgba8(255, 90, 31, 255));
    accent.anti_alias = true;
    if let Some(rect) = Rect::from_xywh(
        center_x - 14.0 * scale,
        center_y - 76.0 * scale,
        28.0 * scale,
        3.0 * scale,
    ) {
        pixmap.fill_rect(rect, &accent, Transform::identity(), None);
    }

    for (label, size, baseline, weight, color) in [
        (
            copy.primary,
            30.0 * scale,
            center_y - 26.0 * scale,
            Weight::Semibold,
            Color::from_rgba8(230, 224, 209, 255),
        ),
        (
            copy.title,
            16.0 * scale,
            center_y + 17.0 * scale,
            Weight::Semibold,
            Color::from_rgba8(230, 224, 209, 255),
        ),
        (
            copy.detail,
            13.0 * scale,
            center_y + 45.0 * scale,
            Weight::Regular,
            Color::from_rgba8(154, 150, 141, 255),
        ),
    ] {
        let x = (center_x - text::width(label, size, weight) / 2.0).max(12.0 * scale);
        text::draw(&mut pixmap, x, baseline, label, size, weight, color);
    }

    let button = BUTTON * scale;
    let left = width as f32 - (MARGIN + BUTTON) * scale;
    let top = MARGIN * scale;
    let radius = 12.0 * scale;
    let mut panel = PathBuilder::new();
    panel.move_to(left + radius, top);
    panel.line_to(left + button - radius, top);
    panel.quad_to(left + button, top, left + button, top + radius);
    panel.line_to(left + button, top + button - radius);
    panel.quad_to(
        left + button,
        top + button,
        left + button - radius,
        top + button,
    );
    panel.line_to(left + radius, top + button);
    panel.quad_to(left, top + button, left, top + button - radius);
    panel.line_to(left, top + radius);
    panel.quad_to(left, top, left + radius, top);
    panel.close();
    if let Some(panel) = panel.finish() {
        let mut fill = Paint::default();
        fill.set_color(Color::from_rgba8(20, 20, 22, 247));
        fill.anti_alias = true;
        pixmap.fill_path(
            &panel,
            &fill,
            FillRule::Winding,
            Transform::identity(),
            None,
        );
        let mut border = Paint::default();
        border.set_color(Color::from_rgba8(42, 42, 46, 235));
        border.anti_alias = true;
        pixmap.stroke_path(
            &panel,
            &border,
            &Stroke {
                width: scale,
                ..Default::default()
            },
            Transform::identity(),
            None,
        );
    }
    let inset = 13.0 * scale;
    let mut cross = PathBuilder::new();
    cross.move_to(left + inset, top + inset);
    cross.line_to(left + button - inset, top + button - inset);
    cross.move_to(left + button - inset, top + inset);
    cross.line_to(left + inset, top + button - inset);
    if let Some(cross) = cross.finish() {
        let mut ink = Paint::default();
        ink.set_color(Color::from_rgba8(230, 224, 209, 230));
        ink.anti_alias = true;
        pixmap.stroke_path(
            &cross,
            &ink,
            &Stroke {
                width: 2.0 * scale,
                line_cap: tiny_skia::LineCap::Round,
                ..Default::default()
            },
            Transform::identity(),
            None,
        );
    }

    Some(pixmap)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::connection::{ConnectionFailure, ConnectionStage};

    #[test]
    fn pre_video_surface_uses_orange_background_accent_and_text() {
        let pixmap = render(1280, 720, 1.0, ConnectionStage::FindingDirectRoute)
            .expect("connection surface should render");

        assert_eq!((pixmap.width(), pixmap.height()), (1280, 720));
        assert_eq!(&pixmap.data()[..4], &[7, 7, 8, 255]);
        assert!(pixmap
            .data()
            .as_chunks::<4>()
            .0
            .iter()
            .any(|pixel| { pixel[0] > 240 && pixel[1] > 60 && pixel[1] < 130 && pixel[2] < 70 }));
        assert!(pixmap
            .data()
            .as_chunks::<4>()
            .0
            .iter()
            .any(|pixel| { pixel[0] > 170 && pixel[1] > 160 && pixel[2] > 140 }));
    }

    #[test]
    fn failure_replaces_progress_on_the_same_dark_surface() {
        let pixmap = render(
            640,
            360,
            1.0,
            ConnectionStage::Failed(ConnectionFailure::Network),
        )
        .expect("failure surface should render");

        assert_eq!(&pixmap.data()[..4], &[7, 7, 8, 255]);
        assert!(pixmap
            .data()
            .as_chunks::<4>()
            .0
            .iter()
            .any(|pixel| { pixel[0] > 240 && pixel[1] > 60 && pixel[1] < 130 && pixel[2] < 70 }));
    }

    #[test]
    fn connected_video_has_no_native_surface_to_cover_the_sink() {
        assert!(render(1280, 720, 1.0, ConnectionStage::Connected).is_none());
    }

    #[test]
    fn pre_video_close_target_matches_the_visible_top_right_control() {
        assert!(close_hit_test(1242.0, 38.0, 1280, 720, 1.0));
        assert!(close_hit_test(1222.0, 18.0, 1280, 720, 1.0));
        assert!(!close_hit_test(640.0, 360.0, 1280, 720, 1.0));
        assert!(!close_hit_test(1215.0, 65.0, 1280, 720, 1.0));
    }
}
