//! Parameter selector (dropdown/cycle) bound to a truce parameter.

use crate::ParamState;

/// Show a cycling selector bound to a truce parameter.
///
/// Displays the current formatted value. Click to cycle through options.
/// For parameters with discrete values (enums, integer ranges), this
/// cycles through all valid values using the host's formatting.
///
/// `step_count` is the number of discrete steps (e.g., 3 for an enum
/// with 3 variants). If 0, cycles in 0.1 increments.
pub fn param_selector(
    ui: &mut egui::Ui,
    state: &ParamState,
    id: impl Into<u32>,
    label: &str,
    step_count: u32,
) -> egui::Response {
    let id = id.into();
    let current_text = state.format(id);

    let desired = egui::vec2(80.0, 40.0);
    let (rect, response) = ui.allocate_exact_size(desired, egui::Sense::click());

    if response.clicked() {
        let value = state.get(id);
        let new_value = if step_count > 1 {
            // Cycle through discrete steps
            let step = 1.0 / (step_count - 1) as f64;
            let next = value + step;
            if next > 1.0 + step * 0.5 {
                0.0
            } else {
                next.min(1.0)
            }
        } else {
            // Cycle in 0.1 increments
            let next = ((value * 10.0).round() + 1.0) / 10.0;
            if next > 1.0 {
                0.0
            } else {
                next
            }
        };
        state.set_immediate(id, new_value);
    }

    if ui.is_rect_visible(rect) {
        let painter = ui.painter_at(rect);

        // Background box
        let box_rect = egui::Rect::from_center_size(
            egui::pos2(rect.center().x, rect.top() + 14.0),
            egui::vec2(76.0, 22.0),
        );
        let bg = if response.hovered() {
            ui.visuals().widgets.hovered.bg_fill
        } else {
            ui.visuals().widgets.inactive.bg_fill
        };
        painter.rect_filled(box_rect, 4.0, bg);

        // Value text
        let text_color = ui.visuals().text_color();
        painter.text(
            box_rect.center(),
            egui::Align2::CENTER_CENTER,
            &current_text,
            egui::FontId::proportional(12.0),
            text_color,
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
