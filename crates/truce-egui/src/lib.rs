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
#[cfg(not(target_os = "ios"))]
pub mod editor;
pub mod font;
pub mod platform;
pub mod renderer;
#[cfg(not(target_os = "ios"))]
mod screenshot;
pub mod theme;
pub mod widgets;

#[cfg(target_os = "ios")]
mod editor_ios;

#[cfg(not(target_os = "ios"))]
pub use editor::{EditorUi, EguiEditor};

#[cfg(target_os = "ios")]
pub use editor_ios::{EditorUi, EguiEditor};

fn actual_window_size_id() -> egui::Id {
    egui::Id::new("truce_egui_actual_window_size")
}

/// Stash the true OS window size (logical points) in egui ctx data so a
/// plugin's `ui()` can read it back via [`actual_window_size`].
///
/// This exists because the editor feeds `screen_rect = self.size / zoom`
/// to egui (so layout is consistent when a plugin applies
/// `ctx.set_zoom_factor`). A plugin that scales its UI from the window size
/// therefore cannot recover the real window size from `ctx.screen_rect()`
/// alone — egui also transiently rescales `screen_rect` on zoom-change
/// frames. Reading this out-of-band value avoids that resize feedback loop.
pub fn set_actual_window_size(ctx: &egui::Context, size: (u32, u32)) {
    ctx.data_mut(|data| data.insert_temp(actual_window_size_id(), size));
}

/// The true OS window size (logical points) as stored by the editor each
/// frame. See [`set_actual_window_size`]. Returns `None` before the first
/// frame has run.
#[must_use]
pub fn actual_window_size(ctx: &egui::Context) -> Option<(u32, u32)> {
    ctx.data(|data| data.get_temp(actual_window_size_id()))
}
