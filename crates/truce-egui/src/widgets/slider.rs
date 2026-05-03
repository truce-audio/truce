//! Parameter slider bound to truce's gesture protocol.

use truce_core::editor::PluginContext;

/// Show a horizontal slider bound to a truce parameter.
///
/// Uses egui's built-in `Slider` widget with normalized 0.0-1.0 range.
/// Automatically handles begin/set/end gesture protocol for host automation.
pub fn param_slider<P: ?Sized>(
    ui: &mut egui::Ui,
    state: &PluginContext<P>,
    id: impl Into<u32>,
) -> egui::Response {
    let id = id.into();
    let mut value = state.get_param(id) as f32;
    let response = ui.add(egui::Slider::new(&mut value, 0.0..=1.0));
    if response.drag_started() {
        state.begin_edit(id);
    }
    if response.changed() {
        state.set_param(id, value as f64);
    }
    if response.drag_stopped() {
        state.end_edit(id);
    }
    response
}
