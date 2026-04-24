//! Audio plugin UI widgets: knobs, sliders, toggles, labels, headers.

use std::f32::consts::PI;

use crate::interaction::InteractionState;
use crate::layout::{
    compute_section_offsets, GridLayout, Layout, PluginLayout, WidgetKind, GRID_GAP, GRID_HEADER_H,
    GRID_PADDING, GRID_SECTION_H,
};
use crate::render::RenderBackend;
use crate::snapshot::ParamSnapshot;
use crate::theme::{Color, Theme};

/// Widget type for interaction state tracking.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WidgetType {
    Knob,
    Slider,
    Toggle,
    Selector,
    /// Dropdown list — click to open a popup of all options.
    Dropdown,
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
    let cy = y + size / 2.0 - 5.0; // leave room for label below
    let radius = size / 2.0 - 4.0;

    // Knob range: from 225° (bottom-left) to -45° (bottom-right), going clockwise
    // In radians: 225° = 5π/4, -45° = -π/4 (or 315° = 7π/4)
    let start_angle = 0.75 * PI; // 135° from 12 o'clock → 225° in standard math
    let end_angle = 2.25 * PI; // 405° = 45° past full rotation
    let arc_start = start_angle;
    let arc_end = end_angle;

    // Track arc (full range background)
    ctx.stroke_arc(cx, cy, radius, arc_start, arc_end, theme.knob_track, 2.0);

    // Value arc (filled portion)
    let value_angle = arc_start + value * (arc_end - arc_start);
    if value > 0.01 {
        ctx.stroke_arc(cx, cy, radius, arc_start, value_angle, theme.knob_fill, 2.0);
    }

    // Pointer line from center to current position
    let pointer_len = radius * 0.6;
    let px = cx + pointer_len * value_angle.cos();
    let py = cy + pointer_len * value_angle.sin();
    ctx.draw_line(cx, cy, px, py, theme.knob_pointer, 1.5);

    // Hover highlight ring
    if highlighted {
        ctx.stroke_arc(cx, cy, radius + 2.0, arc_start, arc_end, theme.accent, 1.0);
    }

    // Value text (below knob)
    let val_size = 10.0;
    let val_w = ctx.text_width(value_text, val_size);
    ctx.draw_text(
        value_text,
        cx - val_w / 2.0,
        y + size - 9.0,
        val_size,
        theme.text,
    );

    // Label text (below value)
    let label_size = 9.0;
    let label_w = ctx.text_width(label, label_size);
    ctx.draw_text(
        label,
        cx - label_w / 2.0,
        y + size + 2.0,
        label_size,
        theme.text_dim,
    );
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
        y + (h - title_size) / 2.0 - 1.0,
        title_size,
        theme.header_text,
    );

    let ver_size = 9.0;
    let ver_w = ctx.text_width(version, ver_size);
    ctx.draw_text(
        version,
        x + w - ver_w - 10.0,
        y + (h - ver_size) / 2.0 - 1.0,
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
    let track_y = y + height / 2.0 - 5.0;
    let track_h = 3.0;
    let margin = 4.0;
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
    let thumb_r = 4.0;
    ctx.fill_circle(
        thumb_x,
        track_y + track_h / 2.0,
        thumb_r,
        theme.knob_pointer,
    );
    if highlighted {
        ctx.fill_circle(
            thumb_x,
            track_y + track_h / 2.0,
            thumb_r + 1.5,
            theme.accent,
        );
        ctx.fill_circle(
            thumb_x,
            track_y + track_h / 2.0,
            thumb_r,
            theme.knob_pointer,
        );
    }

    // Value text
    let val_size = 10.0;
    let cx = x + width / 2.0;
    let val_w = ctx.text_width(value_text, val_size);
    ctx.draw_text(
        value_text,
        cx - val_w / 2.0,
        y + height - 9.0,
        val_size,
        theme.text,
    );

    // Label
    let label_size = 9.0;
    let label_w = ctx.text_width(label, label_size);
    ctx.draw_text(
        label,
        cx - label_w / 2.0,
        y + height + 2.0,
        label_size,
        theme.text_dim,
    );
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
    let cy = y + height / 2.0 - 5.0;

    // Toggle track (pill shape)
    let track_w = 20.0;
    let track_h = 10.0;
    let track_x = cx - track_w / 2.0;
    let track_y = cy - track_h / 2.0;
    let bg = if is_on {
        theme.knob_fill
    } else {
        theme.knob_track
    };
    ctx.fill_rect(track_x, track_y, track_w, track_h, bg);

    // Thumb circle
    let thumb_x = if is_on {
        track_x + track_w - track_h / 2.0
    } else {
        track_x + track_h / 2.0
    };
    ctx.fill_circle(thumb_x, cy, track_h / 2.0 - 1.0, theme.knob_pointer);

    if highlighted {
        ctx.fill_rect(
            track_x - 1.0,
            track_y - 1.0,
            track_w + 2.0,
            track_h + 2.0,
            theme.accent,
        );
        ctx.fill_rect(track_x, track_y, track_w, track_h, bg);
        ctx.fill_circle(thumb_x, cy, track_h / 2.0 - 1.0, theme.knob_pointer);
    }

    // Value text
    let val_size = 10.0;
    let val_w = ctx.text_width(value_text, val_size);
    ctx.draw_text(
        value_text,
        cx - val_w / 2.0,
        y + height - 9.0,
        val_size,
        theme.text,
    );

    // Label
    let label_size = 9.0;
    let label_w = ctx.text_width(label, label_size);
    ctx.draw_text(
        label,
        cx - label_w / 2.0,
        y + height + 2.0,
        label_size,
        theme.text_dim,
    );
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
    let cy = y + height / 2.0 - 5.0;

    // Background box — size to fit content
    let val_size = 10.0;
    let arrow_size = 8.0;
    let arrow_pad = 9.0; // space for arrow on each side
    let val_w = ctx.text_width(value_text, val_size);
    let box_w = (val_w + arrow_pad * 2.0 + 5.0).max(width - 8.0);
    let box_h = 13.0;
    let box_x = cx - box_w / 2.0;
    let box_y = cy - box_h / 2.0;
    let bg = if highlighted {
        theme.accent
    } else {
        theme.knob_track
    };
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
    ctx.draw_text(
        "<",
        box_x + 3.0,
        cy - arrow_size / 2.0,
        arrow_size,
        theme.text_dim,
    );
    let gt_w = ctx.text_width(">", arrow_size);
    ctx.draw_text(
        ">",
        box_x + box_w - gt_w - 3.0,
        cy - arrow_size / 2.0,
        arrow_size,
        theme.text_dim,
    );

    // Label (below)
    let label_size = 9.0;
    let label_w = ctx.text_width(label, label_size);
    ctx.draw_text(
        label,
        cx - label_w / 2.0,
        y + height + 2.0,
        label_size,
        theme.text_dim,
    );
}

