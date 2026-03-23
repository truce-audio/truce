//! XY pad control bound to two truce parameters.

use crate::ParamState;

const PAD_MARGIN: f32 = 4.0;
const LABEL_H: f32 = 14.0;
const DOT_RADIUS: f32 = 6.0;

/// Show an XY pad bound to two truce parameters.
///
/// `id_x` controls the horizontal axis (0=left, 1=right).
/// `id_y` controls the vertical axis (0=bottom, 1=top).
/// Drag anywhere in the pad to set both values simultaneously.
pub fn param_xy_pad(
    ui: &mut egui::Ui,
    state: &ParamState,
    id_x: u32,
    id_y: u32,
    label: &str,
) -> egui::Response {
    let desired = egui::vec2(120.0, 120.0 + LABEL_H);
    let (rect, response) = ui.allocate_exact_size(desired, egui::Sense::drag());

    let pad_rect = egui::Rect::from_min_max(
        egui::pos2(rect.left() + PAD_MARGIN, rect.top() + PAD_MARGIN),
        egui::pos2(rect.right() - PAD_MARGIN, rect.bottom() - PAD_MARGIN - LABEL_H),
    );

    let mut vx = state.get(id_x) as f32;
    let mut vy = state.get(id_y) as f32;

    if response.drag_started() {
        state.begin_gesture(id_x);
        state.begin_gesture(id_y);
    }
    if response.dragged() || response.drag_started() {
        if let Some(pos) = response.interact_pointer_pos() {
            vx = ((pos.x - pad_rect.left()) / pad_rect.width()).clamp(0.0, 1.0);
            vy = 1.0 - ((pos.y - pad_rect.top()) / pad_rect.height()).clamp(0.0, 1.0);
            state.set_value(id_x, vx as f64);
            state.set_value(id_y, vy as f64);
        }
    }
    if response.drag_stopped() {
        state.end_gesture(id_x);
        state.end_gesture(id_y);
    }

    if ui.is_rect_visible(rect) {
        let painter = ui.painter_at(rect);

        let bg = ui.visuals().extreme_bg_color;
        let grid_color = egui::Color32::from_gray(45);
        let active_color = ui.visuals().widgets.active.bg_fill;
        let dim_color = ui.visuals().widgets.noninteractive.fg_stroke.color;

        // Background
        painter.rect_filled(pad_rect, 4.0, bg);

        // Grid lines (crosshair at center)
        let cx = pad_rect.center().x;
        let cy = pad_rect.center().y;
        painter.line_segment(
            [egui::pos2(cx, pad_rect.top()), egui::pos2(cx, pad_rect.bottom())],
            egui::Stroke::new(1.0, grid_color),
        );
        painter.line_segment(
            [egui::pos2(pad_rect.left(), cy), egui::pos2(pad_rect.right(), cy)],
            egui::Stroke::new(1.0, grid_color),
        );

        // Dot at current position
        let dot_x = pad_rect.left() + pad_rect.width() * vx;
        let dot_y = pad_rect.top() + pad_rect.height() * (1.0 - vy);
        painter.circle_filled(egui::pos2(dot_x, dot_y), DOT_RADIUS, active_color);
        painter.circle_stroke(
            egui::pos2(dot_x, dot_y),
            DOT_RADIUS,
            egui::Stroke::new(1.5, egui::Color32::WHITE),
        );

        // Crosshair lines to dot
        painter.line_segment(
            [egui::pos2(dot_x, pad_rect.top()), egui::pos2(dot_x, pad_rect.bottom())],
            egui::Stroke::new(0.5, active_color.linear_multiply(0.3)),
        );
        painter.line_segment(
            [egui::pos2(pad_rect.left(), dot_y), egui::pos2(pad_rect.right(), dot_y)],
            egui::Stroke::new(0.5, active_color.linear_multiply(0.3)),
        );

        // Label
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
