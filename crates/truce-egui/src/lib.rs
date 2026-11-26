//! egui-based GUI backend for truce audio plugins.
//!
//! Provides `EguiEditor`, an implementation of `truce_core::Editor` that
//! renders using egui's immediate-mode UI via egui-wgpu. Gives plugin
//! developers access to egui's full widget library, layout system, and
//! ecosystem while retaining truce's parameter binding and host integration.
//!
//! # Quick Start
//!
//! ```ignore
//! use truce_egui::EguiEditor;
//! use truce_egui::widgets::{param_knob, param_slider};
//! use truce_core::editor::PluginContext;
//!
//! let editor = EguiEditor::new(params, (800, 600), |ui: &mut egui::Ui, state: &PluginContext<MyParams>| {
//!     ui.heading("My Plugin");
//!     param_slider(ui, state, 0u32);
//! });
//! ```

// `editor.rs` is the baseview-driven desktop path; `editor_ios.rs`
// drives the UIView + CADisplayLink + CAMetalLayer host on iOS.
// `renderer.rs` (egui-wgpu wrapper) is shared - it has both a
// baseview-window and a raw-CAMetalLayer constructor.
mod claims;
#[cfg(not(target_os = "ios"))]
pub mod editor;
pub mod font;
pub mod platform;
#[cfg(target_os = "windows")]
mod render_thread;
pub mod renderer;
#[cfg(not(target_os = "ios"))]
mod screenshot;
pub mod theme;
pub mod widgets;

#[cfg(target_os = "ios")]
mod editor_ios;

pub use claims::{claim_keys, claim_shortcuts};
#[cfg(not(target_os = "ios"))]
pub use editor::{EditorUi, EguiEditor};

#[cfg(target_os = "ios")]
pub use editor_ios::{EditorUi, EguiEditor};

/// A fresh egui context for a live editor. egui's keyboard zoom
/// (Cmd/Ctrl with `+` / `-` / `=` / `0`) is off: it rescales the UI's
/// points-per-pixel inside a window whose size the host owns, so the
/// content clips or offsets until reset - and those chords belong to the
/// host anyway unless the editor claims them.
pub(crate) fn new_egui_context() -> egui::Context {
    let ctx = egui::Context::default();
    ctx.options_mut(|o| o.zoom_with_keyboard = false);
    ctx
}
