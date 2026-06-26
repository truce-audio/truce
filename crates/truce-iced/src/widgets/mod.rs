//! Widget library for iced-based plugin UIs.
//!
//! Provides parameter-bound widgets that emit `Message::Param` messages
//! for host communication.

pub mod dropdown;
pub mod knob;
pub mod meter;
pub mod slider;
pub mod toggle;
pub mod xy_pad;

use std::fmt::Debug;

use truce_params::Params;

use crate::param_cache::ParamCache;

// Re-export widget types for convenience.
pub use dropdown::DropdownWidget;
pub use knob::KnobWidget;
pub use meter::MeterWidget;
pub use slider::SliderWidget;
pub use toggle::ToggleWidget;
pub use xy_pad::XYPadWidget;

/// Create a rotary knob bound to a parameter.
pub fn knob<M: Clone + Debug + 'static>(
    id: impl Into<u32>,
    params: &ParamCache<impl Params>,
) -> KnobWidget<'_, M> {
    KnobWidget::new(id.into(), params)
}

/// Create a horizontal slider bound to a parameter.
pub fn param_slider<M: Clone + Debug + 'static>(
    id: impl Into<u32>,
    params: &ParamCache<impl Params>,
) -> SliderWidget<'_, M> {
    SliderWidget::new(id.into(), params)
}

/// Create a toggle switch bound to a parameter.
pub fn param_toggle<M: Clone + Debug + 'static>(
    id: impl Into<u32>,
    params: &ParamCache<impl Params>,
) -> ToggleWidget<'_, M> {
    ToggleWidget::new(id.into(), params)
}

/// Create a dropdown (pick list) bound to an enum parameter.
pub fn param_dropdown<M: Clone + Debug + 'static>(
    id: impl Into<u32>,
    params: &ParamCache<impl Params>,
) -> DropdownWidget<'_, M> {
    DropdownWidget::new(id.into(), params)
}

/// Create a level meter display.
pub fn meter<'a, M: Clone + Debug + 'static>(
    ids: &[impl Into<u32> + Copy],
    params: &'a ParamCache<impl Params>,
) -> MeterWidget<'a, M> {
    let u32_ids: Vec<u32> = ids.iter().map(|id| (*id).into()).collect();
    MeterWidget::new(&u32_ids, params)
}

/// Create an XY pad controlling two parameters.
pub fn xy_pad<M: Clone + Debug + 'static>(
    x_id: impl Into<u32>,
    y_id: impl Into<u32>,
    params: &ParamCache<impl Params>,
) -> XYPadWidget<'_, M> {
    XYPadWidget::new(x_id.into(), y_id.into(), params)
}
