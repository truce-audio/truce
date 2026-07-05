//! Iced GUI backend for truce plugins.
//!
//! An alternative GUI backend that replaces the built-in tiny-skia/wgpu
//! renderer with [iced](https://github.com/iced-rs/iced), giving plugin
//! authors access to a full retained-mode widget toolkit.
//!
//! # Usage Modes
//!
//! ## Auto mode - zero custom code
//!
//! ```rust,ignore
//! fn editor(params: Arc<MyParams>) -> Box<dyn Editor> {
//!     IcedEditor::from_layout(params, layout()).into_editor()
//! }
//! ```
//!
//! ## Custom mode - full iced control
//!
//! ```rust,ignore
//! fn editor(params: Arc<MyParams>) -> Box<dyn Editor> {
//!     IcedEditor::<_, MyIcedUi>::new(params, (600, 400)).into_editor()
//! }
//! ```

pub mod auto_layout;
#[cfg(not(target_os = "ios"))]
pub mod editor;
pub mod font;
// Facade re-assembling the `iced` umbrella's API from its sub-crates, so
// the crate never depends on `iced_winit` (which can't build for iOS).
// Public so plugin crates can reach iced types without the umbrella too.
pub mod iced;
#[cfg(not(target_os = "ios"))]
mod keyboard;
pub mod param_cache;
pub mod param_message;
#[cfg(not(target_os = "ios"))]
pub mod platform;
// Surface pump: owns the wgpu surface + every blocking swapchain call
// (threaded on Windows, inline elsewhere). Per-wgpu-version copy of
// `truce_gpu::pump`.
#[cfg(not(target_os = "ios"))]
mod pump;
// Surface-agnostic iced render pipeline (`IcedRuntime`/`RenderState`/
// `IcedProgram`) shared by the desktop and iOS editors.
mod runtime;
#[cfg(not(target_os = "ios"))]
mod screenshot;
pub mod theme;
pub mod widgets;

#[cfg(target_os = "ios")]
mod editor_ios;

// Re-export primary types for convenience. The plugin-facing traits live
// in the shared `runtime` module (all platforms); only the windowing
// `IcedEditor` differs per platform.
pub use runtime::{AutoPlugin, IcedPlugin};

#[cfg(not(target_os = "ios"))]
pub use editor::IcedEditor;

#[cfg(target_os = "ios")]
pub use editor_ios::IcedEditor;
pub use param_cache::ParamCache;
pub use param_message::{Message, ParamMessage};
// Re-export `PluginContext` so plugin authors can use it without a direct
// truce-core dependency.
pub use truce_core::editor::PluginContext;

// Re-export widget helper functions.
pub use widgets::{knob, meter, param_dropdown, param_slider, param_toggle, xy_pad};

use crate::iced::Element;
use std::fmt::Debug;

/// Convert any truce-iced widget into an `Element` with `.el()`.
///
/// Avoids the verbose `Into::<Element<'a, Message<M>>>::into(...)` pattern.
///
/// ```ignore
/// Row::new()
///     .push(knob(P::Gain, params).label("Gain").el())
///     .push(knob(P::Pan, params).label("Pan").el())
/// ```
pub trait IntoElement<'a, M: Clone + Debug + 'static> {
    fn el(self) -> Element<'a, Message<M>>;
}

impl<'a, M: Clone + Debug + 'static, T: Into<Element<'a, Message<M>>>> IntoElement<'a, M> for T {
    fn el(self) -> Element<'a, Message<M>> {
        self.into()
    }
}
