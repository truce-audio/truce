//! Font loading for iced renderers.

/// Load a TrueType font into iced's font system and return the
/// `crate::iced::Font` to use as the renderer default.
///
/// The font's family name is read from its TTF `name` table - iced
/// (cosmic-text underneath) keys loaded fonts by family name, so the
/// renderer needs that string to refer to the just-loaded bytes.
/// Mirrors the `with_font(bytes)` shape of truce-egui and truce-vizia
/// where the family is also inferred from the font data.
///
/// Used by both the live editor and the snapshot renderer.
///
/// # Panics
///
/// Panics if iced's process-wide font-system `RwLock` is poisoned -
/// in normal operation no holder panics while writing, so this is a
/// recovery-impossible condition rather than a runtime contract.
///
/// Returns `crate::iced::Font::DEFAULT` if the bytes don't parse as a TTF
/// or carry no usable `name` record; iced still loads the bytes (so
/// a downstream `Font::with_name(family)` would still resolve) but
/// the renderer falls back to its default family.
#[must_use]
pub fn apply_font(data: &'static [u8]) -> crate::iced::Font {
    let family = extract_family_name(data);

    let mut fs = iced_graphics::text::font_system()
        .write()
        .expect("font system lock");
    let v_before = fs.version();
    fs.load_font(std::borrow::Cow::Borrowed(data));
    let v_after = fs.version();
    log::debug!(
        "[truce-iced] font loaded: family={family:?}, {} bytes, version {v_before:?}->{v_after:?}",
        data.len()
    );
    drop(fs);

    match family {
        Some(name) => crate::iced::Font {
            // `crate::iced::font::Family::Name` wants a `&'static str`. The
            // caller hands us `&'static [u8]` so the bytes outlive
            // the editor; the leaked `String` is one allocation per
            // `with_font` call (typically once per plugin instance,
            // never inside `process()`).
            family: crate::iced::font::Family::Name(Box::leak(name.into_boxed_str())),
            ..crate::iced::Font::DEFAULT
        },
        None => crate::iced::Font::DEFAULT,
    }
}

/// Extract the typographic family name (name ID 16) from a TTF/OTF
/// `name` table, falling back to the legacy family name (name ID 1)
/// when the typographic record is absent. Returns `None` if the bytes
/// aren't a recognisable font or carry no usable name record.
fn extract_family_name(data: &[u8]) -> Option<String> {
    let face = ttf_parser::Face::parse(data, 0).ok()?;
    let names = face.names();
    // Typographic family (16) preferred when present - it carries
    // the "real" family name on fonts that split into multiple
    // legacy 4-style sub-families (Regular / Bold / Italic /
    // BoldItalic). Falls back to Family (1) which every font has.
    for id in [16u16, 1] {
        for i in 0..names.len() {
            let n = names.get(i)?;
            if n.name_id == id
                && let Some(s) = n.to_string()
            {
                return Some(s);
            }
        }
    }
    None
}
