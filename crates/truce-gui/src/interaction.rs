//! Mouse interaction for GUI widgets.
//!
//! Tracks widget hit regions and maps mouse drags to parameter value changes.

use crate::layout::{GridLayout, Layout, PluginLayout, compute_section_offsets,
                     GRID_GAP, GRID_PADDING, GRID_HEADER_H};
use crate::snapshot::ParamSnapshot;
use crate::widgets::WidgetType;

// ---------------------------------------------------------------------------
// Platform-agnostic input events + edit outputs
// ---------------------------------------------------------------------------

/// Which mouse button triggered an event.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MouseButton {
    Left,
    Right,
    Middle,
}

/// Keyboard modifier state at event time.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Modifiers {
    pub shift: bool,
    pub ctrl: bool,
    pub alt: bool,
    pub meta: bool,
}

/// Platform-agnostic input event consumed by `dispatch`.
///
/// Cursor coordinates are in logical pixels, matching what widgets draw at.
#[derive(Clone, Copy, Debug)]
pub enum InputEvent {
    MouseMove { x: f32, y: f32 },
    MouseDown { x: f32, y: f32, button: MouseButton },
    MouseUp { x: f32, y: f32, button: MouseButton },
    /// Synthesized when the host detects a second click within the
    /// platform-specific threshold. `dispatch` uses this to reset params
    /// to their defaults.
    MouseDoubleClick { x: f32, y: f32 },
    /// Vertical wheel scroll. `dy > 0` = scroll up (away from user),
    /// `dy < 0` = scroll down. Magnitude is in pixels.
    Scroll { x: f32, y: f32, dy: f32 },
    /// The cursor left the editor surface. Dispatch clears hover state.
    MouseLeave,
}

