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
//! fn editor(&mut self) -> Option<Box<dyn Editor>> {
//!     let layout = self.layout();
//!     Some(Box::new(IcedEditor::from_layout(self.params.clone(), layout)))
//! }
//! ```
//!
//! ## Custom mode - full iced control
//!
//! ```rust,ignore
//! fn editor(&mut self) -> Option<Box<dyn Editor>> {
//!     Some(Box::new(IcedEditor::<_, MyIcedUi>::new(
//!         self.params.clone(),
//!         (600, 400),
//!     )))
//! }
//! ```

pub mod auto_layout;
#[cfg(not(target_os = "ios"))]
pub mod editor;
pub mod font;
#[cfg(not(target_os = "ios"))]
mod keyboard;
pub mod param_cache;
pub mod param_message;
#[cfg(not(target_os = "ios"))]
pub mod platform;
#[cfg(not(target_os = "ios"))]
mod screenshot;
pub mod theme;
pub mod widgets;

#[cfg(target_os = "ios")]
mod editor_ios;

// Re-export primary types for convenience.
#[cfg(not(target_os = "ios"))]
pub use editor::{AutoPlugin, IcedEditor, IcedPlugin};

#[cfg(target_os = "ios")]
pub use editor_ios::{AutoPlugin, IcedEditor, IcedPlugin};
pub use param_cache::ParamCache;
pub use param_message::{Message, ParamMessage};
// Re-export `PluginContext` so plugin authors can use it without a direct
// truce-core dependency.
pub use truce_core::editor::PluginContext;

// Re-export widget helper functions.
pub use widgets::{knob, meter, param_dropdown, param_slider, param_toggle, xy_pad};
// `#[allow(deprecated)]` so this re-export of `param_selector`
// (deprecated since 0.56 in favour of `param_dropdown`) doesn't
// fire the lint here - we're surfacing the item by design.
#[allow(deprecated)]
pub use widgets::param_selector;

use iced::Element;
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
