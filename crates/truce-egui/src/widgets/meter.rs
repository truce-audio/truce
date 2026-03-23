//! Level meter (display-only) reading truce meter values.

use crate::ParamState;

/// Show a vertical level meter reading truce meter values.
///
/// `meter_ids` are the meter IDs to display (one bar per ID). The meter
/// is display-only (no interaction). Colors change based on level:
/// green → yellow → red.
pub fn level_meter(
    ui: &mut egui::Ui,
    state: &ParamState,
    meter_ids: &[u32],
    label: &str,
) -> egui::Response {
    let bar_count = meter_ids.len().max(1) as f32;
    let bar_width = 8.0;
    let spacing = 4.0;
    let padding = 8.0;
    let label_h = 14.0;
    let bar_h = 60.0;

    let total_w = bar_count * bar_width + (bar_count - 1.0) * spacing + padding * 2.0;
    let total_h = bar_h + label_h + padding;

    let desired = egui::vec2(total_w, total_h);
    let (rect, response) = ui.allocate_exact_size(desired, egui::Sense::hover());

    if ui.is_rect_visible(rect) {
        let painter = ui.painter_at(rect);

        // Background
        painter.rect_filled(
            rect,
            4.0,
            ui.visuals().extreme_bg_color,
        );

        let bars_w = bar_count * bar_width + (bar_count - 1.0) * spacing;
        let start_x = rect.center().x - bars_w / 2.0;
        let bar_top = rect.top() + padding / 2.0;
        let bar_bottom = bar_top + bar_h;

        for (i, &id) in meter_ids.iter().enumerate() {
            let level = state.meter(id).clamp(0.0, 1.0);
            let x = start_x + i as f32 * (bar_width + spacing);

            let bar_rect = egui::Rect::from_min_size(
                egui::pos2(x, bar_top),
                egui::vec2(bar_width, bar_h),
            );

            // Track background
            painter.rect_filled(bar_rect, 2.0, egui::Color32::from_gray(35));

            // Level bar
            if level > 0.001 {
                let level_h = bar_h * level;
                let level_rect = egui::Rect::from_min_max(
                    egui::pos2(x, bar_bottom - level_h),
                    egui::pos2(x + bar_width, bar_bottom),
                );
                let color = meter_color(level);
                painter.rect_filled(level_rect, 2.0, color);
            }
        }

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

/// Color for a meter level: green → yellow → red.
fn meter_color(level: f32) -> egui::Color32 {
    if level > 0.9 {
        egui::Color32::from_rgb(255, 70, 70)
    } else if level > 0.7 {
        // Interpolate yellow
        let t = (level - 0.7) / 0.2;
        let r = (200.0 + 55.0 * t) as u8;
        let g = (200.0 - 130.0 * t) as u8;
        egui::Color32::from_rgb(r, g, 70)
    } else {
        egui::Color32::from_rgb(77, 200, 120)
    }
}
