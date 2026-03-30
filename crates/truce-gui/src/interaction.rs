//! Mouse interaction for GUI widgets.
//!
//! Tracks widget hit regions and maps mouse drags to parameter value changes.

use crate::layout::{GridLayout, PluginLayout, compute_section_offsets,
                     GRID_GAP, GRID_PADDING, GRID_HEADER_H};
use crate::widgets::WidgetType;

/// A widget's hit region on screen.
#[derive(Clone, Debug)]
pub struct WidgetRegion {
    pub param_id: u32,
    pub widget_type: WidgetType,
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
    /// Center x/y and radius for knob (circular hit test).
    pub cx: f32,
    pub cy: f32,
    pub radius: f32,
    pub normalized_value: f32,
    /// Bottom Y of the dropdown button box, set at draw time.
    /// Used to position the popup directly below the visual button.
    pub dropdown_anchor_y: f32,
}

/// Backward-compatible alias.
pub type KnobRegion = WidgetRegion;

/// State for an open dropdown popup.
pub struct DropdownState {
    /// Region index of the dropdown widget that is open.
    pub region_idx: usize,
    /// Parameter ID of the open dropdown.
    pub param_id: u32,
    /// Popup bounding rect: (x, y, w, h).
    pub popup_rect: (f32, f32, f32, f32),
    /// Option labels.
    pub options: Vec<String>,
    /// Currently selected index.
    pub selected: usize,
    /// Index under the cursor within the popup.
    pub hover_option: Option<usize>,
    /// First visible option index (for scrollable popups).
    pub scroll_offset: usize,
    /// Number of visible options (may be less than options.len() if clamped).
    pub visible_count: usize,
}

/// Tracks the current mouse interaction state.
pub struct InteractionState {
    pub knob_regions: Vec<WidgetRegion>,
    pub dragging: Option<DragState>,
    /// Region index under the cursor (for hover highlight).
    pub hover_idx: Option<usize>,
    /// Currently open dropdown popup (at most one at a time).
    pub dropdown: Option<DropdownState>,
}

pub struct DragState {
    pub region_idx: usize,
    pub param_id: u32,
    pub start_value: f64,
    pub start_y: f32,
    pub widget_type: WidgetType,
    pub region_x: f32,
    pub region_y: f32,
    pub region_w: f32,
    pub region_h: f32,
}

impl Default for InteractionState {
    fn default() -> Self {
        Self::new()
    }
}

impl InteractionState {
    pub fn new() -> Self {
        Self {
            knob_regions: Vec::new(),
            dragging: None,
            hover_idx: None,
            dropdown: None,
        }
    }

    /// Rebuild hit regions from the layout. Call after render.
    pub fn build_regions(&mut self, layout: &PluginLayout) {
        self.knob_regions.clear();

        let knob_size = layout.knob_size;
        let mut y = 24.0f32;

        for row in &layout.rows {
            if row.label.is_some() {
                y += 12.0;
            }

            let total_cols: u32 = row.knobs.iter().map(|k| k.span.max(1)).sum();
            let total_w = total_cols as f32 * (knob_size + 7.0) - 7.0;
            let start_x = (layout.width as f32 - total_w) / 2.0;

            let mut col = 0u32;
            for knob_def in row.knobs.iter() {
                let span = knob_def.span.max(1);
                let x = start_x + col as f32 * (knob_size + 7.0);
                let widget_w = span as f32 * (knob_size + 7.0) - 7.0;
                let cx = x + widget_w / 2.0;
                let cy = y + knob_size / 2.0 - 5.0;
                let radius = knob_size / 2.0 - 4.0;

                self.knob_regions.push(WidgetRegion {
                    param_id: knob_def.param_id,
                    widget_type: WidgetType::Knob,
                    x,
                    y,
                    w: widget_w,
                    h: knob_size,
                    cx,
                    cy,
                    radius,
                    normalized_value: 0.0,
                dropdown_anchor_y: 0.0,
                });
                col += span;
            }

            y += knob_size + 19.0;
        }
    }

    /// Check if a mouse position hits a widget. Returns the region index if so.
    pub fn hit_test(&self, mx: f32, my: f32) -> Option<usize> {
        for (idx, region) in self.knob_regions.iter().enumerate() {
            match region.widget_type {
                WidgetType::Knob => {
                    let dx = mx - region.cx;
                    let dy = my - region.cy;
                    if dx * dx + dy * dy <= region.radius * region.radius {
                        return Some(idx);
                    }
                }
                WidgetType::Meter => continue,
                WidgetType::Slider | WidgetType::Toggle | WidgetType::Selector
                | WidgetType::Dropdown | WidgetType::XYPad => {
                    if mx >= region.x && mx <= region.x + region.w
                        && my >= region.y && my <= region.y + region.h
                    {
                        return Some(idx);
                    }
                }
            }
        }
        None
    }

    /// Get the widget type by region index.
    pub fn widget_type_at(&self, idx: usize) -> Option<WidgetType> {
        self.knob_regions.get(idx).map(|r| r.widget_type)
    }

