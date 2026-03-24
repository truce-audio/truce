//! Parameter-bound horizontal slider.

use vizia::prelude::*;

use crate::param_lens::{ParamFormatLens, ParamNormLens};
use crate::param_model::ParamEvent;

use super::gesture::GestureWrapper;

/// A horizontal slider bound to a parameter ID.
pub struct ParamSlider;

impl ParamSlider {
    /// Create a parameter slider with name and value on one line.
    ///
    /// `id` is the parameter ID, `label` is the display name.
    pub fn new<'a>(cx: &'a mut Context, id: impl Into<u32>, label: &str) -> Handle<'a, VStack> {
        let id = id.into();
        let label = label.to_string();

        VStack::new(cx, move |cx| {
            // Name + value on one row above the slider.
            HStack::new(cx, move |cx| {
                Label::new(cx, &label).class("param-name");
                Label::new(cx, ParamFormatLens(id)).class("param-value");
            })
            .horizontal_gap(Pixels(4.0))
            .height(Auto);

            // GestureWrapper emits BeginEdit/EndEdit on mouse down/up.
            GestureWrapper::new(cx, id, move |cx| {
                Slider::new(cx, ParamNormLens(id))
                    .on_change(move |cx, val| {
                        cx.emit(ParamEvent::SetNormalized(id, val as f64));
                    });
            });
        })
        .class("param-widget")
    }
}
