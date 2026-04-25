//! Built-in GPU-free GUI for truce plugins.
//!
//! Uses a `RenderBackend` trait to abstract over rendering implementations.
//! The default `CpuBackend` uses tiny-skia for software rasterization.

// Widget-drawing helpers, `RenderBackend` trait methods, and interaction
// dispatch all take many independent geometry / state / theme arguments.
// Bundling them into builder structs is a worthwhile refactor but a
// separate change — for now the long signatures are intentional.
#![allow(clippy::too_many_arguments)]

pub mod backend_cpu;
pub mod blit;
pub mod editor;
pub mod font;
pub mod interaction;
pub mod layout;
#[macro_use]
pub mod macros;
pub mod platform;
pub mod render;
pub mod snapshot;
pub mod theme;
pub mod widgets;

pub use editor::BuiltinEditor;
pub use render::{ImageId, RenderBackend};
pub use snapshot::ParamSnapshot;
pub use theme::Theme;

/// Get the platform's display scale factor (Retina = 2.0, normal = 1.0).
pub fn backing_scale() -> f64 {
    platform::main_screen_scale()
}
