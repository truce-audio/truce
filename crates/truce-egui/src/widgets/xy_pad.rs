//! XY pad control bound to two truce parameters.

use crate::ParamState;

const LABEL_H: f32 = 16.0;
const DOT_RADIUS: f32 = 5.0;

/// Show an XY pad bound to two truce parameters.
///
/// `id_x` controls the horizontal axis (0=left, 1=right).
/// `id_y` controls the vertical axis (0=bottom, 1=top).
/// Drag anywhere in the pad to set both values simultaneously.
pub fn param_xy_pad(
    ui: &mut egui::Ui,
    state: &ParamState,
    id_x: impl Into<u32>,
    id_y: impl Into<u32>,
    label: &str,
    width: f32,
    height: f32,
) -> egui::Response {
    let id_x = id_x.into();
    let id_y = id_y.into();
    let desired = egui::vec2(width, height + LABEL_H);
    let (rect, response) = ui.allocate_exact_size(desired, egui::Sense::drag());

    let pad_rect = egui::Rect::from_min_size(
        rect.min,
        egui::vec2(width, height),
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

        // Background (matches iced SURFACE)
        painter.rect_filled(pad_rect, 0.0, crate::theme::SURFACE);

        // Border
        let bc = egui::Color32::from_rgb(115, 115, 115);
        let bs = egui::Stroke::new(1.0, bc);
        painter.line_segment([pad_rect.left_top(), pad_rect.right_top()], bs);
        painter.line_segment([pad_rect.right_top(), pad_rect.right_bottom()], bs);
        painter.line_segment([pad_rect.right_bottom(), pad_rect.left_bottom()], bs);
        painter.line_segment([pad_rect.left_bottom(), pad_rect.left_top()], bs);

        // Crosshair position
        let dot_x = pad_rect.left() + pad_rect.width() * vx;
        let dot_y = pad_rect.top() + pad_rect.height() * (1.0 - vy);

        // Crosshair lines (30% opacity accent)
        let crosshair_color = crate::theme::KNOB_FILL.linear_multiply(0.3);
        painter.line_segment(
            [egui::pos2(dot_x, pad_rect.top()), egui::pos2(dot_x, pad_rect.bottom())],
            egui::Stroke::new(1.0, crosshair_color),
        );
        painter.line_segment(
            [egui::pos2(pad_rect.left(), dot_y), egui::pos2(pad_rect.right(), dot_y)],
            egui::Stroke::new(1.0, crosshair_color),
        );

        // Dot
        painter.circle_filled(egui::pos2(dot_x, dot_y), DOT_RADIUS, crate::theme::KNOB_FILL);

        // Label
        let dim_color = ui.visuals().widgets.noninteractive.fg_stroke.color;
        painter.text(
            egui::pos2(rect.center().x, pad_rect.bottom() + 2.0),
            egui::Align2::CENTER_TOP,
            label,
            egui::FontId::proportional(10.0),
            dim_color,
        );
    }

    response
}
