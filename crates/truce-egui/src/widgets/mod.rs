//! Helper widgets that wrap egui primitives with truce's parameter protocol.
//!
//! These are optional — users can always use raw egui widgets and interact
//! with `ParamState` directly.

mod knob;
mod meter;
mod selector;
mod slider;
mod toggle;
mod xy_pad;

pub use knob::param_knob;
pub use meter::level_meter;
pub use selector::param_selector;
pub use slider::param_slider;
pub use toggle::param_toggle;
pub use xy_pad::param_xy_pad;
