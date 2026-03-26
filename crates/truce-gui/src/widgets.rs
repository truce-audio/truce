//! Audio plugin UI widgets: knobs, sliders, toggles, labels, headers.

use std::f32::consts::PI;

use crate::render::RenderBackend;
use crate::theme::{Color, Theme};

/// Widget type for interaction state tracking.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WidgetType {
    Knob,
    Slider,
    Toggle,
    Selector,
    Meter,
    XYPad,
}

/// Draw a rotary knob.
///
/// `value` is normalized 0.0–1.0.
/// `label` is shown below the knob.
/// `value_text` is shown below the label.
pub fn draw_knob(
    ctx: &mut dyn RenderBackend,
    x: f32,
    y: f32,
    size: f32,
    value: f32,
    label: &str,
    value_text: &str,
    theme: &Theme,
    highlighted: bool,
) {
    let cx = x + size / 2.0;
    let cy = y + size / 2.0 - 8.0; // leave room for label below
    let radius = size / 2.0 - 6.0;

    // Knob range: from 225° (bottom-left) to -45° (bottom-right), going clockwise
    // In radians: 225° = 5π/4, -45° = -π/4 (or 315° = 7π/4)
    let start_angle = 0.75 * PI; // 135° from 12 o'clock → 225° in standard math
    let end_angle = 2.25 * PI; // 405° = 45° past full rotation
    let arc_start = start_angle;
    let arc_end = end_angle;

    // Track arc (full range background)
    ctx.stroke_arc(cx, cy, radius, arc_start, arc_end, theme.knob_track, 3.0);

    // Value arc (filled portion)
    let value_angle = arc_start + value * (arc_end - arc_start);
    if value > 0.01 {
        ctx.stroke_arc(cx, cy, radius, arc_start, value_angle, theme.knob_fill, 3.0);
    }

    // Pointer line from center to current position
    let pointer_len = radius * 0.6;
    let px = cx + pointer_len * value_angle.cos();
    let py = cy + pointer_len * value_angle.sin();
    ctx.draw_line(cx, cy, px, py, theme.knob_pointer, 2.0);

    // Hover highlight ring
    if highlighted {
        ctx.stroke_arc(cx, cy, radius + 3.0, arc_start, arc_end, theme.accent, 1.5);
    }

    // Value text (below knob)
    let val_size = 10.0;
    let val_w = ctx.text_width(value_text, val_size);
    ctx.draw_text(value_text, cx - val_w / 2.0, y + size - 2.0, val_size, theme.text);

    // Label text (below value)
    let label_size = 9.0;
    let label_w = ctx.text_width(label, label_size);
    ctx.draw_text(label, cx - label_w / 2.0, y + size + 10.0, label_size, theme.text_dim);
}

/// Draw a header bar.
pub fn draw_header(
    ctx: &mut dyn RenderBackend,
    x: f32,
    y: f32,
    w: f32,
    h: f32,
    title: &str,
    version: &str,
    theme: &Theme,
) {
    ctx.fill_rect(x, y, w, h, theme.header_bg);

    let title_size = 12.0;
    ctx.draw_text(
        title,
        x + 10.0,
        y + (h - title_size) / 2.0,
        title_size,
        theme.header_text,
    );

    let ver_size = 9.0;
    let ver_w = ctx.text_width(version, ver_size);
    ctx.draw_text(
        version,
        x + w - ver_w - 10.0,
        y + (h - ver_size) / 2.0,
        ver_size,
        theme.text_dim,
    );
}

/// Draw a horizontal slider.
///
/// `value` is normalized 0.0–1.0.
pub fn draw_slider(
    ctx: &mut dyn RenderBackend,
    x: f32,
    y: f32,
    width: f32,
    height: f32,
    value: f32,
    label: &str,
    value_text: &str,
    theme: &Theme,
    highlighted: bool,
) {
    let track_y = y + height / 2.0 - 8.0;
    let track_h = 4.0;
    let margin = 6.0;
    let track_w = width - margin * 2.0;

    // Track background
    ctx.fill_rect(x + margin, track_y, track_w, track_h, theme.knob_track);

    // Filled portion
    let fill_w = track_w * value;
    if fill_w > 0.5 {
        ctx.fill_rect(x + margin, track_y, fill_w, track_h, theme.knob_fill);
    }

    // Thumb
    let thumb_x = x + margin + fill_w;
    let thumb_r = 6.0;
    ctx.fill_circle(thumb_x, track_y + track_h / 2.0, thumb_r, theme.knob_pointer);
    if highlighted {
        ctx.fill_circle(thumb_x, track_y + track_h / 2.0, thumb_r + 2.0, theme.accent);
        ctx.fill_circle(thumb_x, track_y + track_h / 2.0, thumb_r, theme.knob_pointer);
    }

    // Value text
    let val_size = 10.0;
    let cx = x + width / 2.0;
    let val_w = ctx.text_width(value_text, val_size);
    ctx.draw_text(value_text, cx - val_w / 2.0, y + height - 2.0, val_size, theme.text);

    // Label
    let label_size = 9.0;
    let label_w = ctx.text_width(label, label_size);
    ctx.draw_text(label, cx - label_w / 2.0, y + height + 10.0, label_size, theme.text_dim);
}

