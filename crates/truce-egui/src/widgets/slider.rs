//! Parameter slider bound to truce's gesture protocol.

use crate::ParamState;

/// Show a horizontal slider bound to a truce parameter.
///
/// Uses egui's built-in `Slider` widget with normalized 0.0-1.0 range.
/// Automatically handles begin/set/end gesture protocol for host automation.
pub fn param_slider(ui: &mut egui::Ui, state: &ParamState, id: u32) -> egui::Response {
    let mut value = state.get(id) as f32;
    let response = ui.add(egui::Slider::new(&mut value, 0.0..=1.0));
    if response.drag_started() {
        state.begin_gesture(id);
    }
    if response.changed() {
        state.set_value(id, value as f64);
    }
    if response.drag_stopped() {
        state.end_gesture(id);
    }
    response
}
