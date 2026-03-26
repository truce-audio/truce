//! Rotary knob control bound to a truce parameter.

use crate::ParamState;

const KNOB_SIZE: f32 = 60.0;
const KNOB_TOTAL_H: f32 = 90.0;
const KNOB_RADIUS: f32 = 22.0;
const TRACK_WIDTH: f32 = 3.0;

/// 270-degree arc: from bottom-left (135°) to bottom-right (405°).
const START_ANGLE: f32 = std::f32::consts::FRAC_PI_4 * 3.0; // 135° = 0.75π
const SWEEP: f32 = std::f32::consts::FRAC_PI_2 * 3.0; // 270° = 1.5π

/// Show a rotary knob bound to a truce parameter.
///
/// Drag vertically to adjust the value. The knob displays a 270-degree
/// arc with a value indicator and the parameter's formatted value text.
pub fn param_knob(
    ui: &mut egui::Ui,
    state: &ParamState,
    id: impl Into<u32>,
    label: &str,
) -> egui::Response {
    let id = id.into();
    let desired = egui::vec2(KNOB_SIZE, KNOB_TOTAL_H);
    let (rect, response) = ui.allocate_exact_size(desired, egui::Sense::drag());

    let mut value = state.get(id) as f32;

    // Handle vertical drag
    if response.drag_started() {
        state.begin_gesture(id);
    }
    if response.dragged() {
        let delta = -response.drag_delta().y / 150.0;
        value = (value + delta).clamp(0.0, 1.0);
        state.set_value(id, value as f64);
    }
    if response.drag_stopped() {
        state.end_gesture(id);
    }

    // Paint
    if ui.is_rect_visible(rect) {
        let painter = ui.painter_at(rect);
        let center = egui::pos2(rect.center().x, rect.top() + KNOB_RADIUS + 6.0);

        let track_color = ui.visuals().widgets.inactive.bg_fill;
        let active_color = ui.visuals().widgets.active.bg_fill;
        let pointer_color = ui.visuals().widgets.active.fg_stroke.color;
        let text_color = ui.visuals().text_color();
        let dim_color = ui.visuals().widgets.noninteractive.fg_stroke.color;
        let hover_color = egui::Color32::from_gray(115);

        // Hover highlight ring
        let hovered = response.hovered() || response.dragged();
        if hovered {
            let hover_points = arc_points(center, KNOB_RADIUS + 3.0, START_ANGLE, SWEEP, 64);
            painter.add(egui::Shape::line(
                hover_points,
                egui::Stroke::new(1.5, hover_color),
            ));
        }

        // Track arc (background)
        let track_points = arc_points(center, KNOB_RADIUS, START_ANGLE, SWEEP, 64);
        painter.add(egui::Shape::line(
            track_points,
            egui::Stroke::new(TRACK_WIDTH, track_color),
        ));

        // Value arc (filled portion)
        if value > 0.001 {
            let value_sweep = SWEEP * value;
            let n = ((value * 64.0) as usize).max(2);
            let value_points = arc_points(center, KNOB_RADIUS, START_ANGLE, value_sweep, n);
            painter.add(egui::Shape::line(
                value_points,
                egui::Stroke::new(TRACK_WIDTH, active_color),
            ));
        }

        // Pointer line from center to value position
        let value_angle = START_ANGLE + SWEEP * value;
        let pointer_len = KNOB_RADIUS * 0.6;
        let pointer_end = center + egui::vec2(value_angle.cos(), value_angle.sin()) * pointer_len;
        painter.line_segment(
            [center, pointer_end],
            egui::Stroke::new(2.0, pointer_color),
        );

        // Value text (below knob arc)
        let value_text = state.format(id);
        let value_y = center.y + KNOB_RADIUS + 8.0;
        painter.text(
            egui::pos2(rect.center().x, value_y),
            egui::Align2::CENTER_TOP,
            &value_text,
            egui::FontId::proportional(10.0),
            text_color,
        );

        // Label below value
        painter.text(
            egui::pos2(rect.center().x, rect.bottom() - 2.0),
            egui::Align2::CENTER_BOTTOM,
            label,
            egui::FontId::proportional(10.0),
            dim_color,
        );
    }

    response
}

/// Generate points along a circular arc.
fn arc_points(
    center: egui::Pos2,
    radius: f32,
    start: f32,
    sweep: f32,
    segments: usize,
) -> Vec<egui::Pos2> {
    (0..=segments)
        .map(|i| {
            let t = i as f32 / segments as f32;
            let angle = start + sweep * t;
            center + egui::vec2(angle.cos(), angle.sin()) * radius
        })
        .collect()
}
