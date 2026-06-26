//! Level meter (display-only) reading truce meter values.

use truce_core::editor::PluginContext;
use truce_core::meter_display;

/// Show a vertical level meter reading truce meter values.
///
/// `meter_ids` are the meter IDs to display (one bar per ID). The meter
/// is display-only (no interaction). `height` sets the bar height in
/// pixels. Colors change based on level: blue normally, red when clipping.
/// Single-channel meter width in pixels. The widget grows with
/// channel count: `BAR_W` per bar plus `BAR_GAP` between bars.
const BAR_W: f32 = 4.0;
const BAR_GAP: f32 = 2.0;
const BAR_PAD: f32 = 0.0;
/// Width allocated for a single-channel meter. Picked so a mono
/// meter sits visually inside a knob-sized column.
const MIN_METER_W: f32 = 16.0;
const TRACK_BG: egui::Color32 = egui::Color32::from_rgb(42, 42, 48);

// Channel count `usize as f32` to compute per-bar widths; channel
// counts are tiny (<= 64 in any practical audio config).
#[allow(clippy::cast_precision_loss)]
pub fn level_meter<P: ?Sized>(
    ui: &mut egui::Ui,
    state: &PluginContext<P>,
    meter_ids: &[impl Into<u32> + Copy],
    height: f32,
) -> egui::Response {
    let meter_ids: Vec<u32> = meter_ids.iter().map(|id| (*id).into()).collect();
    let channels = meter_ids.len().max(1);
    let bar_h = height;

    // A meter reads live DSP values that change without any UI input,
    // so ask egui to keep painting. The editor's idle gate skips frames
    // egui doesn't request; without this a displayed meter would freeze
    // whenever the user stops interacting.
    ui.ctx().request_repaint();

    let total_gap = BAR_GAP * (channels as f32 - 1.0).max(0.0);
    let min_packed_w = BAR_W * channels as f32 + total_gap + BAR_PAD * 2.0;
    // When the channels fit inside `MIN_METER_W`, stay at that width
    // and stretch bars to fill (matches the pre-grow behavior for
    // mono / stereo meters). Beyond that, grow to fit `BAR_W`-wide
    // bars exactly.
    let total_w = min_packed_w.max(MIN_METER_W);

    let desired = egui::vec2(total_w, bar_h);
    let (rect, response) = ui.allocate_exact_size(desired, egui::Sense::hover());

    if ui.is_rect_visible(rect) {
        let painter = ui.painter_at(rect);

        let bar_w = ((total_w - BAR_PAD * 2.0 - total_gap) / channels as f32).max(BAR_W);
        let start_x = rect.left() + BAR_PAD;
        let bar_top = rect.top();
        let bar_bottom = bar_top + bar_h;

        for (i, &id) in meter_ids.iter().enumerate() {
            let raw = state.get_meter(id);
            let display = meter_display(raw);
            let x = start_x + i as f32 * (bar_w + BAR_GAP);

            // Track background
            let bar_rect =
                egui::Rect::from_min_size(egui::pos2(x, bar_top), egui::vec2(bar_w, bar_h));
            painter.rect_filled(bar_rect, 2.0, TRACK_BG);

            // Level fill
            if display > 0.001 {
                let level_h = bar_h * display;
                let level_rect = egui::Rect::from_min_max(
                    egui::pos2(x, bar_bottom - level_h),
                    egui::pos2(x + bar_w, bar_bottom),
                );
                let color = if display > 0.95 {
                    crate::theme::METER_CLIP
                } else {
                    crate::theme::KNOB_FILL
                };
                painter.rect_filled(level_rect, 2.0, color);
            }
        }
    }

    response
}
