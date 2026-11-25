//! Font loading for egui contexts.

use std::sync::Arc;

/// Load a TrueType font as the default proportional and monospace font.
///
/// Call on an `egui::Context` before rendering. Used by both the live
/// editor and the snapshot renderer.
///
/// # Panics
///
/// Cannot panic in practice - egui's `FontDefinitions::default()`
/// always populates the `Proportional` and `Monospace` families, so
/// the `unwrap()`s succeed. A future egui release that drops either
/// family would surface as a panic here rather than a silent miss.
pub fn apply_font(ctx: &egui::Context, font_data: &'static [u8]) {
    let mut fonts = egui::FontDefinitions::default();
    fonts.font_data.insert(
        "truce_default".to_owned(),
        Arc::new(egui::FontData::from_static(font_data)),
    );
    // Symbol fallback for glyphs the primary font lacks (musical notes
    // like `♩`, misc symbols). egui 0.37 moved its own symbol fonts
    // behind an opt-in feature whose font crate isn't reliably published,
    // so without this such glyphs render as tofu. Appended last, so it
    // only fills gaps the primary (and any egui default) don't cover.
    fonts.font_data.insert(
        "truce_symbols".to_owned(),
        Arc::new(egui::FontData::from_static(truce_font::NOTO_MUSIC)),
    );
    for family in [egui::FontFamily::Proportional, egui::FontFamily::Monospace] {
        let fam = fonts.families.get_mut(&family).unwrap();
        fam.insert(0, "truce_default".to_owned());
        fam.push("truce_symbols".to_owned());
    }
    ctx.set_fonts(fonts);
}
