//! Parameter dropdown (click-to-open list) bound to a truce parameter.
//!
//! Thin wrapper over egui's stock [`egui::ComboBox`] that pulls the
//! current label + the full option list out of the param's
//! [`truce_params::Params::format_value`] and wires the chosen option
//! back through [`PluginContext::automate`]. Useful for `EnumParam`
//! or `IntParam` parameters with many options.

use truce_core::cast::discrete_norm;
use truce_core::editor::{PluginContext, PluginContextReadF32};
use truce_params::{Params, sample::Float};

// Match `param_knob`'s 82px cell height + label-at-y69 layout so a
// row of mixed dropdowns / knobs has all labels at the same baseline.
const CELL_W: f32 = 80.0;
const CELL_H: f32 = 82.0;
/// Y of the label's top edge inside the cell. The knob paints its
/// label with a direct `painter.text(..., Align2::CENTER_TOP)` call
/// at `rect.top + 69`, whose visual top sits a few pixels lower
/// than where `egui::Label` rendered through a `top_down` layout
/// lands at the same y - the layout path applies extra item spacing
/// above the label. Nudging the target down by ~3 px makes the
/// label baselines match by eye.
const LABEL_TOP_Y: f32 = 72.0;
/// `ComboBox` button height under egui's default spacing.
const COMBO_TOTAL_Y: f32 = 22.0;

/// Render a click-to-open dropdown for the param `id` with `label`
/// underneath. Option labels are derived from the param's range +
/// `Params::format_value` so they match what the host shows in its
/// automation lanes.
///
/// `cols` is the cell-column span (1-based). 1 produces the default
/// `CELL_W` cell; 2 produces a `2 * CELL_W` cell, etc. Mirrors the
/// built-in GUI's `.cols(N)` modifier for visual parity when a
/// long-form option label (`"Whole note"`, `"Sixteenth"`) needs more
/// horizontal room than the default cell can hold.
///
/// Returns the encompassing [`egui::Response`] so callers can attach
/// tooltips or chain interactions.
pub fn param_dropdown<P: Params + ?Sized>(
    ui: &mut egui::Ui,
    state: &PluginContext<P>,
    id: impl Into<u32>,
    label: &str,
    cols: u32,
) -> egui::Response {
    let id = id.into();
    let current_text = state.format_param(id);

    // Enumerate option labels via the same path the built-in GUI's
    // `get_options` closure uses (see `truce-gui/src/render_core.rs`).
    let params = state.params();
    let infos = params.param_infos();
    let Some(info) = infos.iter().find(|i| i.id == id).copied() else {
        // Param id not recognised - draw a disabled placeholder so the
        // layout doesn't collapse but no automation can land.
        let resp = ui.add_enabled(false, egui::Label::new(format!("(unknown {id})")));
        ui.label(label);
        return resp;
    };
    let count = info.range.step_count_usize() + 1;

    // Column spans below 1 collapse to 1; clamp to 16 so the cast to
    // f32 is exact (mantissa is 23 bits) and a wild caller value
    // doesn't allocate a half-screen-wide cell.
    #[allow(clippy::cast_precision_loss)] // bounded by clamp above
    let span = cols.clamp(1, 16) as f32;
    let cell_w = CELL_W * span;
    let desired = egui::vec2(cell_w, CELL_H);
    let (rect, _) = ui.allocate_exact_size(desired, egui::Sense::hover());
    let mut child = ui.new_child(
        egui::UiBuilder::new()
            .max_rect(rect)
            .layout(egui::Layout::top_down(egui::Align::Center)),
    );
    // Vertically center the ComboBox button inside the cell. The label
    // still anchors at `LABEL_TOP_Y` to line up with `param_knob`'s
    // label. Layout from the top: pad above to center the button, then
    // pad below to drop the label at the shared Y.
    let combo_top_pad = ((LABEL_TOP_Y - COMBO_TOTAL_Y) / 2.0).max(0.0);
    let combo_bottom_pad = (LABEL_TOP_Y - COMBO_TOTAL_Y - combo_top_pad).max(0.0);
    child.add_space(combo_top_pad);

    let cur_value: f32 = state.get_param(id);
    let combo_resp = egui::ComboBox::from_id_salt(("truce-egui:param_dropdown", id))
        .selected_text(&current_text)
        .width(cell_w - 4.0)
        .show_ui(&mut child, |ui| {
            for i in 0..count {
                let norm = discrete_norm(i, count);
                let plain = info.range.denormalize(norm);
                let label_text = params
                    .format_value(id, plain)
                    .unwrap_or_else(|| format!("{plain:.0}"));
                let norm_f32 = f32::from_f64(norm);
                let selected = (cur_value - norm_f32).abs() < f32::EPSILON.max(1e-4);
                if ui.selectable_label(selected, label_text).clicked() {
                    state.automate(id, norm);
                }
            }
        });

    child.add_space(combo_bottom_pad);
    let dim = ui.visuals().widgets.noninteractive.fg_stroke.color;
    child.add(egui::Label::new(
        egui::RichText::new(label).color(dim).size(10.0),
    ));
    combo_resp.response
}
