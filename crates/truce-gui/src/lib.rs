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

// `blit` (CPU pixmap → wgpu surface upload) is only needed for the
// cpu path. Gated together with `truce-cpu`.
#[cfg(feature = "cpu")]
pub mod blit;
// baseview-bound editor is macOS / Windows / Linux only. iOS
// embeds the editor in a UIView managed by the AUv3 view
// controller - see [`editor_ios`]. `BuiltinEditor` itself stays
// available regardless of the `cpu` feature so the wgpu-only
// `GpuEditor` can wrap it; the cpu-specific fields and Editor
// trait impl inside this module are individually gated.
#[cfg(not(target_os = "ios"))]
pub mod editor;
#[cfg(target_os = "ios")]
pub mod editor_ios;
#[cfg(target_os = "ios")]
pub use editor_ios as editor;
// The wgpu-backed editor wraps `BuiltinEditor` to render through
// `truce_gpu::WgpuBackend`. Lives here so the user-facing renderer
// crate (truce-gui) is a one-stop dep for plugin authors; the wgpu
// primitives stay an implementation detail in truce-gpu.
#[cfg(all(feature = "gpu", not(target_os = "ios")))]
pub mod gpu_editor;
pub mod interaction;
pub mod platform;
mod render_core;

// `CpuBackend` (tiny-skia `RenderBackend` impl) + `font` (fontdue
// glyph cache) live in the sibling `truce-cpu` crate so the CPU
// rasterizer is a peer of `truce-gpu`'s `WgpuBackend` in the crate
// graph. Re-exported under their historical `truce_gui::*` paths
// when `cpu` is enabled so existing call sites keep working.
#[cfg(feature = "cpu")]
pub use truce_cpu::ColorExt;
#[cfg(feature = "cpu")]
pub use truce_cpu::CpuBackend;
#[cfg(feature = "cpu")]
pub use truce_cpu::font;
// Internal sub-module path that `backend_cpu` used to occupy.
#[cfg(feature = "cpu")]
#[doc(hidden)]
pub mod backend_cpu {
    pub use truce_cpu::CpuBackend;
}

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
#[cfg(all(feature = "gpu", not(target_os = "ios")))]
pub use gpu_editor::GpuEditor;
pub use platform::{EditorScale, to_physical_px};

/// Construct truce's default editor for a plugin's `editor()` impl.
///
/// Picks the renderer based on which feature is enabled:
///
/// - `gpu` (opt-in): wraps a [`BuiltinEditor`] in a [`GpuEditor`]
///   that renders directly through `truce_gpu::WgpuBackend`.
/// - `cpu` (default): returns a [`BuiltinEditor`] whose `Editor`
///   impl rasterises to a tiny-skia pixmap and blits it to a wgpu
///   surface.
/// - iOS: always returns the iOS `BuiltinEditor` (UIView-hosted
///   `CAMetalLayer`); the `gpu` feature has no effect on iOS.
///
/// Most layout-only plugins implement [`truce_plugin::PluginLogic::editor`] as:
///
/// ```ignore
/// fn editor(&self) -> Box<dyn truce_core::Editor> {
///     truce_gui::default_editor(
///         self.params.clone(),
///         GridLayout::build(vec![ /* widgets */ ]),
///     )
/// }
/// ```
#[must_use]
pub fn default_editor<P: truce_params::Params + 'static>(
    params: std::sync::Arc<P>,
    layout: truce_gui_types::layout::GridLayout,
) -> Box<dyn truce_core::editor::Editor> {
    let builtin = BuiltinEditor::new_grid(params, layout);
    #[cfg(target_os = "ios")]
    {
        Box::new(builtin)
    }
    #[cfg(all(feature = "gpu", not(target_os = "ios")))]
    {
        Box::new(GpuEditor::new(builtin))
    }
    #[cfg(all(feature = "cpu", not(feature = "gpu"), not(target_os = "ios")))]
    {
        Box::new(builtin)
    }
    #[cfg(all(not(feature = "cpu"), not(feature = "gpu"), not(target_os = "ios")))]
    {
        let _ = builtin;
        compile_error!(
            "truce-gui needs at least one renderer feature: enable `cpu` (default) or `gpu`"
        );
    }
}

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
