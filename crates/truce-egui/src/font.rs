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
    // like `♩`, misc symbols). egui 0.37 ships its fonts only behind its
    // `default_fonts` feature (emoji additionally behind
    // `monochrome_emoji_fonts`), which truce-egui leaves off, so without
    // this such glyphs render as tofu. Appended last, so it only fills
    // gaps the primary and any egui fonts a plug-in opted into don't cover.
    // Noto Music covers music symbols, not emoji or egui's icon glyphs -
    // a plug-in that uses those enables egui's font features itself.
    //
    // Tweaked to sit with the primary text: Noto Music draws its notes
    // small within the em (`♩` is ~0.6 cap height at scale 1) and its tall,
    // bottom-light vertical metrics put the glyph ~2 pt under the
    // primary's baseline at 13 pt. Scale 1.4 and a lift of 0.2 of the
    // scaled size (0.28 em) bring `♩` to cap height on the baseline. Row
    // heights come from the primary face, so they're untouched; a symbol's
    // advance does grow 1.4x with the scale.
    fonts.font_data.insert(
        "truce_symbols".to_owned(),
        Arc::new(
            egui::FontData::from_static(truce_font::NOTO_MUSIC).tweak(egui::FontTweak {
                scale: 1.4,
                y_offset_factor: -0.2,
                ..Default::default()
            }),
        ),
    );
    for family in [egui::FontFamily::Proportional, egui::FontFamily::Monospace] {
        let fam = fonts.families.get_mut(&family).unwrap();
        fam.insert(0, "truce_default".to_owned());
        fam.push("truce_symbols".to_owned());
    }
    ctx.set_fonts(fonts);
}

/// Load the editor's fonts: `font_data` if the editor set one with
/// `with_font`, else [`truce_font::JETBRAINS_MONO`]. Either way the Noto
/// Music symbol fallback comes along (see [`apply_font`]).
///
/// egui 0.37 ships no fonts unless its `default_fonts` feature is on
/// (truce-egui leaves it off), so an editor that applied nothing - the
/// 6.x behaviour without `with_font` - would draw every glyph as tofu.
pub(crate) fn apply_editor_font(ctx: &egui::Context, font_data: Option<&'static [u8]>) {
    apply_font(ctx, font_data.unwrap_or(truce_font::JETBRAINS_MONO));
}

#[cfg(test)]
mod tests {
    /// An editor that never called `with_font` must still draw text: egui
    /// 0.37 ships no fonts of its own here (`default_fonts` off), so
    /// without the fallback every glyph is tofu.
    #[test]
    fn editor_without_with_font_has_glyphs() {
        let ctx = egui::Context::default();
        super::apply_editor_font(&ctx, None);
        let mut out = ctx.run_ui(egui::RawInput::default(), |_| {});
        out.textures_delta.clear();
        let has = |t: &str| ctx.fonts_mut(|f| f.has_glyphs(&egui::FontId::proportional(14.0), t));
        assert!(has("Gain 0.0 dB"), "default font missing");
        assert!(has("\u{2669}"), "Noto Music fallback missing");
        let mut out = ctx.run_ui(egui::RawInput::default(), |_| {});
        out.textures_delta.clear();
    }
}