/// A requested edit to a host parameter, emitted by `dispatch`.
///
/// Callers replay these against their host interface:
/// `Begin → Set* → End` matches the VST3 / CLAP / AU automation protocol.
#[derive(Clone, Copy, Debug)]
pub enum ParamEdit {
    /// Parameter is about to be edited (begin gesture).
    Begin { id: u32 },
    /// Set normalized value.
    Set { id: u32, normalized: f32 },
    /// Edit gesture finished.
    End { id: u32 },
}

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
                y += 14.0;
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

    /// Rebuild hit regions from either layout variant.
    pub fn build_regions_any(&mut self, layout: &Layout) {
        match layout {
            Layout::Rows(pl) => self.build_regions(pl),
            Layout::Grid(gl) => self.build_regions_grid(gl),
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

// ---------------------------------------------------------------------------
// Public `dispatch` — drive widget interactions from input events.
// ---------------------------------------------------------------------------

/// Route a batch of input events through the widget tree, updating
/// `state` in place (hover, drag origins, dropdown open/closed, …) and
/// returning the sequence of parameter edits they imply.
///
/// `state.knob_regions` must be up to date for the current layout; callers
/// typically call `state.build_regions_any(layout)` once after a layout
/// change. `snapshot` provides read access to live parameter values.
///
/// This does NOT mutate any parameter store. Callers replay the returned
/// `ParamEdit`s against their host interface.
pub fn dispatch(
    events: &[InputEvent],
    layout: &Layout,
    snapshot: &ParamSnapshot<'_>,
    state: &mut InteractionState,
) -> Vec<ParamEdit> {
    let mut edits = Vec::new();
    let window_w = layout.width() as f32;
    let window_h = layout.height() as f32;

    for ev in events {
        match *ev {
            InputEvent::MouseMove { x, y } => {
                let drag_info = state
                    .dragging
                    .as_ref()
                    .map(|d| (d.widget_type, d.region_idx));
                if let Some((wtype, region_idx)) = drag_info {
                    let y_id = if wtype == WidgetType::XYPad {
                        layout_param_id_y(layout, region_idx)
                    } else {
                        None
                    };
                    apply_drag(x, y, y_id, state, &mut edits);
                } else {
                    if state.dropdown_is_open() {
                        state.dropdown_update_hover(x, y);
                    }
                    state.hover_idx = state.hit_test(x, y);
                }
            }
            InputEvent::MouseDown { x, y, button: MouseButton::Left } => {
                handle_mouse_down(x, y, layout, snapshot, state, window_w, window_h, &mut edits);
            }
            InputEvent::MouseUp { button: MouseButton::Left, .. } => {
                if let Some(drag) = state.dragging.as_ref() {
                    let param_id = drag.param_id;
                    let was_xy = drag.widget_type == WidgetType::XYPad;
                    let region_idx = drag.region_idx;
                    state.end_drag();
                    edits.push(ParamEdit::End { id: param_id });
                    if was_xy {
                        if let Some(y_id) = layout_param_id_y(layout, region_idx) {
                            edits.push(ParamEdit::End { id: y_id });
                        }
                    }
                }
            }
            InputEvent::MouseDoubleClick { x, y } => {
                if let Some(idx) = state.hit_test(x, y) {
                    let param_id = state.knob_regions[idx].param_id;
                    let default_norm = (snapshot.default_normalized)(param_id);
                    edits.push(ParamEdit::Begin { id: param_id });
                    edits.push(ParamEdit::Set { id: param_id, normalized: default_norm });
                    edits.push(ParamEdit::End { id: param_id });
                }
            }
            InputEvent::Scroll { x, y, dy } => {
                if state.dropdown_is_open() {
                    if state.dropdown_popup_hit(x, y).is_some()
                        || state.dropdown.as_ref().map_or(false, |dd| {
                            let (px, py, pw, ph) = dd.popup_rect;
                            x >= px && x <= px + pw && y >= py && y <= py + ph
                        })
                    {
                        let delta = if dy > 0.0 { -1 } else { 1 };
                        state.dropdown_scroll(delta);
                        continue;
                    }
                }
                if let Some(idx) = state.hit_test(x, y) {
                    let param_id = state.knob_regions[idx].param_id;
                    let norm = (snapshot.get_param)(param_id);
                    let step = dy / 200.0;
                    let new_norm = (norm + step).clamp(0.0, 1.0);
                    edits.push(ParamEdit::Begin { id: param_id });
                    edits.push(ParamEdit::Set { id: param_id, normalized: new_norm });
                    edits.push(ParamEdit::End { id: param_id });
                }
            }
            InputEvent::MouseLeave => {
                state.hover_idx = None;
            }
            // Non-left mouse buttons are currently not wired to any action.
            InputEvent::MouseDown { .. } | InputEvent::MouseUp { .. } => {}
        }
    }

    edits
}

/// Mouse-down handling factored out of the big match so it's readable.
fn handle_mouse_down(
    x: f32,
    y: f32,
    layout: &Layout,
    snapshot: &ParamSnapshot<'_>,
    state: &mut InteractionState,
    window_w: f32,
    window_h: f32,
    edits: &mut Vec<ParamEdit>,
) {
    // If a dropdown popup is open, handle it first.
    if state.dropdown_is_open() {
        if let Some(option_idx) = state.dropdown_popup_hit(x, y) {
            let dd = state.dropdown.as_ref().unwrap();
            let param_id = dd.param_id;
            let count = dd.options.len();
            let new_norm = if count <= 1 {
                0.0
            } else {
                option_idx as f32 / (count - 1) as f32
            };
            edits.push(ParamEdit::Begin { id: param_id });
            edits.push(ParamEdit::Set { id: param_id, normalized: new_norm });
            edits.push(ParamEdit::End { id: param_id });
            state.dropdown_close();
            return;
        }
        // Click outside popup: close. If it landed on the same dropdown
        // button, swallow the click (don't reopen).
        let open_region = state.dropdown.as_ref().unwrap().region_idx;
        state.dropdown_close();
        if let Some(idx) = state.hit_test(x, y) {
            if idx == open_region && state.widget_type_at(idx) == Some(WidgetType::Dropdown) {
                return;
            }
        }
        // Fall through to normal widget hit-test.
    }

    let idx = match state.hit_test(x, y) {
        Some(i) => i,
        None => return,
    };
    let param_id = state.knob_regions[idx].param_id;
    let wtype = state.widget_type_at(idx);

    match wtype {
        Some(WidgetType::Toggle) => {
            let norm = (snapshot.get_param)(param_id);
            let new_norm = if norm > 0.5 { 0.0 } else { 1.0 };
            edits.push(ParamEdit::Begin { id: param_id });
            edits.push(ParamEdit::Set { id: param_id, normalized: new_norm });
            edits.push(ParamEdit::End { id: param_id });
        }
        Some(WidgetType::Selector) => {
            let new_norm = (snapshot.next_discrete_normalized)(param_id);
            edits.push(ParamEdit::Begin { id: param_id });
            edits.push(ParamEdit::Set { id: param_id, normalized: new_norm });
            edits.push(ParamEdit::End { id: param_id });
        }
        Some(WidgetType::Dropdown) => {
            open_dropdown(idx, param_id, snapshot, state, window_w, window_h);
        }
        _ => {
            // Knob / Slider / XYPad / Meter: begin a drag.
            let norm = (snapshot.get_param)(param_id) as f64;
            state.begin_drag(idx, norm, y);
            edits.push(ParamEdit::Begin { id: param_id });
            if wtype == Some(WidgetType::XYPad) {
                if let Some(y_id) = layout_param_id_y(layout, idx) {
                    edits.push(ParamEdit::Begin { id: y_id });
                }
            }
        }
    }
}

fn open_dropdown(
    region_idx: usize,
    param_id: u32,
    snapshot: &ParamSnapshot<'_>,
    state: &mut InteractionState,
    window_w: f32,
    window_h: f32,
) {
    let options = (snapshot.get_options)(param_id);
    if options.is_empty() {
        return;
    }
    let count = options.len();
    let current_norm = (snapshot.get_param)(param_id);
    let selected = (current_norm * (count - 1).max(1) as f32).round() as usize;
    let region = &state.knob_regions[region_idx];

    let item_h = 18.0f32;
    let padding = 4.0f32;

    let anchor_below = region.dropdown_anchor_y; // bottom of button box
    let anchor_above = anchor_below - 20.0;      // top of button box (box_h=20)
    let popup_w = region.w.max(80.0);
    let full_popup_h = options.len() as f32 * item_h + padding * 2.0;

    let (popup_y, avail_h) = if anchor_below + full_popup_h <= window_h {
        (anchor_below, full_popup_h)
    } else if anchor_above - full_popup_h >= 0.0 {
        (anchor_above - full_popup_h, full_popup_h)
    } else {
        let space_below = window_h - anchor_below;
        let space_above = anchor_above;
        if space_below >= space_above {
            (anchor_below, space_below.max(item_h + padding * 2.0))
        } else {
            let h = space_above.max(item_h + padding * 2.0);
            (anchor_above - h, h)
        }
    };

    let visible_count = ((avail_h - padding * 2.0) / item_h).floor().max(1.0) as usize;
    let visible_count = visible_count.min(options.len());
    let popup_h = visible_count as f32 * item_h + padding * 2.0;

    let popup_x = region.x.clamp(0.0, (window_w - popup_w).max(0.0));
    let scroll_offset = if selected >= visible_count {
        selected - visible_count + 1
    } else {
        0
    };

    state.dropdown = Some(DropdownState {
        region_idx,
        param_id,
        popup_rect: (popup_x, popup_y, popup_w, popup_h),
        options,
        selected,
        hover_option: None,
        scroll_offset,
        visible_count,
    });
}

fn apply_drag(
    x: f32,
    y: f32,
    y_id_for_xy: Option<u32>,
    state: &InteractionState,
    edits: &mut Vec<ParamEdit>,
) {
    let drag = match state.dragging.as_ref() {
        Some(d) => d,
        None => return,
    };
    match drag.widget_type {
        WidgetType::XYPad => {
            let pad_margin = 4.0;
            let label_h = 18.0;
            let pad_x = drag.region_x + pad_margin;
            let pad_w = drag.region_w - pad_margin * 2.0;
            let pad_y_start = drag.region_y + pad_margin;
            let pad_h = drag.region_h - pad_margin * 2.0 - label_h;

            let norm_x = ((x - pad_x) / pad_w).clamp(0.0, 1.0);
            let norm_y = (1.0 - (y - pad_y_start) / pad_h).clamp(0.0, 1.0);

            edits.push(ParamEdit::Set { id: drag.param_id, normalized: norm_x });
            if let Some(y_id) = y_id_for_xy {
                edits.push(ParamEdit::Set { id: y_id, normalized: norm_y });
            }
        }
        WidgetType::Slider => {
            if let Some((pid, new_norm)) = state.update_slider_drag(x) {
                edits.push(ParamEdit::Set { id: pid, normalized: new_norm as f32 });
            }
        }
        _ => {
            if let Some((pid, new_norm)) = state.update_drag(y) {
                edits.push(ParamEdit::Set { id: pid, normalized: new_norm as f32 });
            }
        }
    }
}

/// Look up the Y-axis parameter ID for a widget at `region_idx` in the layout.
/// Returns `None` if the widget is not an XY pad (or the index is invalid).
pub(crate) fn layout_param_id_y(layout: &Layout, region_idx: usize) -> Option<u32> {
    match layout {
        Layout::Rows(pl) => {
            let mut i = 0;
            for row in &pl.rows {
                for kd in &row.knobs {
                    if i == region_idx {
                        return kd.param_id_y;
                    }
                    i += 1;
                }
            }
            None
        }
        Layout::Grid(g) => g.widgets.get(region_idx).and_then(|w| w.param_id_y),
    }
}
