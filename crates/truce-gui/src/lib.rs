//! Built-in GPU-free GUI for truce plugins.
//!
//! Uses a `RenderBackend` trait to abstract over rendering implementations.
//! The default `CpuBackend` uses tiny-skia for software rasterization.

pub mod backend_cpu;
pub mod blit;
pub mod cg_blit;
pub mod editor;
pub mod font;
pub mod interaction;
pub mod layout;
#[macro_use]
pub mod macros;
pub mod native_view;
pub mod platform;
pub mod render;
pub mod theme;
pub mod widgets;

pub use editor::BuiltinEditor;
pub use render::RenderBackend;
pub use theme::Theme;

/// Get the platform's display scale factor (Retina = 2.0, normal = 1.0).
pub fn backing_scale() -> f64 {
    platform::main_screen_scale()
}