/// Draw a toggle button (on/off).
///
/// `value` > 0.5 = on, <= 0.5 = off.
pub fn draw_toggle(
    ctx: &mut dyn RenderBackend,
    x: f32,
    y: f32,
    width: f32,
    height: f32,
    value: f32,
    label: &str,
    value_text: &str,
    theme: &Theme,
    highlighted: bool,
) {
    let is_on = value > 0.5;
    let cx = x + width / 2.0;
    let cy = y + height / 2.0 - 8.0;

    // Toggle track (pill shape)
    let track_w = 32.0;
    let track_h = 16.0;
    let track_x = cx - track_w / 2.0;
    let track_y = cy - track_h / 2.0;
    let bg = if is_on { theme.knob_fill } else { theme.knob_track };
    ctx.fill_rect(track_x, track_y, track_w, track_h, bg);

    // Thumb circle
    let thumb_x = if is_on {
        track_x + track_w - track_h / 2.0
    } else {
        track_x + track_h / 2.0
    };
    ctx.fill_circle(thumb_x, cy, track_h / 2.0 - 2.0, theme.knob_pointer);

    if highlighted {
        ctx.fill_rect(track_x - 2.0, track_y - 2.0, track_w + 4.0, track_h + 4.0, theme.accent);
        ctx.fill_rect(track_x, track_y, track_w, track_h, bg);
        ctx.fill_circle(thumb_x, cy, track_h / 2.0 - 2.0, theme.knob_pointer);
    }

    // Value text
    let val_size = 10.0;
    let val_w = ctx.text_width(value_text, val_size);
    ctx.draw_text(value_text, cx - val_w / 2.0, y + height - 2.0, val_size, theme.text);

    // Label
    let label_size = 9.0;
    let label_w = ctx.text_width(label, label_size);
    ctx.draw_text(label, cx - label_w / 2.0, y + height + 10.0, label_size, theme.text_dim);
}

/// Draw a selector (enum parameter — click to cycle through values).
///
/// Shows the current value name with < > arrows.
pub fn draw_selector(
    ctx: &mut dyn RenderBackend,
    x: f32,
    y: f32,
    width: f32,
    height: f32,
    _value: f32,
    label: &str,
    value_text: &str,
    theme: &Theme,
    highlighted: bool,
) {
    let cx = x + width / 2.0;
    let cy = y + height / 2.0 - 8.0;

    // Background box — size to fit content
    let val_size = 10.0;
    let arrow_size = 8.0;
    let arrow_pad = 14.0; // space for arrow on each side
    let val_w = ctx.text_width(value_text, val_size);
    let box_w = (val_w + arrow_pad * 2.0 + 8.0).max(width - 12.0);
    let box_h = 20.0;
    let box_x = cx - box_w / 2.0;
    let box_y = cy - box_h / 2.0;
    let bg = if highlighted { theme.accent } else { theme.knob_track };
    ctx.fill_rect(box_x, box_y, box_w, box_h, bg);

    // Value text (centered)
    ctx.draw_text(
        value_text,
        cx - val_w / 2.0,
        cy - val_size / 2.0,
        val_size,
        theme.text,
    );

    // Left/right arrows
    ctx.draw_text("<", box_x + 4.0, cy - arrow_size / 2.0, arrow_size, theme.text_dim);
    let gt_w = ctx.text_width(">", arrow_size);
    ctx.draw_text(">", box_x + box_w - gt_w - 4.0, cy - arrow_size / 2.0, arrow_size, theme.text_dim);

    // Label (below)
    let label_size = 9.0;
    let label_w = ctx.text_width(label, label_size);
    ctx.draw_text(label, cx - label_w / 2.0, y + height + 10.0, label_size, theme.text_dim);
}