/// Draw a dropdown (closed state) — shows current value with a down arrow.
///
/// When open, `draw_dropdown_popup` renders the option list as an overlay.
pub fn draw_dropdown(
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
    is_open: bool,
) {
    let cx = x + width / 2.0;
    let cy = y + height / 2.0 - 8.0;

    let val_size = 10.0;
    let arrow_pad = 14.0;
    let val_w = ctx.text_width(value_text, val_size);
    let box_w = (val_w + arrow_pad + 12.0).max(width - 12.0);
    let box_h = 20.0;
    let box_x = cx - box_w / 2.0;
    let box_y = cy - box_h / 2.0;
    let bg = if is_open || highlighted {
        theme.accent
    } else {
        theme.knob_track
    };
    ctx.fill_rect(box_x, box_y, box_w, box_h, bg);

    // Value text (left-aligned with padding)
    ctx.draw_text(
        value_text,
        box_x + 6.0,
        cy - val_size / 2.0,
        val_size,
        theme.text,
    );

    // Down arrow on the right
    let arrow_size = 8.0;
    let arrow = if is_open { "\u{25B2}" } else { "\u{25BC}" }; // ▲ / ▼
    let aw = ctx.text_width(arrow, arrow_size);
    ctx.draw_text(
        arrow,
        box_x + box_w - aw - 4.0,
        cy - arrow_size / 2.0,
        arrow_size,
        theme.text_dim,
    );

    // Label (below)
    let label_size = 9.0;
    let label_w = ctx.text_width(label, label_size);
    ctx.draw_text(
        label,
        cx - label_w / 2.0,
        y + height + 2.0,
        label_size,
        theme.text_dim,
    );
}