    /// Get the region by index.
    pub fn region_at(&self, idx: usize) -> Option<&WidgetRegion> {
        self.knob_regions.get(idx)
    }

    /// Begin a drag on a widget by region index.
    pub fn begin_drag(&mut self, idx: usize, current_normalized: f64, mouse_y: f32) {
        let region = match self.knob_regions.get(idx) {
            Some(r) => r,
            None => return,
        };
        let param_id = region.param_id;
        let wtype = region.widget_type;
        self.dragging = Some(DragState {
            region_idx: idx,
            param_id,
            start_value: current_normalized,
            start_y: mouse_y,
            widget_type: wtype,
            region_x: region.x,
            region_y: region.y,
            region_w: region.w,
            region_h: region.h,
        });
    }

    /// Update during a drag. Returns (param_id, new_normalized_value) if dragging.
    pub fn update_drag(&self, mouse_y: f32) -> Option<(u32, f64)> {
        let drag = self.dragging.as_ref()?;
        let dy = drag.start_y - mouse_y;
        let delta = dy as f64 / 200.0;
        let new_value = (drag.start_value + delta).clamp(0.0, 1.0);
        Some((drag.param_id, new_value))
    }

    /// Update during a horizontal slider drag. Returns (param_id, new_value).
    pub fn update_slider_drag(&self, mouse_x: f32) -> Option<(u32, f64)> {
        let drag = self.dragging.as_ref()?;
        let margin = 4.0;
        let rel = (mouse_x - drag.region_x - margin) / (drag.region_w - margin * 2.0);
        let new_value = (rel as f64).clamp(0.0, 1.0);
        Some((drag.param_id, new_value))
    }

    /// End a drag.
    pub fn end_drag(&mut self) {
        self.dragging = None;
    }

    /// Test if a point is inside the open dropdown popup.
    /// Returns the absolute option index (accounting for scroll) if hit, or None.
    pub fn dropdown_popup_hit(&self, mx: f32, my: f32) -> Option<usize> {
        let dd = self.dropdown.as_ref()?;
        let (px, py, pw, ph) = dd.popup_rect;
        if mx < px || mx > px + pw || my < py || my > py + ph {
            return None;
        }
        let item_h = 18.0f32;
        let padding = 4.0f32;
        let local_idx = ((my - py - padding) / item_h) as usize;
        let abs_idx = dd.scroll_offset + local_idx;
        if abs_idx < dd.options.len() && local_idx < dd.visible_count {
            Some(abs_idx)
        } else {
            None
        }
    }

    /// Update the hovered option in the open dropdown popup.
    pub fn dropdown_update_hover(&mut self, mx: f32, my: f32) {
        if let Some(ref mut dd) = self.dropdown {
            let (px, py, pw, ph) = dd.popup_rect;
            if mx >= px && mx <= px + pw && my >= py && my <= py + ph {
                let item_h = 18.0f32;
                let padding = 4.0f32;
                let local_idx = ((my - py - padding) / item_h) as usize;
                let abs_idx = dd.scroll_offset + local_idx;
                dd.hover_option = if abs_idx < dd.options.len() && local_idx < dd.visible_count {
                    Some(abs_idx)
                } else {
                    None
                };
            } else {
                dd.hover_option = None;
            }
        }
    }

    /// Whether a dropdown popup is currently open.
    pub fn dropdown_is_open(&self) -> bool {
        self.dropdown.is_some()
    }

    /// Close the dropdown popup.
    pub fn dropdown_close(&mut self) {
        self.dropdown = None;
    }

    /// Scroll the dropdown popup by `delta` items (positive = down, negative = up).
    pub fn dropdown_scroll(&mut self, delta: i32) {
        if let Some(ref mut dd) = self.dropdown {
            let max_offset = dd.options.len().saturating_sub(dd.visible_count);
            let new_offset = (dd.scroll_offset as i32 + delta).clamp(0, max_offset as i32) as usize;
            dd.scroll_offset = new_offset;
        }
    }

    /// Rebuild hit regions from a grid layout.
    pub fn build_regions_grid(&mut self, layout: &GridLayout) {
        self.knob_regions.clear();

        let section_offsets = compute_section_offsets(layout);

        for gw in &layout.widgets {
            let x = GRID_PADDING + gw.col as f32 * (layout.cell_size + GRID_GAP);
            let y = GRID_HEADER_H + GRID_PADDING
                + gw.row as f32 * (layout.cell_size + GRID_GAP)
                + section_offsets[gw.row as usize];
            let w = gw.col_span as f32 * (layout.cell_size + GRID_GAP) - GRID_GAP;
            let h = gw.row_span as f32 * (layout.cell_size + GRID_GAP) - GRID_GAP;
            let cx = x + w / 2.0;
            let cy = y + h / 2.0 - 5.0;
            let radius = w.min(h) / 2.0 - 4.0;

            self.knob_regions.push(WidgetRegion {
                param_id: gw.param_id,
                widget_type: WidgetType::Knob, // set later by editor
                x, y, w, h,
                cx, cy, radius,
                normalized_value: 0.0,
                dropdown_anchor_y: 0.0,
            });
        }
    }
}
