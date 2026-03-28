//! Default egui visuals and color constants matching truce-gui's dark theme.

use egui::Color32;

// Core palette — matches truce_gui::theme::Theme::dark()
pub const BACKGROUND: Color32 = Color32::from_rgb(31, 31, 36);
pub const SURFACE: Color32 = Color32::from_rgb(46, 46, 56);
pub const PRIMARY: Color32 = Color32::from_rgb(77, 153, 242);
pub const TEXT: Color32 = Color32::from_rgb(230, 230, 235);
pub const TEXT_DIM: Color32 = Color32::from_rgb(140, 140, 153);
pub const HEADER_BG: Color32 = Color32::from_rgb(20, 20, 26);
pub const HEADER_TEXT: Color32 = Color32::from_rgb(191, 191, 200);
pub const KNOB_TRACK: Color32 = Color32::from_rgb(64, 64, 77);
pub const KNOB_FILL: Color32 = Color32::from_rgb(77, 153, 242);
pub const METER_CLIP: Color32 = Color32::from_rgb(224, 69, 69);

/// Create egui `Visuals` matching truce-gui's default dark theme.
pub fn dark() -> egui::Visuals {
    let mut visuals = egui::Visuals::dark();

    visuals.panel_fill = BACKGROUND;
    visuals.window_fill = SURFACE;
    visuals.extreme_bg_color = HEADER_BG;
    visuals.faint_bg_color = SURFACE;

    visuals.widgets.noninteractive.bg_fill = SURFACE;
    visuals.widgets.noninteractive.fg_stroke = egui::Stroke::new(1.0, TEXT_DIM);

    visuals.widgets.inactive.bg_fill = KNOB_TRACK;
    visuals.widgets.inactive.fg_stroke = egui::Stroke::new(1.0, TEXT);

    visuals.widgets.hovered.bg_fill = Color32::from_rgb(77, 77, 92);
    visuals.widgets.hovered.fg_stroke = egui::Stroke::new(1.0, TEXT);

    visuals.widgets.active.bg_fill = PRIMARY;
    visuals.widgets.active.fg_stroke = egui::Stroke::new(1.0, Color32::WHITE);

    visuals.selection.bg_fill = PRIMARY.linear_multiply(0.4);
    visuals.selection.stroke = egui::Stroke::new(1.0, PRIMARY);

    visuals.override_text_color = Some(TEXT);

    visuals
}
