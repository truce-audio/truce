//! Parameter-bound toggle switch.

use vizia::prelude::*;

use crate::param_lens::ParamBoolLens;
use crate::param_model::ParamEvent;

/// A toggle switch bound to a boolean parameter.
pub struct ParamToggle;

impl ParamToggle {
    /// Create a parameter toggle.
    ///
    /// `id` is the parameter ID (should be a `Discrete { min: 0, max: 1 }`
    /// or `BoolParam`). `label` is the display name shown below.
    pub fn new<'a>(cx: &'a mut Context, id: impl Into<u32>, label: &str) -> Handle<'a, VStack> {
        let id = id.into();
        let label = label.to_string();

        VStack::new(cx, move |cx| {
            Switch::new(cx, ParamBoolLens(id))
                .on_toggle(move |cx| {
                    let current: bool = ParamBoolLens(id).get(cx);
                    let new_val = if current { 0.0 } else { 1.0 };
                    cx.emit(ParamEvent::SetImmediate(id, new_val));
                });

            Label::new(cx, &label).class("param-name");
        })
        .class("param-widget")
    }
}
