//! GPU rendering backend for truce plugins.
//!
//! Uses wgpu (Metal/DX12/Vulkan) with lyon tessellation and fontdue
//! glyph atlas. Implements `truce_gui::RenderBackend` so widgets
//! render identically to the CPU path.
//!
//! Platform windowing is provided by baseview.

mod backend;
pub mod editor;
pub mod platform;
pub mod snapshot;

pub use backend::WgpuBackend;
pub use editor::GpuEditor;
