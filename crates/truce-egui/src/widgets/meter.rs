//! Level meter (display-only) reading truce meter values.

use truce_core::meter_display;

use crate::ParamState;

/// Show a vertical level meter reading truce meter values.
///
/// `meter_ids` are the meter IDs to display (one bar per ID). The meter
/// is display-only (no interaction). `height` sets the bar height in
/// pixels. Colors change based on level: blue normally, red when clipping.
/// Default meter width in pixels.
const METER_W: f32 = 16.0;
const BAR_GAP: f32 = 2.0;
const BAR_PAD: f32 = 0.0;
const TRACK_BG: egui::Color32 = egui::Color32::from_rgb(42, 42, 48);

pub fn level_meter(
    ui: &mut egui::Ui,
    state: &ParamState,
    meter_ids: &[impl Into<u32> + Copy],
    height: f32,
) -> egui::Response {
    let meter_ids: Vec<u32> = meter_ids.iter().map(|id| (*id).into()).collect();
    let channels = meter_ids.len().max(1);
    let bar_h = height;

    let desired = egui::vec2(METER_W, bar_h);
    let (rect, response) = ui.allocate_exact_size(desired, egui::Sense::hover());

    if ui.is_rect_visible(rect) {
        let painter = ui.painter_at(rect);

        let total_gap = BAR_GAP * (channels as f32 - 1.0).max(0.0);
        let bar_w = ((METER_W - BAR_PAD * 2.0 - total_gap) / channels as f32).max(4.0);
        let start_x = rect.left() + BAR_PAD;
        let bar_top = rect.top();
        let bar_bottom = bar_top + bar_h;

        for (i, &id) in meter_ids.iter().enumerate() {
            let raw = state.meter(id);
            let display = meter_display(raw);
            let x = start_x + i as f32 * (bar_w + BAR_GAP);

            // Track background
            let bar_rect = egui::Rect::from_min_size(
                egui::pos2(x, bar_top),
                egui::vec2(bar_w, bar_h),
            );
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

