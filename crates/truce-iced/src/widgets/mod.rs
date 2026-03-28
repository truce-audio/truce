//! Widget library for iced-based plugin UIs.
//!
//! Provides parameter-bound widgets that emit `Message::Param` messages
//! for host communication.

pub mod knob;
pub mod meter;
pub mod selector;
pub mod slider;
pub mod toggle;
pub mod xy_pad;

use std::fmt::Debug;

use truce_params::Params;

use crate::param_state::ParamState;

// Re-export widget types for convenience.
pub use knob::KnobWidget;
pub use meter::MeterWidget;
pub use selector::SelectorWidget;
pub use slider::SliderWidget;
pub use toggle::ToggleWidget;
pub use xy_pad::XYPadWidget;

/// Create a rotary knob bound to a parameter.
pub fn knob<'a, M: Clone + Debug + 'static>(
    id: impl Into<u32>,
    params: &'a ParamState<impl Params>,
) -> KnobWidget<'a, M> {
    KnobWidget::new(id.into(), params)
}

/// Create a horizontal slider bound to a parameter.
pub fn param_slider<'a, M: Clone + Debug + 'static>(
    id: impl Into<u32>,
    params: &'a ParamState<impl Params>,
) -> SliderWidget<'a, M> {
    SliderWidget::new(id.into(), params)
}

/// Create a toggle switch bound to a parameter.
pub fn param_toggle<'a, M: Clone + Debug + 'static>(
    id: impl Into<u32>,
    params: &'a ParamState<impl Params>,
) -> ToggleWidget<'a, M> {
    ToggleWidget::new(id.into(), params)
}

/// Create a selector (pick list) bound to an enum parameter.
pub fn param_selector<'a, M: Clone + Debug + 'static>(
    id: impl Into<u32>,
    params: &'a ParamState<impl Params>,
) -> SelectorWidget<'a, M> {
    SelectorWidget::new(id.into(), params)
}

/// Create a level meter display.
pub fn meter<'a, M: Clone + Debug + 'static>(
    ids: &[impl Into<u32> + Copy],
    params: &'a ParamState<impl Params>,
) -> MeterWidget<'a, M> {
    let u32_ids: Vec<u32> = ids.iter().map(|id| (*id).into()).collect();
    MeterWidget::new(&u32_ids, params)
}

/// Create an XY pad controlling two parameters.
pub fn xy_pad<'a, M: Clone + Debug + 'static>(
    x_id: impl Into<u32>,
    y_id: impl Into<u32>,
    params: &'a ParamState<impl Params>,
) -> XYPadWidget<'a, M> {
    XYPadWidget::new(x_id.into(), y_id.into(), params)
}
