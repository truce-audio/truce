//! Toggle switch bound to a truce parameter.

use crate::ParamState;

/// Show a toggle switch bound to a truce boolean parameter.
///
/// Clicking toggles between 0.0 (off) and 1.0 (on). Uses the immediate
/// gesture protocol (begin + set + end in one shot).
pub fn param_toggle(
    ui: &mut egui::Ui,
    state: &ParamState,
    id: u32,
    label: &str,
) -> egui::Response {
    let is_on = state.get(id) > 0.5;

    let desired = egui::vec2(60.0, 30.0);
    let (rect, response) = ui.allocate_exact_size(desired, egui::Sense::click());

    if response.clicked() {
        let new_value = if is_on { 0.0 } else { 1.0 };
        state.set_immediate(id, new_value);
    }

    if ui.is_rect_visible(rect) {
        let painter = ui.painter_at(rect);

        // Switch track
        let track_h = 16.0;
        let track_w = 32.0;
        let track_rect = egui::Rect::from_center_size(
            egui::pos2(rect.center().x, rect.top() + track_h / 2.0 + 2.0),
            egui::vec2(track_w, track_h),
        );

        let track_color = if is_on {
            ui.visuals().widgets.active.bg_fill
        } else {
            ui.visuals().widgets.inactive.bg_fill
        };
        painter.rect_filled(track_rect, track_h / 2.0, track_color);

        // Switch thumb
        let thumb_x = if is_on {
            track_rect.right() - track_h / 2.0
        } else {
            track_rect.left() + track_h / 2.0
        };
        painter.circle_filled(
            egui::pos2(thumb_x, track_rect.center().y),
            track_h / 2.0 - 2.0,
            egui::Color32::WHITE,
        );

        // Label
        let dim_color = ui.visuals().widgets.noninteractive.fg_stroke.color;
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
