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
/// Y of the label's top edge inside the cell - exactly where
/// `param_knob` paints its own. Painted directly the same way (not laid
/// out after the `ComboBox`), so egui's widget metrics can't move it:
/// egui 0.37's taller `interact_size` pushed a flowed label ~2 pt below
/// the knobs'.
const LABEL_TOP_Y: f32 = 69.0;
/// Space above the `ComboBox` button, centring it in the area over the
/// label.
const COMBO_TOP_PAD: f32 = 25.0;

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
    child.add_space(COMBO_TOP_PAD);
    // egui 0.37's 8 pt button padding leaves a 1-col cell too narrow for
    // a 6-char option ("Mode A", "Square") and wraps it mid-word into a
    // taller button that can reach the label. Tighter padding fits those
    // again; anything longer truncates with an ellipsis (`cols` widens
    // the cell) rather than wrapping.
    child.spacing_mut().button_padding.x = 4.0;

    let cur_value: f32 = state.get_param(id);
    let combo_resp = egui::ComboBox::from_id_salt(("truce-egui:param_dropdown", id))
        .selected_text(&current_text)
        .width(cell_w - 4.0)
        .truncate()
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

    let dim = ui.visuals().widgets.noninteractive.fg_stroke.color;
    // Clipped to the cell like `param_knob`'s, so a long label can't run
    // into the neighbour.
    ui.painter_at(rect).text(
        egui::pos2(rect.center().x, rect.top() + LABEL_TOP_Y),
        egui::Align2::CENTER_TOP,
        label,
        egui::FontId::proportional(10.0),
        dim,
    );
    combo_resp.response
}
