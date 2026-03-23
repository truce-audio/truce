//! XY pad controlling two parameters simultaneously.

use vizia::prelude::*;
use vizia::vg;

use crate::param_model::{ParamEvent, ParamModel};

/// A 2D XY pad bound to two parameter IDs (X and Y axes).
///
/// Drag anywhere in the pad to set both parameters simultaneously.
/// The X axis maps left→right (0→1), Y axis maps bottom→top (0→1).
pub struct XYPad {
    x_id: u32,
    y_id: u32,
    dragging: bool,
}

impl XYPad {
    /// Create an XY pad.
    ///
    /// `x_id` and `y_id` are the parameter IDs for the X and Y axes.
    /// `label` is the display name shown below.
    pub fn new<'a>(
        cx: &'a mut Context,
        x_id: u32,
        y_id: u32,
        label: &str,
    ) -> Handle<'a, VStack> {
        let label = label.to_string();

        VStack::new(cx, move |cx| {
            Self {
                x_id,
                y_id,
                dragging: false,
            }
            .build(cx, |_cx| {});
            Label::new(cx, &label).class("param-name");
        })
        .class("param-widget")
    }

    fn update_from_position(&self, cx: &mut EventContext, x: f32, y: f32) {
        let bounds = cx.bounds();
        let norm_x = ((x - bounds.x) / bounds.w).clamp(0.0, 1.0) as f64;
        let norm_y = (1.0 - (y - bounds.y) / bounds.h).clamp(0.0, 1.0) as f64;
        cx.emit(ParamEvent::SetNormalized(self.x_id, norm_x));
        cx.emit(ParamEvent::SetNormalized(self.y_id, norm_y));
    }
}

impl View for XYPad {
    fn event(&mut self, cx: &mut EventContext, event: &mut vizia::events::Event) {
        event.map(|window_event, _meta| match window_event {
            WindowEvent::MouseDown(MouseButton::Left) => {
                self.dragging = true;
                cx.capture();
                cx.emit(ParamEvent::BeginEdit(self.x_id));
                cx.emit(ParamEvent::BeginEdit(self.y_id));
                let (mx, my) = (cx.mouse().cursor_x, cx.mouse().cursor_y);
                self.update_from_position(cx, mx, my);
            }
            WindowEvent::MouseMove(x, y) => {
                if self.dragging {
                    self.update_from_position(cx, *x, *y);
                }
            }
            WindowEvent::MouseUp(MouseButton::Left) => {
                if self.dragging {
                    self.dragging = false;
                    cx.release();
                    cx.emit(ParamEvent::EndEdit(self.x_id));
                    cx.emit(ParamEvent::EndEdit(self.y_id));
                }
            }
            _ => {}
        });
    }

    fn draw(&self, cx: &mut DrawContext, canvas: &Canvas) {
        let bounds = cx.bounds();
        if bounds.w < 1.0 || bounds.h < 1.0 {
            return;
        }

        let (vx, vy) = cx
            .data::<ParamModel>()
            .map(|m| (m.get(self.x_id) as f32, m.get(self.y_id) as f32))
            .unwrap_or((0.5, 0.5));

        // Background.
        let bg_rect = vg::Rect::from_xywh(bounds.x, bounds.y, bounds.w, bounds.h);
        let mut bg_paint = vg::Paint::default();
        bg_paint.set_color(vg::Color::from_rgb(46, 46, 56));
        bg_paint.set_anti_alias(true);
        canvas.draw_rect(bg_rect, &bg_paint);

        // Crosshair lines.
        let px = bounds.x + vx * bounds.w;
        let py = bounds.y + (1.0 - vy) * bounds.h;

        let mut line_paint = vg::Paint::default();
        line_paint.set_color(vg::Color::from_argb(77, 255, 255, 255));
        line_paint.set_stroke_width(1.0);
        line_paint.set_style(vg::PaintStyle::Stroke);
        line_paint.set_anti_alias(true);

        // Horizontal line.
        canvas.draw_line(
            vg::Point::new(bounds.x, py),
            vg::Point::new(bounds.x + bounds.w, py),
            &line_paint,
        );

        // Vertical line.
        canvas.draw_line(
            vg::Point::new(px, bounds.y),
            vg::Point::new(px, bounds.y + bounds.h),
            &line_paint,
        );

        // Position dot.
        let mut dot_paint = vg::Paint::default();
        dot_paint.set_color(vg::Color::from_rgb(77, 153, 242));
        dot_paint.set_anti_alias(true);
        canvas.draw_circle(vg::Point::new(px, py), 5.0, &dot_paint);
    }
}
