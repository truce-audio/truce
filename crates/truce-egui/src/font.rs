//! Font loading for egui contexts.

use std::sync::Arc;

/// Load a TrueType font as the default proportional and monospace font.
///
/// Call on an `egui::Context` before rendering. Used by both the live
/// editor and the snapshot renderer.
pub fn apply_font(ctx: &egui::Context, font_data: &'static [u8]) {
    let mut fonts = egui::FontDefinitions::default();
    fonts.font_data.insert(
        "truce_default".to_owned(),
        Arc::new(egui::FontData::from_static(font_data)),
    );
    for family in [egui::FontFamily::Proportional, egui::FontFamily::Monospace] {
        fonts
            .families
            .get_mut(&family)
            .unwrap()
            .insert(0, "truce_default".to_owned());
    }
    ctx.set_fonts(fonts);
}
