//! Parameter-bound rotary knob with custom arc rendering.

use std::f32::consts::PI;

use vizia::prelude::*;
use vizia::vg;

use crate::param_lens::{ParamFormatLens, ParamNormLens};
use crate::param_model::ParamEvent;

use super::gesture::GestureWrapper;

/// 270-degree arc: from bottom-left (135°) to bottom-right (405°).
const START_DEG: f32 = 135.0;
const SWEEP_DEG: f32 = 270.0;
const TRACK_WIDTH: f32 = 3.0;
const POINTER_RADIUS: f32 = 3.0;

/// Custom arc visual for the knob — replaces vizia's default ArcTrack
/// which draws unwanted center lines.
struct KnobArc {
    param_id: u32,
}

impl View for KnobArc {
    fn draw(&self, cx: &mut DrawContext, canvas: &Canvas) {
        let bounds = cx.bounds();
        if bounds.w < 4.0 || bounds.h < 4.0 {
            return;
        }

        let center_x = bounds.x + bounds.w * 0.5;
        let center_y = bounds.y + bounds.h * 0.5;
        let radius = (bounds.w.min(bounds.h) * 0.5) - TRACK_WIDTH - 2.0;

        let value = cx
            .data::<crate::param_model::ParamModel>()
            .map(|m| m.get(self.param_id) as f32)
            .unwrap_or(0.0);

        // Track arc (full 270°).
        draw_arc(
            canvas,
            center_x,
            center_y,
            radius,
            START_DEG,
            SWEEP_DEG,
            TRACK_WIDTH,
            vg::Color::from_rgb(58, 58, 68),
        );

        // Active arc (filled portion).
        if value > 0.001 {
            draw_arc(
                canvas,
                center_x,
                center_y,
                radius,
                START_DEG,
                SWEEP_DEG * value,
                TRACK_WIDTH,
                vg::Color::from_rgb(77, 153, 242),
            );
        }

        // Pointer dot at current position.
        let angle_rad = (START_DEG + SWEEP_DEG * value) * PI / 180.0;
        let dot_x = center_x + angle_rad.cos() * radius;
        let dot_y = center_y + angle_rad.sin() * radius;

        let mut dot_paint = vg::Paint::default();
        dot_paint.set_color(vg::Color::from_rgb(230, 230, 235));
        dot_paint.set_anti_alias(true);
        canvas.draw_circle(vg::Point::new(dot_x, dot_y), POINTER_RADIUS, &dot_paint);
    }
}

fn draw_arc(
    canvas: &Canvas,
    cx: f32,
    cy: f32,
    radius: f32,
    start_deg: f32,
    sweep_deg: f32,
    width: f32,
    color: vg::Color,
) {
    let oval = vg::Rect::from_xywh(
        cx - radius,
        cy - radius,
        radius * 2.0,
        radius * 2.0,
    );

    let mut paint = vg::Paint::default();
    paint.set_color(color);
    paint.set_stroke_width(width);
    paint.set_stroke_cap(vg::PaintCap::Round);
    paint.set_style(vg::PaintStyle::Stroke);
    paint.set_anti_alias(true);

    // useCenter = false: draw just the arc, no lines to center.
    canvas.draw_arc(oval, start_deg, sweep_deg, false, &paint);
}

/// A rotary knob bound to a parameter ID.
pub struct ParamKnob;

impl ParamKnob {
    /// Create a parameter knob with custom arc visual.
    pub fn new<'a>(cx: &'a mut Context, id: u32, label: &str) -> Handle<'a, VStack> {
        let label = label.to_string();

        VStack::new(cx, move |cx| {
            // GestureWrapper emits BeginEdit/EndEdit on mouse down/up.
            GestureWrapper::new(cx, id, move |cx| {
                Knob::custom(cx, 0.5, ParamNormLens(id), move |cx, _lens| {
                    KnobArc { param_id: id }
                        .build(cx, |_| {})
                        .width(Percentage(100.0))
                        .height(Percentage(100.0))
                })
                .on_change(move |cx, val| {
                    cx.emit(ParamEvent::SetNormalized(id, val as f64));
                });
            });

            // Formatted value label.
            Label::new(cx, ParamFormatLens(id)).class("param-value");

            // Parameter name label.
            Label::new(cx, &label).class("param-name");
        })
        .class("param-widget")
    }
}
