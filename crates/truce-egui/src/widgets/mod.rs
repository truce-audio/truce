//! Helper widgets that wrap egui primitives with truce's parameter protocol.
//!
//! These are optional - users can always use raw egui widgets and call
//! `PluginContext` methods (`get_param`, `set_param`, `automate`, …) directly.

mod dropdown;
mod knob;
mod meter;
mod selector;
mod slider;
mod toggle;
mod xy_pad;

pub use dropdown::param_dropdown;
pub use knob::param_knob;
pub use meter::level_meter;
// `#[allow(deprecated)]` so this re-export of an item that
// carries its own `#[deprecated]` doesn't fire the lint here -
// we're surfacing the item by design, not calling it.
#[allow(deprecated)]
pub use selector::param_selector;
pub use slider::param_slider;
pub use toggle::param_toggle;
pub use xy_pad::param_xy_pad;
