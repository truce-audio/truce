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
//! use truce_egui::{EguiEditor, ParamState};
//! use truce_egui::widgets::{param_knob, param_slider};
//!
//! let editor = EguiEditor::new((800, 600), |ctx: &egui::Context, state: &ParamState| {
//!     egui::CentralPanel::default().show(ctx, |ui| {
//!         ui.heading("My Plugin");
//!         param_slider(ui, state, 0);
//!     });
//! });
//! ```

pub mod editor;
pub mod font;
pub mod param_state;
pub mod platform;
pub mod renderer;
pub mod snapshot;
pub mod theme;
pub mod widgets;

pub use editor::{EditorUi, EguiEditor};
pub use param_state::ParamState;
