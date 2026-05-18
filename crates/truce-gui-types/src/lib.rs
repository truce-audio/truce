//! Lightweight GUI types for truce. No rasterization, no windowing.
//!
//! `truce-gui-types` carries the trait + data surface that GUI
//! backends (the built-in `truce-gui::BuiltinEditor`, plus
//! `truce-egui`, `truce-iced`, `truce-slint`) build on. Crates that
//! only need to *describe* layouts and react to platform-translated
//! input events depend on this crate; the heavy machinery
//! (tiny-skia, baseview, truce-font, fontdue) stays in `truce-gui`.
//!
//! The split exists so `truce-plugin` (the user-facing
//! `PluginLogic` trait crate) can name `GridLayout` /
//! `RenderBackend` / `WidgetRegion` without pulling in a software
//! rasterizer + windowing toolkit. Plugin authors who supply a
//! custom editor (egui, iced, slint, raw window handle) end up
//! transitively depending only on `truce-gui-types` instead of the
//! full `truce-gui`.

// Widget-drawing helpers, `RenderBackend` trait methods, and interaction
// dispatch all take many independent geometry / state / theme arguments.
// Bundling them into builder structs is a worthwhile refactor but a
// separate change - for now the long signatures are intentional.
#![allow(clippy::too_many_arguments)]

pub mod interaction;
pub mod layout;
#[macro_use]
pub mod macros;
pub mod render;
pub mod snapshot;
pub mod theme;
pub mod widgets;

pub use render::{ImageId, RenderBackend};
pub use snapshot::ParamSnapshot;
pub use theme::Theme;
