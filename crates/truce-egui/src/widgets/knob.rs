//! Rotary knob control bound to a truce parameter.

use truce_core::editor::{PluginContext, PluginContextReadF32};
use truce_params::Params;

const KNOB_SIZE: f32 = 60.0;
const KNOB_TOTAL_H: f32 = 82.0;
const KNOB_RADIUS: f32 = 25.0;
const TRACK_WIDTH: f32 = 3.0;

/// 270-degree arc: from bottom-left (135°) to bottom-right (405°).
const START_ANGLE: f32 = std::f32::consts::FRAC_PI_4 * 3.0; // 135° = 0.75π
const SWEEP: f32 = std::f32::consts::FRAC_PI_2 * 3.0; // 270° = 1.5π

/// Show a rotary knob bound to a truce parameter.
///
/// Drag vertically to adjust the value. The knob displays a 270-degree
/// arc with a value indicator and the parameter's formatted value text.
///
/// Discrete-ranged params (`Discrete` / `Enum`) snap to their nearest
/// step on each frame. Snapping uses a per-drag accumulator stored in
/// egui memory so sub-step pointer movement isn't swallowed by the
/// host's snap-on-write round-trip - without it, dragging a discrete
/// knob feels stuck because each frame writes a sub-step delta that
/// the host rounds back to the same value.
pub fn param_knob<P: Params + ?Sized>(
    ui: &mut egui::Ui,
    state: &PluginContext<P>,
    id: impl Into<u32>,
    label: &str,
) -> egui::Response {
    let id = id.into();
    let desired = egui::vec2(KNOB_SIZE, KNOB_TOTAL_H);
    let (rect, response) = ui.allocate_exact_size(desired, egui::Sense::drag());

    let mut value = state.get_param(id);

    // Number of transitions between adjacent discrete values for this
    // param, or `None` for continuous params. Cast bound from
    // `step_count`'s `NonZeroU32` (`<= u32::MAX`) into the typical
    // single-digit count range an enum / discrete param sees in
    // practice - the precision-loss `as f32` cast is exact for the
    // values we care about.
    let step_count = state
        .params()
        .param_infos()
        .iter()
        .find(|i| i.id == id)
        .and_then(|info| info.range.step_count());

    let acc_id = ui.make_persistent_id(("truce-egui:param_knob:acc", id));

    if response.drag_started() {
        state.begin_edit(id);
        // Seed the accumulator at the host's current snapped value so
        // the first drag frame starts from where the user clicked.
        ui.memory_mut(|m| m.data.insert_temp(acc_id, value));
    }
    if response.dragged() {
        let delta = -response.drag_delta().y / 150.0;
        let prev_unrounded: f32 = ui
            .memory(|m| m.data.get_temp::<f32>(acc_id))
            .unwrap_or(value);
        let unrounded = (prev_unrounded + delta).clamp(0.0, 1.0);
        ui.memory_mut(|m| m.data.insert_temp(acc_id, unrounded));
        let snapped = if let Some(steps) = step_count {
            #[allow(clippy::cast_precision_loss)] // step counts fit in f32 mantissa
            let n = steps.get() as f32;
            (unrounded * n).round() / n
        } else {
            unrounded
        };
        if (snapped - value).abs() > 1e-5 {
            value = snapped;
            state.set_param(id, f64::from(value));
        }
    }
    if response.drag_stopped() {
        state.end_edit(id);
        ui.memory_mut(|m| m.data.remove_temp::<f32>(acc_id));
    }

    // Paint
    if ui.is_rect_visible(rect) {
        let painter = ui.painter_at(rect);
        let center = egui::pos2(rect.center().x, rect.top() + KNOB_RADIUS + 5.0);

        let track_color = ui.visuals().widgets.inactive.bg_fill;
        let active_color = ui.visuals().widgets.active.bg_fill;
        let pointer_color = ui.visuals().widgets.active.fg_stroke.color;
        let text_color = ui.visuals().text_color();
        let dim_color = ui.visuals().widgets.noninteractive.fg_stroke.color;
        let hover_color = egui::Color32::from_gray(115);

        // Hover highlight ring
        let hovered = response.hovered() || response.dragged();
        if hovered {
            let hover_points = arc_points(center, KNOB_RADIUS + 3.0, START_ANGLE, SWEEP, 64);
            painter.add(egui::Shape::line(
                hover_points,
                egui::Stroke::new(1.5_f32, hover_color),
            ));
        }

        // Track arc (background)
        let track_points = arc_points(center, KNOB_RADIUS, START_ANGLE, SWEEP, 64);
        painter.add(egui::Shape::line(
            track_points,
            egui::Stroke::new(TRACK_WIDTH, track_color),
        ));

        // Value arc (filled portion)
        if value > 0.001 {
            let value_sweep = SWEEP * value;
            // `value` is in `(0, 1]` so `value * 64.0` ≤ 64; the
            // truncation to usize is bounded.
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            let n = ((value * 64.0) as usize).max(2);
            let value_points = arc_points(center, KNOB_RADIUS, START_ANGLE, value_sweep, n);
            painter.add(egui::Shape::line(
                value_points,
                egui::Stroke::new(TRACK_WIDTH, active_color),
            ));
        }

        // Pointer line from center to value position
        let value_angle = START_ANGLE + SWEEP * value;
        let pointer_len = KNOB_RADIUS * 0.6;
        let pointer_end = center + egui::vec2(value_angle.cos(), value_angle.sin()) * pointer_len;
        painter.line_segment(
            [center, pointer_end],
            egui::Stroke::new(2.0_f32, pointer_color),
        );

        // Value text (below knob arc)
        let value_text = state.format_param(id);
        let value_y = center.y + KNOB_RADIUS + 2.0;
        painter.text(
            egui::pos2(rect.center().x, value_y),
            egui::Align2::CENTER_TOP,
            &value_text,
            egui::FontId::proportional(10.0),
            text_color,
        );

        // Label below value
        painter.text(
            egui::pos2(rect.center().x, value_y + 12.0),
            egui::Align2::CENTER_TOP,
            label,
            egui::FontId::proportional(10.0),
            dim_color,
        );
    }

    response
}

/// Generate points along a circular arc.
//
// `i / segments` arc parameterization; segments is small (knob arc
// resolution, typically <= 64), so the f32 widening is exact.
#[allow(clippy::cast_precision_loss)]
fn arc_points(
    center: egui::Pos2,
    radius: f32,
    start: f32,
    sweep: f32,
    segments: usize,
) -> Vec<egui::Pos2> {
    (0..=segments)
        .map(|i| {
            let t = i as f32 / segments as f32;
            let angle = start + sweep * t;
            center + egui::vec2(angle.cos(), angle.sin()) * radius
        })
        .collect()
}
