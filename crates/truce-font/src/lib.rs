//! Bundled fonts for the truce audio plugin framework.
//!
//! Currently exposes a single font — JetBrains Mono Regular — as a
//! `&'static [u8]` of the TTF bytes, suitable for hand-off to
//! `fontdue`, `egui::FontData::from_static`, `iced::Font` (via the
//! `with_font` helpers on each editor backend), or any other font
//! consumer that takes raw bytes.
//!
//! ## License
//!
//! JetBrains Mono is distributed under the SIL Open Font License,
//! Version 1.1. The full license text is included in the source tree
//! at `fonts/OFL.txt` and is also bundled into the published crate
//! tarball. The font's embedded copyright notice
//! ("Copyright 2020 The JetBrains Mono Project Authors") is preserved
//! inside the TTF.
//!
//! Downstream redistribution must keep the font's copyright notice
//! intact and ship a copy of the OFL.

/// JetBrains Mono Regular as raw TTF bytes. Suitable for embedding in
/// a plugin GUI as the fallback / canonical truce monospace font.
///
/// The full license text is at `fonts/OFL.txt` in the crate source
/// (also packaged into the crates.io tarball via the manifest's
/// `include` list).
pub static JETBRAINS_MONO: &[u8] = include_bytes!("../fonts/JetBrainsMono-Regular.ttf");
