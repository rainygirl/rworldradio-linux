//! A small horizontal LED-style peak-level bar (glossy rounded segments,
//! green/yellow/red by position - styled after a classic vertical studio LED
//! meter, just laid on its side to fit a single status row). Port of the Haiku
//! build's `LevelMeterView`; the level is pushed in by the caller (0.0-1.0) and
//! this has no timer of its own.

use egui::{Color32, CornerRadius, Rect, Response, Sense, Stroke, Ui, Vec2};

const SEGMENT_COUNT: usize = 10;
const SEGMENT_GAP: f32 = 2.0;
const CORNER_RADIUS: u8 = 2;
const SIZE: Vec2 = Vec2::new(80.0, 18.0);

fn lighten(color: Color32, amount: f32) -> Color32 {
    let mix = |channel: u8| (f32::from(channel) + (255.0 - f32::from(channel)) * amount) as u8;
    Color32::from_rgb(mix(color.r()), mix(color.g()), mix(color.b()))
}

fn darken(color: Color32, amount: f32) -> Color32 {
    let mix = |channel: u8| (f32::from(channel) * (1.0 - amount)) as u8;
    Color32::from_rgb(mix(color.r()), mix(color.g()), mix(color.b()))
}

pub fn level_meter(ui: &mut Ui, level: f32) -> Response {
    let level = level.clamp(0.0, 1.0);
    let (rect, response) = ui.allocate_exact_size(SIZE, Sense::hover());
    if !ui.is_rect_visible(rect) {
        return response;
    }

    let painter = ui.painter();
    let lit_count = (level * SEGMENT_COUNT as f32 + 0.5) as usize;
    let segment_width =
        (rect.width() - SEGMENT_GAP * (SEGMENT_COUNT as f32 - 1.0)) / SEGMENT_COUNT as f32;
    // Unlit segments come from the inactive-widget fill rather than the panel or
    // extreme background: in a dark theme those are near-black, which made the
    // unlit part of the meter invisible instead of reading as "off".
    let unlit_base = ui.visuals().widgets.inactive.bg_fill;

    for index in 0..SEGMENT_COUNT {
        let left = rect.left() + index as f32 * (segment_width + SEGMENT_GAP);
        let segment = Rect::from_min_size(
            egui::pos2(left, rect.top()),
            Vec2::new(segment_width, rect.height()),
        );

        let lit = index < lit_count;
        let base = if !lit {
            unlit_base
        } else if index >= SEGMENT_COUNT - 2 {
            Color32::from_rgb(220, 40, 50) // red: top two segments
        } else if index >= SEGMENT_COUNT - 4 {
            Color32::from_rgb(235, 195, 30) // yellow: next two down
        } else {
            Color32::from_rgb(55, 190, 75) // green: everything else
        };

        // Glossy look: a lighter highlight on the top half fading into the base
        // colour - egui has no gradient primitive, so this is two stacked fills
        // rather than the Haiku version's BGradientLinear.
        painter.rect_filled(segment, CornerRadius::same(CORNER_RADIUS), base);
        let highlight = Rect::from_min_max(
            segment.min,
            egui::pos2(segment.max.x, segment.min.y + segment.height() * 0.45),
        );
        painter.rect_filled(
            highlight,
            CornerRadius::same(CORNER_RADIUS),
            lighten(base, if lit { 0.45 } else { 0.12 }),
        );
        painter.rect_stroke(
            segment,
            CornerRadius::same(CORNER_RADIUS),
            Stroke::new(1.0_f32, darken(base, 0.55)),
            egui::StrokeKind::Inside,
        );
    }

    response
}