/// Draw a vertical level meter with one or more channels.
///
/// Each level is 0.0–1.0 (linear, not dB).
pub fn draw_meter(
    ctx: &mut dyn RenderBackend,
    x: f32,
    y: f32,
    width: f32,
    height: f32,
    levels: &[f32],
    label: &str,
    theme: &Theme,
) {
    let cx = x + width / 2.0;
    let num = levels.len().max(1);
    let bar_w = 6.0f32;
    let gap = 2.0f32;
    let total_bar_w = num as f32 * bar_w + (num as f32 - 1.0).max(0.0) * gap;
    let bar_h = height - 4.0; // fill nearly full height
    let bar_start_x = cx - total_bar_w / 2.0;
    let bar_y = y + 2.0;

    for (i, &level) in levels.iter().enumerate() {
        let bx = bar_start_x + i as f32 * (bar_w + gap);

        // Background
        ctx.fill_rect(bx, bar_y, bar_w, bar_h, theme.knob_track);

        // dB-scaled fill from bottom
        let display = truce_core::meter_display(level);
        let fill_h = bar_h * display;
        if fill_h > 0.5 {
            // Blue normally, red when clipping (> -3 dB ≈ display > 0.95)
            let color = if display > 0.95 {
                Color::rgb(0.88, 0.27, 0.27)
            } else {
                theme.knob_fill
            };
            ctx.fill_rect(bx, bar_y + bar_h - fill_h, bar_w, fill_h, color);
        }
    }

    // Label (below the widget, same position as knob labels)
    let label_size = 8.0;
    let label_w = ctx.text_width(label, label_size);
    ctx.draw_text(label, cx - label_w / 2.0, y + height + 4.0, label_size, theme.text_dim);
}

/// Draw an XY pad (2D control for two parameters).
///
/// `value_x` and `value_y` are normalized 0.0–1.0.
pub fn draw_xy_pad(
    ctx: &mut dyn RenderBackend,
    x: f32,
    y: f32,
    width: f32,
    height: f32,
    value_x: f32,
    value_y: f32,
    label_x: &str,
    label_y: &str,
    theme: &Theme,
    highlighted: bool,
) {
    let pad_margin = 4.0;
    let pad_x = x + pad_margin;
    let pad_y = y + pad_margin;
    let pad_w = width - pad_margin * 2.0;
    let pad_h = height - pad_margin * 2.0;

    // Background
    ctx.fill_rect(pad_x, pad_y, pad_w, pad_h, theme.knob_track);

    // Crosshair lines
    let dot_x = pad_x + value_x.clamp(0.0, 1.0) * pad_w;
    let dot_y = pad_y + (1.0 - value_y.clamp(0.0, 1.0)) * pad_h; // invert Y
    let line_color = theme.text_dim;
    ctx.draw_line(dot_x, pad_y, dot_x, pad_y + pad_h, line_color, 1.0);
    ctx.draw_line(pad_x, dot_y, pad_x + pad_w, dot_y, line_color, 1.0);

    // Dot at intersection
    let dot_color = if highlighted { theme.accent } else { theme.knob_fill };
    ctx.fill_circle(dot_x, dot_y, 5.0, dot_color);
    ctx.fill_circle(dot_x, dot_y, 3.0, theme.knob_pointer);

    // Border
    if highlighted {
        ctx.draw_line(pad_x, pad_y, pad_x + pad_w, pad_y, theme.accent, 1.5);
        ctx.draw_line(pad_x + pad_w, pad_y, pad_x + pad_w, pad_y + pad_h, theme.accent, 1.5);
        ctx.draw_line(pad_x + pad_w, pad_y + pad_h, pad_x, pad_y + pad_h, theme.accent, 1.5);
        ctx.draw_line(pad_x, pad_y + pad_h, pad_x, pad_y, theme.accent, 1.5);
    }

    // Axis labels: X below the widget (like knob labels), Y at top-left inside pad
    let label_size = 8.0;
    let x_label_w = ctx.text_width(label_x, label_size);
    let cx = x + width / 2.0;
    ctx.draw_text(label_x, cx - x_label_w / 2.0, y + height + 4.0, label_size, theme.text_dim);

    if !label_y.is_empty() {
        ctx.draw_text(label_y, pad_x + 3.0, pad_y + 2.0, label_size, theme.text_dim);
    }
}

/// Draw a group/section label.
pub fn draw_section_label(
    ctx: &mut dyn RenderBackend,
    x: f32,
    y: f32,
    w: f32,
    label: &str,
    theme: &Theme,
) {
    let size = 9.0;
    let label_w = ctx.text_width(label, size);
    ctx.draw_text(label, x + (w - label_w) / 2.0, y, size, theme.text_dim);
}
