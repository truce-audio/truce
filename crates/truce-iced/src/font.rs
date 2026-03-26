//! Font loading for iced renderers.

/// Load a TrueType font into iced's font system and return the `iced::Font`
/// to use as the renderer default.
///
/// Used by both the live editor and the snapshot renderer.
pub fn apply_font(family: &'static str, data: &'static [u8]) -> iced::Font {
    let mut fs = iced_graphics::text::font_system()
        .write()
        .expect("font system lock");
    let v_before = fs.version();
    fs.load_font(std::borrow::Cow::Borrowed(data));
    let v_after = fs.version();
    eprintln!(
        "[truce-iced] font loaded: family={family:?}, {} bytes, version {v_before:?}->{v_after:?}",
        data.len()
    );
    drop(fs);
    iced::Font {
        family: iced::font::Family::Name(family),
        ..iced::Font::DEFAULT
    }
}