/// Draw the dropdown popup overlay showing visible options.
///
/// `scroll_offset` is the index of the first visible option.
/// `visible_count` is how many options to draw (may be less than total).
pub fn draw_dropdown_popup(
    ctx: &mut dyn RenderBackend,
    x: f32,
    y: f32,
    width: f32,
    options: &[String],
    selected_index: usize,
    hover_index: Option<usize>,
    scroll_offset: usize,
    visible_count: usize,
    theme: &Theme,
) {
    let item_h = 18.0;
    let padding = 4.0;
    let popup_w = width.max(80.0);
    let popup_h = visible_count as f32 * item_h + padding * 2.0;
    let popup_x = x;
    let popup_y = y;

    // Background
    ctx.fill_rect(popup_x, popup_y, popup_w, popup_h, theme.surface);
    // Border
    ctx.draw_line(
        popup_x,
        popup_y,
        popup_x + popup_w,
        popup_y,
        theme.text_dim,
        1.0,
    );
    ctx.draw_line(
        popup_x + popup_w,
        popup_y,
        popup_x + popup_w,
        popup_y + popup_h,
        theme.text_dim,
        1.0,
    );
    ctx.draw_line(
        popup_x + popup_w,
        popup_y + popup_h,
        popup_x,
        popup_y + popup_h,
        theme.text_dim,
        1.0,
    );
    ctx.draw_line(
        popup_x,
        popup_y + popup_h,
        popup_x,
        popup_y,
        theme.text_dim,
        1.0,
    );

    let text_size = 10.0;
    let visible_end = (scroll_offset + visible_count).min(options.len());
    for (vis_i, abs_i) in (scroll_offset..visible_end).enumerate() {
        let iy = popup_y + padding + vis_i as f32 * item_h;

        // Highlight selected or hovered item
        if hover_index == Some(abs_i) {
            ctx.fill_rect(popup_x + 1.0, iy, popup_w - 2.0, item_h, theme.accent);
        } else if abs_i == selected_index {
            ctx.fill_rect(popup_x + 1.0, iy, popup_w - 2.0, item_h, theme.knob_track);
        }

        ctx.draw_text(
            &options[abs_i],
            popup_x + 6.0,
            iy + (item_h - text_size) / 2.0,
            text_size,
            theme.text,
        );
    }

    // Scroll indicators
    let arrow_size = 8.0;
    let cx = popup_x + popup_w / 2.0;
    if scroll_offset > 0 {
        let aw = ctx.text_width("\u{25B2}", arrow_size);
        ctx.draw_text(
            "\u{25B2}",
            cx - aw / 2.0,
            popup_y + 1.0,
            arrow_size,
            theme.text_dim,
        );
    }
    if visible_end < options.len() {
        let aw = ctx.text_width("\u{25BC}", arrow_size);
        ctx.draw_text(
            "\u{25BC}",
            cx - aw / 2.0,
            popup_y + popup_h - arrow_size - 1.0,
            arrow_size,
            theme.text_dim,
        );
    }
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
    let bar_w = 4.0f32;
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
    ctx.draw_text(
        label,
        cx - label_w / 2.0,
        y + height + 4.0,
        label_size,
        theme.text_dim,
    );
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
    let dot_color = if highlighted {
        theme.accent
    } else {
        theme.knob_fill
    };
    ctx.fill_circle(dot_x, dot_y, 3.0, dot_color);
    ctx.fill_circle(dot_x, dot_y, 2.0, theme.knob_pointer);

    // Border
    if highlighted {
        ctx.draw_line(pad_x, pad_y, pad_x + pad_w, pad_y, theme.accent, 1.0);
        ctx.draw_line(
            pad_x + pad_w,
            pad_y,
            pad_x + pad_w,
            pad_y + pad_h,
            theme.accent,
            1.0,
        );
        ctx.draw_line(
            pad_x + pad_w,
            pad_y + pad_h,
            pad_x,
            pad_y + pad_h,
            theme.accent,
            1.0,
        );
        ctx.draw_line(pad_x, pad_y + pad_h, pad_x, pad_y, theme.accent, 1.0);
    }

    // Axis labels: X below the widget (like knob labels), Y at top-left inside pad
    let label_size = 8.0;
    let x_label_w = ctx.text_width(label_x, label_size);
    let cx = x + width / 2.0;
    ctx.draw_text(
        label_x,
        cx - x_label_w / 2.0,
        y + height + 3.0,
        label_size,
        theme.text_dim,
    );

    if !label_y.is_empty() {
        ctx.draw_text(
            label_y,
            pad_x + 2.0,
            pad_y + 1.0,
            label_size,
            theme.text_dim,
        );
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

// ---------------------------------------------------------------------------
// Public compositor — draws an entire layout in one call.
// ---------------------------------------------------------------------------

/// Render every widget in `layout` onto `backend` using `theme`,
/// reading live values from `snapshot` and interaction flags from
/// `state`.
///
/// Does not call `backend.clear()` or `backend.present()` — the caller
/// owns the surrounding frame. This lets plugins with custom renderers
/// draw their own content first (or last) and still get the same widget
/// chrome as `BuiltinEditor`.
///
/// `state.knob_regions` is expected to be up to date for `layout`;
/// callers typically call `state.build_regions_any(layout)` after any
/// layout change. `draw` updates `dropdown_anchor_y` on each region it
/// draws so that subsequent dropdown opens via `interaction::dispatch`
/// position the popup under the current button.
pub fn draw(
    backend: &mut dyn RenderBackend,
    layout: &Layout,
    theme: &Theme,
    snapshot: &ParamSnapshot<'_>,
    state: &mut InteractionState,
) {
    match layout {
        Layout::Rows(pl) => draw_rows(backend, pl, theme, snapshot, state),
        Layout::Grid(gl) => draw_grid(backend, gl, theme, snapshot, state),
    }
    draw_dropdown_overlay(backend, theme, state);
}

fn resolve_wkind_to_type(
    kind: Option<WidgetKind>,
    param_id: u32,
    snapshot: &ParamSnapshot<'_>,
) -> WidgetType {
    match kind {
        Some(WidgetKind::Knob) => WidgetType::Knob,
        Some(WidgetKind::Slider) => WidgetType::Slider,
        Some(WidgetKind::Toggle) => WidgetType::Toggle,
        Some(WidgetKind::Selector) => WidgetType::Selector,
        Some(WidgetKind::Dropdown) => WidgetType::Dropdown,
        Some(WidgetKind::Meter) => WidgetType::Meter,
        Some(WidgetKind::XYPad) => WidgetType::XYPad,
        None => (snapshot.widget_type)(param_id),
    }
}

fn draw_rows(
    backend: &mut dyn RenderBackend,
    pl: &PluginLayout,
    theme: &Theme,
    snapshot: &ParamSnapshot<'_>,
    state: &mut InteractionState,
) {
    let w = pl.width;
    let knob_size = pl.knob_size;
    draw_header(
        backend, 0.0, 0.0, w as f32, 20.0, pl.title, pl.version, theme,
    );

    let mut y = 24.0;
    let mut region_idx = 0usize;

    for row in &pl.rows {
        if let Some(label) = row.label {
            draw_section_label(backend, 0.0, y, w as f32, label, theme);
            y += 14.0;
        }

        let total_cols: u32 = row.knobs.iter().map(|k| k.span.max(1)).sum();
        let total_w = total_cols as f32 * (knob_size + 7.0) - 7.0;
        let start_x = (w as f32 - total_w) / 2.0;

        let mut col = 0u32;
        for kd in row.knobs.iter() {
            let span = kd.span.max(1);
            let x = start_x + col as f32 * (knob_size + 7.0);
            let widget_w = span as f32 * (knob_size + 7.0) - 7.0;
            let widget_h = knob_size;

            draw_widget_entry(
                backend,
                theme,
                snapshot,
                state,
                region_idx,
                x,
                y,
                widget_w,
                widget_h,
                kd.param_id,
                kd.param_id_y,
                kd.meter_ids.as_deref(),
                kd.label,
                kd.widget,
                false, // rows: never center the knob in its cell
            );

            region_idx += 1;
            col += span;
        }

        y += knob_size + 19.0;
    }
}

fn draw_grid(
    backend: &mut dyn RenderBackend,
    grid: &GridLayout,
    theme: &Theme,
    snapshot: &ParamSnapshot<'_>,
    state: &mut InteractionState,
) {
    let w = grid.width;
    draw_header(
        backend,
        0.0,
        0.0,
        w as f32,
        20.0,
        grid.title,
        grid.version,
        theme,
    );

    let section_offsets = compute_section_offsets(grid);

    for &(row_idx, label) in &grid.sections {
        let y = GRID_HEADER_H
            + GRID_PADDING
            + row_idx as f32 * (grid.cell_size + GRID_GAP)
            + section_offsets[row_idx as usize]
            - GRID_SECTION_H;
        draw_section_label(backend, 0.0, y, w as f32, label, theme);
    }

    for (idx, gw) in grid.widgets.iter().enumerate() {
        let x = GRID_PADDING + gw.col as f32 * (grid.cell_size + GRID_GAP);
        let y = GRID_HEADER_H
            + GRID_PADDING
            + gw.row as f32 * (grid.cell_size + GRID_GAP)
            + section_offsets[gw.row as usize];
        let widget_w = gw.col_span as f32 * (grid.cell_size + GRID_GAP) - GRID_GAP;
        let widget_h = gw.row_span as f32 * (grid.cell_size + GRID_GAP) - GRID_GAP;

        draw_widget_entry(
            backend,
            theme,
            snapshot,
            state,
            idx,
            x,
            y,
            widget_w,
            widget_h,
            gw.param_id,
            gw.param_id_y,
            gw.meter_ids.as_deref(),
            gw.label,
            gw.widget,
            true, // grid: center knobs within their cell
        );
    }
}

#[allow(clippy::too_many_arguments)]
fn draw_widget_entry(
    backend: &mut dyn RenderBackend,
    theme: &Theme,
    snapshot: &ParamSnapshot<'_>,
    state: &mut InteractionState,
    region_idx: usize,
    x: f32,
    y: f32,
    widget_w: f32,
    widget_h: f32,
    param_id: u32,
    param_id_y: Option<u32>,
    meter_ids: Option<&[u32]>,
    label: &'static str,
    explicit_kind: Option<WidgetKind>,
    center_knob_in_cell: bool,
) {
    let normalized = (snapshot.get_param)(param_id);
    let value_text = (snapshot.format_param)(param_id);
    let is_hovered = state.hover_idx == Some(region_idx);
    let wtype = resolve_wkind_to_type(explicit_kind, param_id, snapshot);

    match wtype {
        WidgetType::Toggle => draw_toggle(
            backend,
            x,
            y,
            widget_w,
            widget_h,
            normalized,
            label,
            &value_text,
            theme,
            is_hovered,
        ),
        WidgetType::Slider => draw_slider(
            backend,
            x,
            y,
            widget_w,
            widget_h,
            normalized,
            label,
            &value_text,
            theme,
            is_hovered,
        ),
        WidgetType::Selector => draw_selector(
            backend,
            x,
            y,
            widget_w,
            widget_h,
            normalized,
            label,
            &value_text,
            theme,
            is_hovered,
        ),
        WidgetType::Dropdown => {
            let is_open = state
                .dropdown
                .as_ref()
                .map_or(false, |dd| dd.region_idx == region_idx);
            draw_dropdown(
                backend,
                x,
                y,
                widget_w,
                widget_h,
                normalized,
                label,
                &value_text,
                theme,
                is_hovered,
                is_open,
            );
            let anchor_cy = y + widget_h / 2.0 - 8.0;
            if let Some(region) = state.knob_regions.get_mut(region_idx) {
                region.dropdown_anchor_y = anchor_cy + 10.0;
            }
        }
        WidgetType::Meter => {
            let fallback = [param_id];
            let ids = meter_ids.unwrap_or(&fallback);
            let levels: Vec<f32> = ids.iter().map(|&id| (snapshot.get_meter)(id)).collect();
            draw_meter(backend, x, y, widget_w, widget_h, &levels, label, theme);
        }
        WidgetType::XYPad => {
            let val_y_id = param_id_y.unwrap_or(param_id);
            let vx = (snapshot.get_param)(param_id);
            let vy = (snapshot.get_param)(val_y_id);
            let x_name_str = (snapshot.param_name)(param_id);
            let y_name_str = (snapshot.param_name)(val_y_id);
            let x_name: &str = if x_name_str.is_empty() {
                label
            } else {
                &x_name_str
            };
            let y_name: &str = &y_name_str;
            draw_xy_pad(
                backend, x, y, widget_w, widget_h, vx, vy, x_name, y_name, theme, is_hovered,
            );
        }
        WidgetType::Knob => {
            if center_knob_in_cell {
                let knob_size = widget_w.min(widget_h);
                let kx = x + (widget_w - knob_size) / 2.0;
                let ky = y + (widget_h - knob_size) / 2.0;
                draw_knob(
                    backend,
                    kx,
                    ky,
                    knob_size,
                    normalized,
                    label,
                    &value_text,
                    theme,
                    is_hovered,
                );
            } else {
                draw_knob(
                    backend,
                    x,
                    y,
                    widget_h,
                    normalized,
                    label,
                    &value_text,
                    theme,
                    is_hovered,
                );
            }
        }
    }
}

fn draw_dropdown_overlay(backend: &mut dyn RenderBackend, theme: &Theme, state: &InteractionState) {
    if let Some(ref dd) = state.dropdown {
        let (px, py, pw, _) = dd.popup_rect;
        draw_dropdown_popup(
            backend,
            px,
            py,
            pw,
            &dd.options,
            dd.selected,
            dd.hover_option,
            dd.scroll_offset,
            dd.visible_count,
            theme,
        );
    }
}
