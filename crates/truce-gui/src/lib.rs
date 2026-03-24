//! Built-in GPU-free GUI for truce plugins.
//!
//! Uses a `RenderBackend` trait to abstract over rendering implementations.
//! The default `CpuBackend` uses tiny-skia for software rasterization.

pub mod backend_cpu;
pub mod editor;
pub mod font;
pub mod interaction;
pub mod layout;
#[macro_use]
pub mod macros;
pub mod platform;
pub mod render;
pub mod theme;
pub mod widgets;

pub use editor::BuiltinEditor;
pub use render::RenderBackend;
pub use theme::Theme;

/// Get the platform's display scale factor (Retina = 2.0, normal = 1.0).
/// Also available via `Editor::scale_factor()` on `BuiltinEditor`.
#[cfg(target_os = "macos")]
pub fn backing_scale() -> f64 {
    unsafe { platform::truce_platform_backing_scale() }
}

#[cfg(not(target_os = "macos"))]
pub fn backing_scale() -> f64 {
    1.0
}
