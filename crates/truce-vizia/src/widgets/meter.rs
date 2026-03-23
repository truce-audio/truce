//! Level meter display.

use vizia::prelude::*;
use vizia::vg;

use crate::param_model::ParamModel;

/// A level meter displaying one or more channels.
///
/// Reads meter values from `ParamModel::meter()` each frame.
/// Does not emit parameter events (read-only display).
pub struct LevelMeter {
    meter_ids: Vec<u32>,
}

impl LevelMeter {
    /// Create a level meter.
    ///
    /// `meter_ids` are the meter IDs to display (e.g. `[METER_L, METER_R]`
    /// for a stereo meter). `label` is the display name shown below.
    pub fn new<'a>(
        cx: &'a mut Context,
        meter_ids: &[u32],
        label: &str,
    ) -> Handle<'a, VStack> {
        let label = label.to_string();
        let ids = meter_ids.to_vec();

        VStack::new(cx, move |cx| {
            Self { meter_ids: ids }.build(cx, |_cx| {});
            Label::new(cx, &label).class("param-name");
        })
        .class("param-widget")
    }
}

impl View for LevelMeter {
    fn draw(&self, cx: &mut DrawContext, canvas: &Canvas) {
        let bounds = cx.bounds();
        if bounds.w < 1.0 || bounds.h < 1.0 {
            return;
        }

        let num_channels = self.meter_ids.len().max(1);
        let gap = 2.0;
        let corner_r = 2.0;
        let bar_w = (bounds.w - gap * (num_channels as f32 - 1.0)) / num_channels as f32;

        // Background with rounded corners.
        let bg_rrect = vg::RRect::new_rect_xy(
            vg::Rect::from_xywh(bounds.x, bounds.y, bounds.w, bounds.h),
            corner_r,
            corner_r,
        );
        let mut bg_paint = vg::Paint::default();
        bg_paint.set_color(vg::Color::from_rgb(42, 42, 52));
        bg_paint.set_anti_alias(true);
        canvas.draw_rrect(bg_rrect, &bg_paint);

        // Per-channel bars.
        for (i, &meter_id) in self.meter_ids.iter().enumerate() {
            let level = cx
                .data::<ParamModel>()
                .map(|m| m.meter(meter_id))
                .unwrap_or(0.0)
                .clamp(0.0, 1.0);

            let x = bounds.x + i as f32 * (bar_w + gap);
            let bar_h = (bounds.h - 2.0) * level;
            let y = bounds.y + bounds.h - 1.0 - bar_h;

            // Gradient: green at bottom → yellow mid → red at top.
            let color = if level < 0.5 {
                vg::Color::from_rgb(60, 180, 80)
            } else if level < 0.8 {
                vg::Color::from_rgb(200, 190, 50)
            } else {
                vg::Color::from_rgb(220, 60, 50)
            };

            let bar_rrect = vg::RRect::new_rect_xy(
                vg::Rect::from_xywh(x + 1.0, y, bar_w - 2.0, bar_h),
                1.0,
                1.0,
            );
            let mut bar_paint = vg::Paint::default();
            bar_paint.set_color(color);
            bar_paint.set_anti_alias(true);
            canvas.draw_rrect(bar_rrect, &bar_paint);
        }
    }
}
