//! Parameter dropdown (combo box) bound to a truce parameter.

use crate::ParamState;

/// Show a dropdown (combo box) bound to a truce parameter.
///
/// Displays the current value and opens a popup list when clicked,
/// allowing the user to select from all available values.
///
/// `step_count` is the number of discrete steps (e.g., 3 for an enum
/// with 3 variants). `options` provides the formatted label for each step.
pub fn param_dropdown(
    ui: &mut egui::Ui,
    state: &ParamState,
    id: impl Into<u32>,
    label: &str,
    step_count: u32,
    options: &[String],
) -> egui::Response {
    let id_u32 = id.into();
    let current_text = state.format(id_u32);

    let desired = egui::vec2(100.0, 40.0);
    let (rect, _) = ui.allocate_exact_size(desired, egui::Sense::hover());

    let box_rect = egui::Rect::from_center_size(
        egui::pos2(rect.center().x, rect.top() + 14.0),
        egui::vec2(96.0, 22.0),
    );

    let combo_id = egui::Id::new(("truce_dropdown", id_u32));
    let response = egui::ComboBox::from_id_salt(combo_id)
        .selected_text(&current_text)
        .width(box_rect.width())
        .show_ui(ui, |ui| {
            let count = step_count.max(1) as usize;
            for (i, opt) in options.iter().enumerate() {
                let norm = if count <= 1 {
                    0.0
                } else {
                    i as f64 / (count - 1) as f64
                };
                let is_selected = (state.get(id_u32) - norm).abs() < 0.01;
                if ui.selectable_label(is_selected, opt).clicked() {
                    state.set_immediate(id_u32, norm);
                }
            }
        })
        .response;

    // Label below
    if ui.is_rect_visible(rect) {
        let dim_color = ui.visuals().widgets.noninteractive.fg_stroke.color;
        ui.painter().text(
            egui::pos2(rect.center().x, rect.bottom() - 2.0),
            egui::Align2::CENTER_BOTTOM,
            label,
            egui::FontId::proportional(10.0),
            dim_color,
        );
    }

    response
}
