//! Slint GUI backend for truce audio plugins.
//!
//! Provides `SlintEditor`, which implements `truce_core::Editor` using
//! Slint's software renderer + baseview + wgpu. Developers write their UI
//! in `.slint` markup (compiled at build time) and wire parameters through
//! `ParamState`.
//!
//! # Usage
//!
//! ```ignore
//! use truce_slint::{SlintEditor, ParamState};
//!
//! // In your plugin's custom_editor():
//! SlintEditor::new((400, 300), |state: ParamState| {
//!     let ui = MyPluginUi::new().unwrap();
//!
//!     // UI → host
//!     let s = state.clone();
//!     ui.on_gain_changed(move |v| s.set_immediate(0, v as f64));
//!
//!     // host → UI (returned closure called each frame)
//!     Box::new(move |state: &ParamState| {
//!         ui.set_gain(state.get(0) as f32);
//!     })
//! })
//! ```

pub mod blit;
pub mod editor;
pub mod param_state;
pub mod platform;

pub use editor::SlintEditor;
pub use param_state::ParamState;

// Re-export slint so plugin authors can use it without a direct dependency.
pub use slint;
