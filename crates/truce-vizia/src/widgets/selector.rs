//! Parameter-bound enum selector (click-to-cycle).

use vizia::prelude::*;

use crate::param_lens::ParamFormatLens;
use crate::param_model::ParamEvent;

/// A click-to-cycle selector bound to an enum parameter.
///
/// Displays the current formatted value. Each click advances to the
/// next enum variant, wrapping around at the end.
pub struct ParamSelector;

impl ParamSelector {
    /// Create a parameter selector.
    ///
    /// `id` is the parameter ID (should be an `Enum` param).
    /// `label` is the display name. `num_options` is the total
    /// number of enum variants.
    pub fn new<'a>(
        cx: &'a mut Context,
        id: impl Into<u32>,
        label: &str,
        num_options: u32,
    ) -> Handle<'a, VStack> {
        let id = id.into();
        let label = label.to_string();

        VStack::new(cx, move |cx| {
            let count = num_options;

            // Clickable label showing the current value.
            Label::new(cx, ParamFormatLens(id))
                .class("param-value")
                .cursor(CursorIcon::Hand)
                .on_press(move |cx| {
                    // Advance to next option.
                    let current: f32 = crate::param_lens::ParamNormLens(id).get(cx);
                    let idx = (current * count as f32).floor() as u32;
                    let next = (idx + 1) % count;
                    let new_norm = (next as f64 + 0.5) / count as f64;
                    cx.emit(ParamEvent::SetImmediate(id, new_norm.clamp(0.0, 1.0)));
                });

            Label::new(cx, &label).class("param-name");
        })
        .class("param-widget")
    }
}
