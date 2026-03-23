//! Iced GUI backend for truce plugins.
//!
//! An alternative GUI backend that replaces the built-in tiny-skia/wgpu
//! renderer with [iced](https://github.com/iced-rs/iced), giving plugin
//! authors access to a full retained-mode widget toolkit.
//!
//! # Usage Modes
//!
//! ## Auto mode — zero custom code
//!
//! ```rust,ignore
//! fn editor(&mut self) -> Option<Box<dyn Editor>> {
//!     let layout = self.layout();
//!     Some(Box::new(IcedEditor::from_layout(self.params.clone(), layout)))
//! }
//! ```
//!
//! ## Custom mode — full iced control
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
pub mod editor;
pub mod editor_handle;
pub mod param_message;
pub mod param_state;
pub mod platform;
pub mod snapshot;
pub mod theme;
pub mod widgets;

// Re-export primary types for convenience.
pub use editor::{AutoPlugin, IcedEditor, IcedPlugin};
pub use editor_handle::EditorHandle;
pub use param_message::{Message, ParamMessage};
pub use param_state::ParamState;

// Re-export widget helper functions.
pub use widgets::{knob, meter, param_selector, param_slider, param_toggle, xy_pad};
