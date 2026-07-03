//! GPU rendering primitives for truce plugins.
//!
//! Provides [`WgpuBackend`], the wgpu+lyon+skrifa implementation of
//! [`truce_gui_types::RenderBackend`]. Used by the user-facing
//! editor wrappers (`truce_gui::GpuEditor`, `truce_egui::EguiEditor`,
//! `truce_iced::IcedEditor`, `truce_slint::SlintEditor`) and as the
//! GPU pipeline backing `truce_gui::default_editor`.
//!
//! Plugin authors don't depend on this crate directly - they call
//! `truce_gui::default_editor(...)` from their `PluginLogic::editor`
//! impl, which pulls `WgpuBackend` transitively.

mod backend;
pub mod platform;
#[cfg(not(target_os = "ios"))]
pub mod pump;

pub use backend::WgpuBackend;
