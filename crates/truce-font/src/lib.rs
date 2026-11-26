//! Bundled fonts for the truce audio plugin framework.
//!
//! Currently exposes a single font - `JetBrains` Mono Regular - as a
//! `&'static [u8]` of the TTF bytes, suitable for hand-off to
//! `egui::FontData::from_static`, `iced::Font` (via the
//! `with_font` helpers on each editor backend), or any other font
//! consumer that takes raw bytes.
//!
//! ## License
//!
//! `JetBrains` Mono is distributed under the SIL Open Font License,
//! Version 1.1. The full license text is included in the source tree
//! at `fonts/OFL.txt` and is also bundled into the published crate
//! tarball. The font's embedded copyright notice
//! ("Copyright 2020 The `JetBrains` Mono Project Authors") is preserved
//! inside the TTF.
//!
//! Noto Music (the symbol fallback, [`NOTO_MUSIC`]) is under the same
//! license with its own notice, shipped as `fonts/NotoMusic-OFL.txt`.
//!
//! Downstream redistribution must keep each font's copyright notice
//! intact and ship a copy of its OFL.

/// `JetBrains` Mono Regular as raw TTF bytes. Suitable for embedding in
/// a plugin GUI as the fallback / canonical truce monospace font.
///
/// The full license text is at `fonts/OFL.txt` in the crate source
/// (also packaged into the crates.io tarball via the manifest's
/// `include` list).
pub static JETBRAINS_MONO: &[u8] = include_bytes!("../fonts/JetBrainsMono-Regular.ttf");

/// Noto Music Regular as raw TTF bytes - a small (~80 KB) symbol fallback
/// covering musical and miscellaneous-symbol glyphs (e.g. `♩` U+2669) that
/// a plug-in UI may use but that `JetBrains` Mono lacks. egui 0.37 ships
/// its own fonts only behind its `default_fonts` / `monochrome_emoji_fonts`
/// features, which truce-egui leaves off, so truce ships this as the
/// fallback for GUI backends. It doesn't cover emoji or egui's icon glyphs.
///
/// Noto Music is licensed under the SIL Open Font License v1.1 (the same
/// license as `JetBrains` Mono, with its own copyright notice); its text is
/// at `fonts/NotoMusic-OFL.txt`, also packaged via the manifest's
/// `include` list.
pub static NOTO_MUSIC: &[u8] = include_bytes!("../fonts/NotoMusic-Regular.ttf");

/// Glyph rasterization over the bundled font. Behind the `raster`
/// feature so byte-only consumers don't pull skrifa / tiny-skia.
#[cfg(feature = "raster")]
pub mod raster;
