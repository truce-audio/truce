//! Built-in GPU-free GUI for truce plugins (heavyweight runtime).
//!
//! Uses a [`truce_gui_types::RenderBackend`] trait to abstract over
//! rendering implementations. The default [`backend_cpu::CpuBackend`]
//! uses tiny-skia for software rasterization. The non-runtime data
//! types (layout, widget regions, interaction state, theme, render
//! trait, plugin-logic trait) live in `truce-gui-types` and
//! `truce-plugin`; this crate re-exports them so existing
//! `truce_gui::...` paths keep working.

// Widget-drawing helpers, `RenderBackend` trait methods, and interaction
// dispatch all take many independent geometry / state / theme arguments.
// The long signatures are intentional; bundling them into builder
// structs would obscure call sites without simplifying any single
// call.
#![allow(clippy::too_many_arguments)]

pub mod backend_cpu;
pub mod blit;
// baseview-bound editor is macOS / Windows / Linux only. iOS
// embeds the editor in a UIView managed by the AUv3 view
// controller - see [`editor_ios`].
#[cfg(not(target_os = "ios"))]
pub mod editor;
#[cfg(target_os = "ios")]
pub mod editor_ios;
#[cfg(target_os = "ios")]
pub use editor_ios as editor;
pub mod font;
pub mod interaction;
pub mod platform;
mod render_core;

// Re-export the lightweight data + trait surface from `truce-gui-types`
// so old `truce_gui::layout::*` / `truce_gui::widgets::*` /
// `truce_gui::theme::*` paths continue to resolve. New code can import
// directly from `truce_gui_types`.
#[cfg(target_os = "ios")]
pub use truce_gui_types::ios;
pub use truce_gui_types::{ImageId, ParamSnapshot, RenderBackend, Theme};
pub use truce_gui_types::{layout, render, snapshot, theme, widgets};

// Re-export plugin-logic traits from `truce-plugin` for the same
// backward-compat reason.
pub use truce_plugin::{PluginLogic, PluginLogic64, PluginLogicCore, default_hit_test};

#[doc(hidden)]
pub use truce_plugin::__plugin_logic_deps;

pub use editor::BuiltinEditor;
pub use platform::{EditorScale, to_physical_px};

/// Get the display scale factor used to size the next editor.
///
/// Screenshot rendering pins this to a deterministic value via
/// [`truce_core::screenshot::override_scale`] (default 2.0) so a
/// reference PNG baked on one host renders at the same physical
/// dimensions on any other. Outside screenshot rendering the
/// override is unset and we return the platform's main-screen DPI
/// query (Retina = 2.0, normal = 1.0).
#[must_use]
pub fn backing_scale() -> f64 {
    if let Some(s) = truce_core::screenshot::override_scale() {
        return s;
    }
    platform::main_screen_scale()
}

// ---------------------------------------------------------------------------
// tiny-skia conversions for the light `Color` type
//
// `truce-gui-types::theme::Color` doesn't pull in `tiny-skia` (the
// whole point of the split). The conversion helpers live here so
// the CPU backend and screenshot pipeline can call them without
// reimplementing the f32→u8 saturation logic at each site.
// ---------------------------------------------------------------------------

/// Extension trait giving [`truce_gui_types::theme::Color`] the
/// `to_skia` / `to_premultiplied` methods that used to live on the
/// inherent impl, now relocated here so `truce-gui-types` stays
/// rasterizer-free.
pub trait ColorExt {
    fn to_skia(&self) -> tiny_skia::Color;
    fn to_premultiplied(&self) -> tiny_skia::PremultipliedColorU8;
}

impl ColorExt for truce_gui_types::theme::Color {
    fn to_skia(&self) -> tiny_skia::Color {
        tiny_skia::Color::from_rgba(self.r, self.g, self.b, self.a)
            .unwrap_or(tiny_skia::Color::BLACK)
    }

    fn to_premultiplied(&self) -> tiny_skia::PremultipliedColorU8 {
        self.to_skia().premultiply().to_color_u8()
    }
}
