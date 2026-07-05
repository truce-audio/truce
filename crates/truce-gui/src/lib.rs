//! Built-in GUI for truce plugins.
//!
//! Orchestrates the two `truce_gui_types::RenderBackend` impls into
//! editor types: with the default `cpu` feature, `BuiltinEditor`
//! rasterises widgets to a `truce_cpu::CpuBackend` (tiny-skia)
//! pixmap and blits it to a wgpu surface; with the `gpu` feature,
//! `GpuEditor` renders directly through `truce_gpu::WgpuBackend`.
//! The non-runtime data types (layout, widget regions, interaction
//! state, theme, render trait) live in `truce-gui-types` and the
//! plugin traits in `truce-plugin`; this crate re-exports them so
//! existing `truce_gui::...` paths keep working.

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

// `CpuBackend` (tiny-skia `RenderBackend` impl) + `font` (skrifa
// glyph cache) live in the sibling `truce-cpu` crate so the CPU
// rasterizer is a peer of `truce-gpu`'s `WgpuBackend` in the crate
// graph. Re-exported under their historical `truce_gui::*` paths
// so existing call sites keep working. Available whenever the `cpu`
// feature is on *or* we're building for iOS — iOS always rasterizes
// through `CpuBackend` (see `editor_ios`), independent of features,
// so `truce-cpu` is a hard dep there.
#[cfg(any(feature = "cpu", target_os = "ios"))]
pub use truce_cpu::ColorExt;
#[cfg(any(feature = "cpu", target_os = "ios"))]
pub use truce_cpu::CpuBackend;
#[cfg(any(feature = "cpu", target_os = "ios"))]
pub use truce_cpu::font;
// Internal sub-module path that `backend_cpu` used to occupy.
#[cfg(any(feature = "cpu", target_os = "ios"))]
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

// The editor constructors below are feature/target gated; their
// imports carry the same gate so a types-only build (neither
// renderer feature) stays warning-free.
#[cfg(any(feature = "cpu", feature = "gpu", target_os = "ios"))]
use std::sync::Arc;
#[cfg(any(feature = "cpu", feature = "gpu", target_os = "ios"))]
use truce_core::editor::Editor;
use truce_core::screenshot::override_scale;
#[cfg(any(feature = "cpu", feature = "gpu", target_os = "ios"))]
use truce_gui_types::layout::GridLayout;
#[cfg(any(feature = "cpu", feature = "gpu", target_os = "ios"))]
use truce_params::Params;

// Re-export plugin-logic traits from `truce-plugin` for the same
// backward-compat reason.
pub use truce_plugin::{PluginLogic, PluginLogic64, PluginLogicCore, default_hit_test};

#[doc(hidden)]
pub use truce_plugin::__plugin_logic_deps;

pub use editor::BuiltinEditor;
#[cfg(all(feature = "gpu", not(target_os = "ios")))]
pub use gpu_editor::GpuEditor;
pub use platform::{EditorScale, PaintPacer, to_physical_px};

/// Construct truce's default editor for a plugin's `editor()` impl.
///
/// Picks the renderer based on which feature is enabled:
///
/// - `gpu` (opt-in): wraps a [`BuiltinEditor`] in a `GpuEditor`
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
/// fn editor(params: Arc<MyParams>) -> Box<dyn truce_core::Editor> {
///     truce_gui::default_editor(
///         params,
///         GridLayout::build(vec![ /* widgets */ ]),
///     )
/// }
/// ```
///
/// Only compiled when a renderer feature (`cpu` or `gpu`) is on.
/// Crates that depend on `truce-gui` purely for its types / platform
/// helpers (e.g. `truce-egui`, `truce-iced`, `truce-slint`) can build
/// it with neither feature and simply not call this function.
#[cfg(any(feature = "cpu", feature = "gpu", target_os = "ios"))]
#[must_use]
pub fn default_editor<P: Params + 'static>(params: Arc<P>, layout: GridLayout) -> Box<dyn Editor> {
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
}

/// Fluent shorthand for [`default_editor`]. Build a `GridLayout`,
/// then close the `editor()` impl with `.into_editor(&params)`:
///
/// ```ignore
/// fn editor(params: Arc<MyParams>) -> Box<dyn truce_core::Editor> {
///     GridLayout::build(vec![ /* widgets */ ])
///         .with_title("GAIN")
///         .into_editor(&params)
/// }
/// ```
///
/// Equivalent to `default_editor(params, layout)` - the
/// `&Arc<P>` is cloned internally so the call site stays free of an
/// explicit `.clone()`. Bring it into scope with
/// `use truce_gui::IntoLayoutEditor;` (it can't ride along on
/// `truce::prelude`, which deliberately doesn't depend on this crate).
///
/// The method name mirrors [`truce_core::IntoEditor::into_editor`] (the
/// blanket "box a concrete editor" helper used by egui / iced / slint),
/// so every `editor()` impl ends the same way - layout plugins just
/// pass their params.
///
/// Same feature gating as [`default_editor`]: only compiled when a
/// renderer feature (`cpu` / `gpu`) is on, or on iOS.
#[cfg(any(feature = "cpu", feature = "gpu", target_os = "ios"))]
pub trait IntoLayoutEditor {
    /// Wrap this layout in truce's default editor, picking the
    /// renderer from the active `truce-gui` feature. See
    /// [`default_editor`].
    fn into_editor<P: Params + 'static>(self, params: &Arc<P>) -> Box<dyn Editor>;
}

#[cfg(any(feature = "cpu", feature = "gpu", target_os = "ios"))]
impl IntoLayoutEditor for GridLayout {
    fn into_editor<P: Params + 'static>(self, params: &Arc<P>) -> Box<dyn Editor> {
        default_editor(params.clone(), self)
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
    if let Some(s) = override_scale() {
        return s;
    }
    platform::main_screen_scale()
}
