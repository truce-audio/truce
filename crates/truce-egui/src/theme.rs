//! Default egui visuals matching truce-gui's dark theme.

/// Create egui `Visuals` matching truce-gui's default dark theme.
///
/// Colors are derived from `truce_gui::theme::Theme::dark()`:
/// - background: rgb(0.12, 0.12, 0.14)
/// - surface:    rgb(0.18, 0.18, 0.22)
/// - primary:    rgb(0.30, 0.60, 0.95)  (accent blue)
/// - text:       rgb(0.90, 0.90, 0.92)
/// - text_dim:   rgb(0.55, 0.55, 0.60)
pub fn dark() -> egui::Visuals {
    let mut visuals = egui::Visuals::dark();

    // Background and panels
    let bg = egui::Color32::from_rgb(31, 31, 36); // 0.12, 0.12, 0.14
    let surface = egui::Color32::from_rgb(46, 46, 56); // 0.18, 0.18, 0.22
    let primary = egui::Color32::from_rgb(77, 153, 242); // 0.30, 0.60, 0.95
    let text = egui::Color32::from_rgb(230, 230, 235); // 0.90, 0.90, 0.92
    let text_dim = egui::Color32::from_rgb(140, 140, 153); // 0.55, 0.55, 0.60

    visuals.panel_fill = bg;
    visuals.window_fill = surface;
    visuals.extreme_bg_color = egui::Color32::from_rgb(20, 20, 26);
    visuals.faint_bg_color = surface;

    // Widget styling
    visuals.widgets.noninteractive.bg_fill = surface;
    visuals.widgets.noninteractive.fg_stroke = egui::Stroke::new(1.0, text_dim);

    visuals.widgets.inactive.bg_fill = egui::Color32::from_rgb(64, 64, 77);
    visuals.widgets.inactive.fg_stroke = egui::Stroke::new(1.0, text);

    visuals.widgets.hovered.bg_fill = egui::Color32::from_rgb(77, 77, 92);
    visuals.widgets.hovered.fg_stroke = egui::Stroke::new(1.0, text);

    visuals.widgets.active.bg_fill = primary;
    visuals.widgets.active.fg_stroke = egui::Stroke::new(1.0, egui::Color32::WHITE);

    visuals.selection.bg_fill = primary.linear_multiply(0.4);
    visuals.selection.stroke = egui::Stroke::new(1.0, primary);

    visuals.override_text_color = Some(text);

    visuals
}
