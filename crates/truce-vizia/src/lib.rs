//! Vizia-based GUI backend for truce audio plugins.
//!
//! Desktop-only. Vizia 0.4 is explicitly cross-platform Windows /
//! Linux / macOS - the skia + GL stack it uses has no wired-up iOS
//! bindings. iOS plugins must use the built-in `truce-gui`,
//! `truce-egui`, or `truce-slint` backend.
//!
//! Built on top of `vizia` (umbrella) with the `baseview` feature
//! enabled. The workspace `[patch]` redirects `vizia_baseview`'s
//! `baseview` dep to our `baseview-truce` fork so vizia-backed
//! plugins inherit the AAX / Pro Tools teardown fix.
//!
//! Screenshot tests work in this backend too: `Editor::screenshot`
//! drives vizia's draw pipeline against a CPU-backed Skia raster
//! surface (see `screenshot.rs`) so `cargo truce screenshot` and
//! `truce_test::screenshot!` produce pixels without an active OS
//! window or GL context.

#![cfg(not(target_os = "ios"))]

mod editor;
mod param_lens;
mod platform;
mod screenshot;
pub mod widgets;

pub use editor::{SetupFn, ViziaEditor};
pub use param_lens::ParamLens;

// Re-exports plugin authors will want.
pub use truce_core::editor::PluginContext;
pub use vizia;
