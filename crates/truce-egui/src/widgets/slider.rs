//! Parameter slider bound to truce's gesture protocol.

use truce_core::editor::{PluginContext, PluginContextReadF32};
use truce_params::{ParamRange, Params};

/// Show a horizontal slider bound to a truce parameter.
///
/// For `Discrete` ranges (`IntParam` / `BoolParam`) the slider works
/// in plain-value space (`min..=max`), snaps to integer steps, and
/// shows the integer plain value. For `Linear` / `Logarithmic`
/// continuous ranges the slider stays in normalized `[0.0, 1.0]`
/// space (egui's slider doesn't natively grok log ticks).
/// Begin/set/end gesture wrapping matches the host-automation
/// protocol either way.
pub fn param_slider<P: Params + ?Sized>(
    ui: &mut egui::Ui,
    state: &PluginContext<P>,
    id: impl Into<u32>,
) -> egui::Response {
    let id = id.into();
    let range = state
        .params()
        .param_infos()
        .iter()
        .find(|i| i.id == id)
        .map(|i| i.range);
    if let Some(ParamRange::Discrete { min, max }) = range {
        // `get_param_plain` returns an integer-valued f32 for IntParam;
        // the i64 round is exact for the legal range [i32::MIN, i32::MAX].
        #[allow(clippy::cast_possible_truncation)]
        let mut plain = state.get_param_plain(id).round() as i64;
        let response = ui.add(egui::Slider::new(&mut plain, min..=max).integer());
        if response.drag_started() {
            state.begin_edit(id);
        }
        if response.changed() {
            #[allow(clippy::cast_precision_loss)]
            let norm = ParamRange::Discrete { min, max }.normalize(plain as f64);
            state.set_param(id, norm);
        }
        if response.drag_stopped() {
            state.end_edit(id);
        }
        return response;
    }
    let mut value = state.get_param(id);
    let response = ui.add(egui::Slider::new(&mut value, 0.0..=1.0));
    if response.drag_started() {
        state.begin_edit(id);
    }
    if response.changed() {
        state.set_param(id, f64::from(value));
    }
    if response.drag_stopped() {
        state.end_edit(id);
    }
    response
}
