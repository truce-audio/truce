//! Lightweight GUI types for truce. No rasterization, no windowing.
//!
//! `truce-gui-types` carries the trait + data surface that GUI
//! backends (the built-in `truce-gui::BuiltinEditor`, plus
//! `truce-egui`, `truce-iced`, `truce-slint`) build on. Crates that
//! only need to *describe* layouts and react to platform-translated
//! input events depend on this crate; the heavy machinery
//! (tiny-skia, baseview, truce-font, fontdue) stays in `truce-gui`.
//!
//! The split exists so `truce-plugin` (the user-facing
//! `PluginLogic` trait crate) can name `GridLayout` /
//! `RenderBackend` / `WidgetRegion` without pulling in a software
//! rasterizer + windowing toolkit. Plugin authors who supply a
//! custom editor (egui, iced, slint, raw window handle) end up
//! transitively depending only on `truce-gui-types` instead of the
//! full `truce-gui`.

// Widget-drawing helpers, `RenderBackend` trait methods, and interaction
// dispatch all take many independent geometry / state / theme arguments.
// The long signatures are intentional; bundling them into builder
// structs would obscure call sites without simplifying any single
// call.
#![allow(clippy::too_many_arguments)]

pub mod interaction;
pub mod layout;
#[macro_use]
pub mod macros;
pub mod render;
pub mod snapshot;
pub mod theme;
pub mod widgets;

#[cfg(target_os = "ios")]
pub mod ios;

pub use render::{ImageId, RenderBackend};
pub use snapshot::ParamSnapshot;
pub use theme::Theme;

/// Convert a logical extent (in points) to physical pixels.
///
/// Standardised rounding policy across every truce GUI backend:
/// round to nearest, then clamp the result to `1` so a degenerate
/// `0 × scale` doesn't collapse a wgpu surface (`width: 0` is a
/// validation error). The `logical.max(1)` guard handles the
/// converse - a zero-logical caller can't multiply through to `0`
/// before the round.
// Logical pixel sizes are bounded by `u32::MAX / scale`; in practice
// no editor exceeds 16384 logical pixels.
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
#[inline]
#[must_use]
pub fn to_physical_px(logical: u32, scale: f64) -> u32 {
    (f64::from(logical.max(1)) * scale).round().max(1.0) as u32
}
