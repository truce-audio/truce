//! Font loading for iced renderers.

/// Load a TrueType font into iced's font system and return the `iced::Font`
/// to use as the renderer default.
///
/// Used by both the live editor and the snapshot renderer.
pub fn apply_font(family: &'static str, data: &'static [u8]) -> iced::Font {
    iced_graphics::text::font_system()
        .write()
        .expect("font system lock")
        .load_font(std::borrow::Cow::Borrowed(data));
    iced::Font {
        family: iced::font::Family::Name(family),
        ..iced::Font::DEFAULT
    }
}
