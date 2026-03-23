// Suppress warnings from the `objc` 0.2 crate's macros (used in query_backing_scale).
#![allow(unexpected_cfgs)]

//! Vizia GUI backend for truce audio plugins.
//!
//! Provides `ViziaEditor`, which implements `truce_core::Editor` using
//! vizia + baseview for embedded plugin GUIs. Vizia handles windowing,
//! event dispatch, and rendering (Skia/GL) internally — no custom
//! platform shim needed.
//!
//! # Usage
//!
//! ```ignore
//! use truce_vizia::{ViziaEditor, ParamEvent};
//! use truce_vizia::widgets::*;
//! use vizia::prelude::*;
//!
//! fn editor() -> Box<dyn truce_core::editor::Editor> {
//!     Box::new(ViziaEditor::new((400, 300), |cx| {
//!         HStack::new(cx, |cx| {
//!             ParamKnob::new(cx, 0, "Gain");
//!             ParamToggle::new(cx, 2, "Bypass");
//!         });
//!     }))
//! }
//! ```

pub mod editor;
pub mod param_lens;
pub mod param_model;
pub mod snapshot;
pub mod theme;
pub mod widgets;

pub use editor::ViziaEditor;
pub use param_lens::{MeterLens, ParamBoolLens, ParamFormatLens, ParamNormLens};
pub use param_model::{ParamEvent, ParamModel};

// Re-export vizia so plugin authors can use `vizia::prelude::*`
// without adding vizia as a direct dependency.
pub use vizia;
