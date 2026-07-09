//! XY pad control bound to two truce parameters.

use truce_core::editor::{PluginContext, PluginContextReadF32};

const LABEL_H: f32 = 16.0;
const DOT_RADIUS: f32 = 5.0;

/// Show an XY pad bound to two truce parameters.
///
/// `id_x` controls the horizontal axis (0=left, 1=right).
/// `id_y` controls the vertical axis (0=bottom, 1=top).
/// Drag anywhere in the pad to set both values simultaneously.
pub fn param_xy_pad<P: ?Sized>(
    ui: &mut egui::Ui,
    state: &PluginContext<P>,
    id_x: impl Into<u32>,
    id_y: impl Into<u32>,
    label: &str,
    width: f32,
    height: f32,
) -> egui::Response {
    let id_x = id_x.into();
    let id_y = id_y.into();
    let desired = egui::vec2(width, height + LABEL_H);
    let (rect, response) = ui.allocate_exact_size(desired, egui::Sense::drag());

    let pad_rect = egui::Rect::from_min_size(rect.min, egui::vec2(width, height));

    let mut vx = state.get_param(id_x);
    let mut vy = state.get_param(id_y);

    if response.drag_started() {
        state.begin_edit(id_x);
        state.begin_edit(id_y);
    }
    if (response.dragged() || response.drag_started())
        && let Some(pos) = response.interact_pointer_pos()
    {
        vx = ((pos.x - pad_rect.left()) / pad_rect.width()).clamp(0.0, 1.0);
        vy = 1.0 - ((pos.y - pad_rect.top()) / pad_rect.height()).clamp(0.0, 1.0);
        state.set_param(id_x, f64::from(vx));
        state.set_param(id_y, f64::from(vy));
    }
    if response.drag_stopped() {
        state.end_edit(id_x);
        state.end_edit(id_y);
    }

    if ui.is_rect_visible(rect) {
        // Expand the clip by the dot radius so the dot draws fully past the
        // pad edge at the value extremes. `painter_at(rect)` would cut its
        // left/top/right halves (the bottom sits in the label reserve).
        let painter = ui.painter_at(rect.expand(DOT_RADIUS));

        // Background (matches iced SURFACE)
        painter.rect_filled(pad_rect, 0.0, crate::theme::SURFACE);

        // Border. Inset by half a pixel so the 1px stroke stays inside
        // `painter_at(rect)`'s clip: egui's clip rect is max-exclusive,
        // so a line exactly on `pad_rect.right()` - which is the column's
        // right edge when the pad fills the available width - gets cut,
        // leaving the right border invisible. (Left/top sit on the
        // inclusive min edge and the bottom is inset by `LABEL_H`, so
        // only the right edge is affected, but inset all four for an
        // even border.)
        let bc = egui::Color32::from_rgb(115, 115, 115);
        let bs = egui::Stroke::new(1.0_f32, bc);
        let b = pad_rect.shrink(0.5);
        painter.line_segment([b.left_top(), b.right_top()], bs);
        painter.line_segment([b.right_top(), b.right_bottom()], bs);
        painter.line_segment([b.right_bottom(), b.left_bottom()], bs);
        painter.line_segment([b.left_bottom(), b.left_top()], bs);

        // Crosshair position. The dot sits on the pad edge at the value
        // extremes; the painter clip is expanded by the radius (below) so
        // its outer half draws past the border instead of being cut.
        let dot_x = pad_rect.left() + pad_rect.width() * vx;
        let dot_y = pad_rect.top() + pad_rect.height() * (1.0 - vy);

        // Crosshair lines (30% opacity accent)
        let crosshair_color = crate::theme::KNOB_FILL.linear_multiply(0.3);
        painter.line_segment(
            [
                egui::pos2(dot_x, pad_rect.top()),
                egui::pos2(dot_x, pad_rect.bottom()),
            ],
            egui::Stroke::new(1.0_f32, crosshair_color),
        );
        painter.line_segment(
            [
                egui::pos2(pad_rect.left(), dot_y),
                egui::pos2(pad_rect.right(), dot_y),
            ],
            egui::Stroke::new(1.0_f32, crosshair_color),
        );

        // Dot
        painter.circle_filled(
            egui::pos2(dot_x, dot_y),
            DOT_RADIUS,
            crate::theme::KNOB_FILL,
        );

        // Label
        let dim_color = ui.visuals().widgets.noninteractive.fg_stroke.color;
        painter.text(
            egui::pos2(rect.center().x, pad_rect.bottom() + 2.0),
            egui::Align2::CENTER_TOP,
            label,
            egui::FontId::proportional(10.0),
            dim_color,
        );
    }

    response
}
