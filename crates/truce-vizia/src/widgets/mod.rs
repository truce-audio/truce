//! Parameter-bound widgets for truce-vizia.
//!
//! Each widget wraps a vizia built-in view and wires it to the
//! `ParamModel` via `ParamEvent` emissions. Widgets read parameter
//! values directly from the `EditorContext` (through `ParamModel`)
//! each frame, and emit `ParamEvent` on user interaction.

mod gesture;
mod knob;
mod meter;
mod selector;
mod slider;
mod toggle;
mod xy_pad;

pub use knob::ParamKnob;
pub use meter::LevelMeter;
pub use selector::ParamSelector;
pub use slider::ParamSlider;
pub use toggle::ParamToggle;
pub use xy_pad::XYPad;
