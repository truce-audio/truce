//! Mouse interaction for GUI widgets.
//!
//! Tracks widget hit regions and maps mouse drags to parameter value changes.

use truce_core::Float;
use truce_core::cast::{discrete_index, discrete_norm};

use crate::layout::{
    GRID_GAP, GRID_PADDING, GridLayout, Layout, PluginLayout, ROWS_COLUMN_GAP, ROWS_LAYOUT_TOP,
    ROWS_ROW_GAP, ROWS_SECTION_LABEL_HEIGHT, WidgetKind, compute_section_offsets,
};
use crate::snapshot::ParamSnapshot;
use crate::widgets::WidgetType;

/// Lower an explicit `WidgetKind` from a layout helper into the
/// runtime `WidgetType` the interaction code dispatches on. `None`
/// (meaning "infer from param range") stays as Knob - callers that
/// need inference overwrite `widget_type` after calling
/// `build_regions_*`.
//
// `Some(Knob) => Knob` and `None => Knob` share a value but mean
// different things - explicit user-specified Knob vs. an
// inference-pending placeholder. Keep the arms separate so the
// distinction is greppable.
#[allow(clippy::match_same_arms)]
fn widget_kind_to_type(kind: Option<WidgetKind>) -> WidgetType {
    match kind {
        Some(WidgetKind::Knob) => WidgetType::Knob,
        Some(WidgetKind::Slider) => WidgetType::Slider,
        Some(WidgetKind::Toggle) => WidgetType::Toggle,
        Some(WidgetKind::Selector) => WidgetType::Selector,
        Some(WidgetKind::Dropdown) => WidgetType::Dropdown,
        Some(WidgetKind::Meter) => WidgetType::Meter,
        Some(WidgetKind::XYPad) => WidgetType::XYPad,
        None => WidgetType::Knob,
    }
}

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
// Standard four modifier flags - bitflags would just add ceremony.
#[allow(clippy::struct_excessive_bools)]
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
///
/// `pointer_id` distinguishes simultaneous pointers (multi-touch).
/// Mouse-driven flows always pass [`SINGLE_POINTER`] (= 0); iOS touch
/// dispatch uses the `UITouch*` cast to `u64` so each finger gets a
/// stable identifier across `Down → Move → Up`.
#[derive(Clone, Copy, Debug)]
pub enum InputEvent {
    MouseMove {
        pointer_id: u64,
        x: f32,
        y: f32,
    },
    MouseDown {
        pointer_id: u64,
        x: f32,
        y: f32,
        button: MouseButton,
    },
    MouseUp {
        pointer_id: u64,
        x: f32,
        y: f32,
        button: MouseButton,
    },
    /// Synthesized when the host detects a second click within the
    /// platform-specific threshold. `dispatch` uses this to reset params
    /// to their defaults.
    MouseDoubleClick {
        x: f32,
        y: f32,
    },
    /// Vertical wheel scroll. `dy > 0` = scroll up (away from user),
    /// `dy < 0` = scroll down. Magnitude is in pixels.
    Scroll {
        x: f32,
        y: f32,
        dy: f32,
    },
    /// The cursor left the editor surface. Dispatch clears hover state.
    MouseLeave,
}

/// Single-pointer sentinel for mouse-driven flows. iOS touch
/// dispatch substitutes the `UITouch*` cast to `u64` so multiple
/// fingers can drag independently.
pub const SINGLE_POINTER: u64 = 0;

/// Pixels of vertical drag (or wheel travel) that map to a full
/// 0.0 → 1.0 normalized parameter range. Shared between knob drag
/// and the scroll-wheel knob adjustment so the two feel uniform.
const KNOB_PIXELS_PER_UNIT: f32 = 200.0;

// The `BaseviewTranslator` lives in `truce-gui` (heavy crate) because
// it depends on `baseview` for windowing-platform event translation.
// Light backends (truce-egui, truce-iced, truce-slint) don't use it
// - they translate their own framework's events into `InputEvent`s
// and call `dispatch` directly.

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
    /// Number of visible options (may be less than `options.len()` if clamped).
    pub visible_count: usize,
}

/// Tracks the current mouse / touch interaction state.
#[derive(Default)]
pub struct InteractionState {
    pub knob_regions: Vec<WidgetRegion>,
    /// One entry per active pointer (mouse: at most 1; touch: up
    /// to one per finger). Keyed by `DragState::pointer_id`. Linear
    /// scan - N is bounded by the device's reported max touches
    /// (≤10 in practice).
    pub drags: Vec<DragState>,
    /// Region index under the cursor (for hover highlight).
    pub hover_idx: Option<usize>,
    /// Currently open dropdown popup (at most one at a time).
    pub dropdown: Option<DropdownState>,
    /// Active touch-drag on the open dropdown popup - set on
    /// `MouseDown` inside the popup, updated on `MouseMove`
    /// (mapping vertical motion to `scroll_offset` change),
    /// cleared on `MouseUp`. iOS pattern: tap to select, swipe to
    /// scroll. Desktop scroll-wheel handling stays through the
    /// `Scroll` event.
    pub popup_drag: Option<PopupDrag>,
    /// Set by event handlers whose visible side effect isn't otherwise
    /// observable to `dispatch_events` (e.g. `MouseLeave` clearing
    /// hover state). The editor reads this via `take_repaint_request`
    /// to avoid relying on diff-checks of every individual visible
    /// field.
    needs_repaint: bool,
}

/// Active touch-drag on the open dropdown popup.
pub struct PopupDrag {
    pub pointer_id: u64,
    pub start_y: f32,
    pub start_scroll_offset: usize,
    /// True once the user has moved more than `ITEM_H / 2` from
    /// `start_y`. Distinguishes a tap (select on release) from a
    /// scroll-drag (keep popup open on release).
    pub scrolled: bool,
}

pub struct DragState {
    /// Identifier of the pointer (mouse or touch) driving this drag.
    /// See [`SINGLE_POINTER`].
    pub pointer_id: u64,
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

impl InteractionState {
    /// Read and clear the explicit repaint flag set by event handlers.
    pub fn take_repaint_request(&mut self) -> bool {
        std::mem::replace(&mut self.needs_repaint, false)
    }

    /// Rebuild hit regions from the layout. Call after render.
    // Layout col counts widen `u32 as f32`; column counts are
    // bounded by the editor's row width.
    #[allow(clippy::cast_precision_loss)]
    pub fn build_regions(&mut self, layout: &PluginLayout) {
        // `dropdown_anchor_y` is filled in by the draw pass, not here.
        // `update_interaction` rebuilds regions every frame, but the
        // render that repopulates the anchor can be skipped (the macOS
        // CPU path gates `render` behind a repaint check). Carry prior
        // anchors over by index so an idle, non-rendering frame doesn't
        // reset them to 0 and strand the next dropdown popup at the top
        // of the window.
        let prior_anchors: Vec<f32> = self
            .knob_regions
            .iter()
            .map(|r| r.dropdown_anchor_y)
            .collect();
        self.knob_regions.clear();

        let knob_size = layout.knob_size;
        let pitch = knob_size + ROWS_COLUMN_GAP;
        let mut y = ROWS_LAYOUT_TOP;

        for row in &layout.rows {
            if row.label.is_some() {
                y += ROWS_SECTION_LABEL_HEIGHT;
            }

            let total_cols: u32 = row.knobs.iter().map(|k| k.span.max(1)).sum();
            let total_w = total_cols as f32 * pitch - ROWS_COLUMN_GAP;
            let start_x = (layout.width as f32 - total_w) / 2.0;

            let mut col = 0u32;
            for knob_def in &row.knobs {
                let span = knob_def.span.max(1);
                let x = start_x + col as f32 * pitch;
                let widget_w = span as f32 * pitch - ROWS_COLUMN_GAP;
                let cx = x + widget_w / 2.0;
                let cy = y + knob_size / 2.0 - 5.0;
                let radius = knob_size / 2.0 - 4.0;

                let idx = self.knob_regions.len();
                self.knob_regions.push(WidgetRegion {
                    param_id: knob_def.param_id,
                    widget_type: widget_kind_to_type(knob_def.widget),
                    x,
                    y,
                    w: widget_w,
                    h: knob_size,
                    cx,
                    cy,
                    radius,
                    normalized_value: 0.0,
                    dropdown_anchor_y: prior_anchors.get(idx).copied().unwrap_or(0.0),
                });
                col += span;
            }

            y += knob_size + ROWS_ROW_GAP;
        }
    }

    /// Check if a mouse position hits a widget. Returns the region index if so.
    #[must_use]
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
                WidgetType::Meter => {}
                WidgetType::Slider
                | WidgetType::Toggle
                | WidgetType::Selector
                | WidgetType::Dropdown
                | WidgetType::XYPad => {
                    if mx >= region.x
                        && mx <= region.x + region.w
                        && my >= region.y
                        && my <= region.y + region.h
                    {
                        return Some(idx);
                    }
                }
            }
        }
        None
    }

    /// Get the widget type by region index.
    #[must_use]
    pub fn widget_type_at(&self, idx: usize) -> Option<WidgetType> {
        self.knob_regions.get(idx).map(|r| r.widget_type)
    }

    /// Get the region by index.
    #[must_use]
    pub fn region_at(&self, idx: usize) -> Option<&WidgetRegion> {
        self.knob_regions.get(idx)
    }

    /// Begin a drag on a widget by region index. Returns any prior
    /// drag for the same `pointer_id` so the caller can emit a
    /// matching `ParamEdit::End` for it - without this, hosts that
    /// model gestures as a Begin/End stack (VST3, CLAP, AU on iOS)
    /// see a stranded Begin and report the param as permanently
    /// "being touched". iOS reliably triggers this when a system
    /// gesture recognizer (Control Center swipe, multitasking
    /// gesture) steals a touch without firing `touchesCancelled:`;
    /// the next `touchesBegan:` may reuse the same `UITouch*`
    /// pointer for a different finger.
    #[must_use]
    pub fn begin_drag(
        &mut self,
        pointer_id: u64,
        idx: usize,
        current_normalized: f64,
        mouse_y: f32,
    ) -> Option<DragState> {
        let region = self.knob_regions.get(idx)?;
        let param_id = region.param_id;
        let wtype = region.widget_type;
        let stranded = self
            .drags
            .iter()
            .position(|d| d.pointer_id == pointer_id)
            .map(|i| self.drags.swap_remove(i));
        self.drags.push(DragState {
            pointer_id,
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
        stranded
    }

    /// Find the drag for a pointer (read-only).
    #[must_use]
    pub fn drag_for(&self, pointer_id: u64) -> Option<&DragState> {
        self.drags.iter().find(|d| d.pointer_id == pointer_id)
    }

    /// Update a single drag's knob value (vertical-drag widgets).
    /// Returns the new (`param_id`, normalized value) for the drag
    /// matching `pointer_id`, or `None` if no such drag is active.
    #[must_use]
    pub fn update_drag(&self, pointer_id: u64, mouse_y: f32) -> Option<(u32, f64)> {
        let drag = self.drag_for(pointer_id)?;
        let dy = drag.start_y - mouse_y;
        let delta = f64::from(dy) / f64::from(KNOB_PIXELS_PER_UNIT);
        let new_value = (drag.start_value + delta).clamp(0.0, 1.0);
        Some((drag.param_id, new_value))
    }

    /// Update a single horizontal-slider drag. Same shape as
    /// [`InteractionState::update_drag`] but maps `x` rather than `y`.
    #[must_use]
    pub fn update_slider_drag(&self, pointer_id: u64, mouse_x: f32) -> Option<(u32, f64)> {
        let drag = self.drag_for(pointer_id)?;
        let margin = 4.0;
        let rel = (mouse_x - drag.region_x - margin) / (drag.region_w - margin * 2.0);
        let new_value = f64::from(rel).clamp(0.0, 1.0);
        Some((drag.param_id, new_value))
    }

    /// End the drag for `pointer_id`. Returns the popped state so
    /// callers can emit the `ParamEdit::End` (and the y-axis `End`
    /// on XY pads) without re-searching the vec.
    pub fn end_drag(&mut self, pointer_id: u64) -> Option<DragState> {
        let idx = self.drags.iter().position(|d| d.pointer_id == pointer_id)?;
        Some(self.drags.swap_remove(idx))
    }

    /// Test if a point is inside the open dropdown popup.
    /// Returns the absolute option index (accounting for scroll) if hit, or None.
    #[must_use]
    // Hit-test math operates on f32 logical pixels bounded by the
    // window size; `(my - py - padding) / item_h` lands in
    // `[0, visible_count]`.
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
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
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
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
    #[must_use]
    pub fn dropdown_is_open(&self) -> bool {
        self.dropdown.is_some()
    }

    /// Close the dropdown popup. Returns the region index of the
    /// dropdown that was open, so the caller can suppress an
    /// immediate-reopen click landing on the same button without
    /// having to read `self.dropdown` *before* closing.
    pub fn dropdown_close(&mut self) -> Option<usize> {
        self.dropdown.take().map(|dd| dd.region_idx)
    }

    /// Scroll the dropdown popup by `delta` items (positive = down, negative = up).
    // Dropdown option counts stay below i32::MAX in practice (UI lists
    // never reach 2 billion).
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_possible_wrap,
        clippy::cast_sign_loss
    )]
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
    //
    // Grid cell coordinates widen `u32 as f32`; cells indices fit in
    // an editor's logical pixel range.
    #[allow(clippy::cast_precision_loss)]
    pub fn build_regions_grid(&mut self, layout: &GridLayout) {
        // See `build_regions`: preserve `dropdown_anchor_y` across the
        // per-frame rebuild so an idle frame that skips render doesn't
        // strand the next dropdown popup at y = 0.
        let prior_anchors: Vec<f32> = self
            .knob_regions
            .iter()
            .map(|r| r.dropdown_anchor_y)
            .collect();
        self.knob_regions.clear();

        let header_h = layout.header_height();
        let section_offsets = compute_section_offsets(layout);

        for gw in &layout.widgets {
            let x = GRID_PADDING + gw.col as f32 * (layout.cell_size + GRID_GAP);
            let y = header_h
                + GRID_PADDING
                + gw.row as f32 * (layout.cell_size + GRID_GAP)
                + section_offsets[gw.row as usize];
            let w = gw.col_span as f32 * (layout.cell_size + GRID_GAP) - GRID_GAP;
            let h = gw.row_span as f32 * (layout.cell_size + GRID_GAP) - GRID_GAP;
            let cx = x + w / 2.0;
            let cy = y + h / 2.0 - 5.0;
            let radius = w.min(h) / 2.0 - 4.0;

            // Pre-populate widget_type from the explicit `widget` kind
            // when the layout declares one. Callers that need
            // range-based inference for `None` (BuiltinEditor) still
            // overwrite this field after the call; for custom editors
            // that always set `widget` via the `layout::dropdown` /
            // `layout::knob` / … helpers, this means dispatch routes
            // correctly out of the box.
            let widget_type = widget_kind_to_type(gw.widget);

            let idx = self.knob_regions.len();
            self.knob_regions.push(WidgetRegion {
                param_id: gw.param_id,
                widget_type,
                x,
                y,
                w,
                h,
                cx,
                cy,
                radius,
                normalized_value: 0.0,
                dropdown_anchor_y: prior_anchors.get(idx).copied().unwrap_or(0.0),
            });
        }
    }
}

// ---------------------------------------------------------------------------
// Public `dispatch` - drive widget interactions from input events.
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
    let (w, h) = (layout.width(), layout.height());
    dispatch_in(events, layout, (w, h), snapshot, state)
}

/// Like [`dispatch`] but takes explicit `window_size` in the same
/// coordinate space as the layout - i.e. the size of the surface the
/// layout is being composited onto.
///
/// Use this when the layout is a chrome panel overlaid on a larger
/// custom-rendered surface (visualizers, graphs, canvases). It lets
/// dropdown popups and other bounds-aware overlays use the full
/// window rather than being clipped to the layout's bounding box -
/// otherwise a popup that wouldn't fit below the button flips above
/// it even when there's room below in the outer window.
// Window dimensions widen `u32 as f32`; window sizes are bounded by
// display dimensions, well below 2^23.
#[allow(clippy::cast_precision_loss)]
pub fn dispatch_in(
    events: &[InputEvent],
    layout: &Layout,
    window_size: (u32, u32),
    snapshot: &ParamSnapshot<'_>,
    state: &mut InteractionState,
) -> Vec<ParamEdit> {
    let mut edits = Vec::new();
    let window_w = window_size.0 as f32;
    let window_h = window_size.1 as f32;

    for ev in events {
        match *ev {
            InputEvent::MouseMove { pointer_id, x, y } => {
                // Popup-drag wins over knob-drag - a finger that
                // landed inside the open popup scrolls the list,
                // not any widget under it.
                if let Some(drag) = state.popup_drag.as_ref()
                    && drag.pointer_id == pointer_id
                {
                    apply_popup_scroll_drag(y, state);
                    continue;
                }
                let drag_info = state
                    .drag_for(pointer_id)
                    .map(|d| (d.widget_type, d.region_idx));
                if let Some((wtype, region_idx)) = drag_info {
                    let y_id = if wtype == WidgetType::XYPad {
                        layout_param_id_y(layout, region_idx)
                    } else {
                        None
                    };
                    apply_drag(pointer_id, x, y, y_id, state, &mut edits);
                } else {
                    // Hover / dropdown-hover are single-cursor concepts;
                    // skip for genuine multi-touch pointers so a second
                    // finger landing doesn't yank hover state away from
                    // the cursor's last position on a hybrid Mac.
                    if pointer_id == SINGLE_POINTER {
                        if state.dropdown_is_open() {
                            state.dropdown_update_hover(x, y);
                        }
                        state.hover_idx = state.hit_test(x, y);
                    }
                }
            }
            InputEvent::MouseDown {
                pointer_id,
                x,
                y,
                button: MouseButton::Left,
            } => {
                handle_mouse_down(
                    pointer_id, x, y, layout, snapshot, state, window_w, window_h, &mut edits,
                );
            }
            InputEvent::MouseUp {
                pointer_id,
                x,
                y,
                button: MouseButton::Left,
            } => {
                // Popup-drag end: if the user didn't scroll
                // appreciably (stayed within `ITEM_H / 2` of the
                // start), treat the touch as a tap and commit the
                // option under the release point. If they did
                // scroll, just keep the popup open.
                if let Some(drag) = state.popup_drag.take()
                    && drag.pointer_id == pointer_id
                {
                    if !drag.scrolled
                        && let Some(option_idx) = state.dropdown_popup_hit(x, y)
                        && let Some(dd) = state.dropdown.as_ref()
                    {
                        let param_id = dd.param_id;
                        let count = dd.options.len();
                        let new_norm = f32::from_f64(discrete_norm(option_idx, count));
                        edits.push(ParamEdit::Begin { id: param_id });
                        edits.push(ParamEdit::Set {
                            id: param_id,
                            normalized: new_norm,
                        });
                        edits.push(ParamEdit::End { id: param_id });
                        state.dropdown_close();
                    }
                    continue;
                }
                if let Some(drag) = state.end_drag(pointer_id) {
                    edits.push(ParamEdit::End { id: drag.param_id });
                    if drag.widget_type == WidgetType::XYPad
                        && let Some(y_id) = layout_param_id_y(layout, drag.region_idx)
                    {
                        edits.push(ParamEdit::End { id: y_id });
                    }
                }
            }
            InputEvent::MouseDoubleClick { x, y } => {
                if let Some(idx) = state.hit_test(x, y) {
                    let param_id = state.knob_regions[idx].param_id;
                    let default_norm = (snapshot.default_normalized)(param_id);
                    edits.push(ParamEdit::Begin { id: param_id });
                    edits.push(ParamEdit::Set {
                        id: param_id,
                        normalized: default_norm,
                    });
                    edits.push(ParamEdit::End { id: param_id });
                }
            }
            InputEvent::Scroll { x, y, dy } => {
                if state.dropdown_is_open() {
                    // An open dropdown captures ALL scroll input: wheel
                    // inside the popup scrolls the list, wheel outside
                    // is absorbed (no-op) so it can't fall through to
                    // the generic knob-scroll path below and silently
                    // advance the param driving this very dropdown.
                    let inside_popup = state.dropdown_popup_hit(x, y).is_some()
                        || state.dropdown.as_ref().is_some_and(|dd| {
                            let (px, py, pw, ph) = dd.popup_rect;
                            x >= px && x <= px + pw && y >= py && y <= py + ph
                        });
                    if inside_popup {
                        // dy == 0 should be a no-op - falling through to
                        // the else branch would silently scroll +1 each
                        // time a host emits a zero-magnitude wheel event.
                        let delta = match dy.partial_cmp(&0.0) {
                            Some(std::cmp::Ordering::Greater) => -1,
                            Some(std::cmp::Ordering::Less) => 1,
                            _ => 0,
                        };
                        if delta != 0 {
                            state.dropdown_scroll(delta);
                        }
                    }
                    continue;
                }
                if let Some(idx) = state.hit_test(x, y) {
                    // Only scroll-adjust continuous-value widgets.
                    // Dropdowns / Selectors / Toggles are discrete UI
                    // affordances - the user expects click to cycle,
                    // not wheel to drag them across their whole range.
                    let wtype = state.knob_regions[idx].widget_type;
                    if matches!(
                        wtype,
                        WidgetType::Knob | WidgetType::Slider | WidgetType::XYPad,
                    ) {
                        let param_id = state.knob_regions[idx].param_id;
                        let norm = (snapshot.get_param)(param_id);
                        let step = dy / KNOB_PIXELS_PER_UNIT;
                        let new_norm = (norm + step).clamp(0.0, 1.0);
                        edits.push(ParamEdit::Begin { id: param_id });
                        edits.push(ParamEdit::Set {
                            id: param_id,
                            normalized: new_norm,
                        });
                        edits.push(ParamEdit::End { id: param_id });
                    }
                }
            }
            InputEvent::MouseLeave => {
                if state.hover_idx.is_some() {
                    state.hover_idx = None;
                    state.needs_repaint = true;
                }
            }
            // Right- and middle-click are intentionally ignored. The
            // built-in editor doesn't have a context menu of its own,
            // and most plugin hosts (VST3, AU, AAX) treat right-click
            // inside the editor surface as their hook for the host's
            // own automation / parameter-link menu - swallowing the
            // event here would suppress that.
            InputEvent::MouseDown { .. } | InputEvent::MouseUp { .. } => {}
        }
    }

    edits
}

/// Mouse-down handling factored out of the big match so it's readable.
fn handle_mouse_down(
    pointer_id: u64,
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
    if let Some(dd) = state.dropdown.as_ref() {
        // MouseDown inside the popup starts a touch-drag - the
        // commit-or-scroll decision is deferred to MouseUp based
        // on whether the user moved or stayed still. Without
        // this, every tap on the popup commits immediately and
        // there's no way for touch users to scroll a list longer
        // than the visible area.
        let (px, py, pw, ph) = dd.popup_rect;
        if x >= px && x <= px + pw && y >= py && y <= py + ph {
            state.popup_drag = Some(PopupDrag {
                pointer_id,
                start_y: y,
                start_scroll_offset: dd.scroll_offset,
                scrolled: false,
            });
            return;
        }
        // Click outside popup: close. If it landed on the same dropdown
        // button, swallow the click (don't reopen).
        if let Some(open_region) = state.dropdown_close()
            && let Some(idx) = state.hit_test(x, y)
            && idx == open_region
            && state.widget_type_at(idx) == Some(WidgetType::Dropdown)
        {
            return;
        }
        // Fall through to normal widget hit-test.
    }

    let Some(idx) = state.hit_test(x, y) else {
        return;
    };
    let param_id = state.knob_regions[idx].param_id;
    let wtype = state.widget_type_at(idx);

    match wtype {
        Some(WidgetType::Toggle) => {
            let norm = (snapshot.get_param)(param_id);
            let new_norm = if norm > 0.5 { 0.0 } else { 1.0 };
            edits.push(ParamEdit::Begin { id: param_id });
            edits.push(ParamEdit::Set {
                id: param_id,
                normalized: new_norm,
            });
            edits.push(ParamEdit::End { id: param_id });
        }
        Some(WidgetType::Selector) => {
            let new_norm = (snapshot.next_discrete_normalized)(param_id);
            edits.push(ParamEdit::Begin { id: param_id });
            edits.push(ParamEdit::Set {
                id: param_id,
                normalized: new_norm,
            });
            edits.push(ParamEdit::End { id: param_id });
        }
        Some(WidgetType::Dropdown) => {
            open_dropdown(idx, param_id, snapshot, state, window_w, window_h);
        }
        _ => {
            // Knob / Slider / XYPad / Meter: begin a drag.
            let norm = f64::from((snapshot.get_param)(param_id));
            // If a system gesture stole the previous touch for this
            // pointer_id without firing `touchesCancelled:`, the
            // displaced drag's `Begin` is still on the host's
            // gesture stack - flush an `End` for it (XY pads need
            // both axes) before opening the new gesture.
            if let Some(stranded) = state.begin_drag(pointer_id, idx, norm, y) {
                edits.push(ParamEdit::End {
                    id: stranded.param_id,
                });
                if stranded.widget_type == WidgetType::XYPad
                    && let Some(y_id) = layout_param_id_y(layout, stranded.region_idx)
                {
                    edits.push(ParamEdit::End { id: y_id });
                }
            }
            edits.push(ParamEdit::Begin { id: param_id });
            if wtype == Some(WidgetType::XYPad)
                && let Some(y_id) = layout_param_id_y(layout, idx)
            {
                edits.push(ParamEdit::Begin { id: y_id });
            }
        }
    }
}

// Layout / hit-test math is f32 logical pixels bounded by window size;
// `((avail_h - padding * 2.0) / item_h)` lands in `[0, options.len()]`.
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_precision_loss
)]
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
    let selected = discrete_index(f64::from(current_norm), count);
    let region = &state.knob_regions[region_idx];

    let item_h = 18.0f32;
    let padding = 4.0f32;

    let anchor_below = region.dropdown_anchor_y; // bottom of button box
    let popup_w = region.w.max(80.0);
    let full_popup_h = options.len() as f32 * item_h + padding * 2.0;

    // Always anchor the popup directly under the dropdown button.
    // If the full list doesn't fit between `anchor_below` and the
    // window's bottom, cap `visible_count` and scroll - DON'T
    // shift the popup upward to make more items fit. Shifting up
    // landed the popup near `y = 0` (literally the top of the
    // editor) for any dropdown whose full option list was taller
    // than the editor, far from the button the user just tapped.
    // Scrolling is the lesser annoyance.
    let popup_y = anchor_below.max(0.0);
    let space_below = (window_h - popup_y).max(item_h + padding * 2.0);
    let avail_h = full_popup_h.min(space_below);

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

/// Touch scroll-drag on the open dropdown popup. Maps vertical
/// motion since the drag started into `scroll_offset` changes
/// (one item per `item_h` of drag). If the user has moved more
/// than half an item from the start, flips `scrolled = true` so
/// the `MouseUp` handler treats the touch as a scroll instead of
/// a commit-on-tap.
//
// Cast contract: `start_scroll_offset` is bounded by
// `dd.options.len()` which (per the dropdown widget shape) caps
// at a few hundred - well below `i32::MAX`. `items_scrolled` is
// `(dy / item_h)` where `dy` is a finite single-frame motion;
// the product never approaches i32 limits.
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_possible_wrap
)]
fn apply_popup_scroll_drag(y: f32, state: &mut InteractionState) {
    let item_h = 18.0f32;
    let (start_y, start_scroll_offset) = match state.popup_drag.as_ref() {
        Some(d) => (d.start_y, d.start_scroll_offset),
        None => return,
    };
    let dy = start_y - y;
    if dy.abs() > item_h / 2.0
        && let Some(d) = state.popup_drag.as_mut()
    {
        d.scrolled = true;
    }
    let items_scrolled = (dy / item_h).round() as i32;
    let new_offset = start_scroll_offset as i32 + items_scrolled;
    if let Some(dd) = state.dropdown.as_mut() {
        let max_offset = (dd.options.len() as i32 - dd.visible_count as i32).max(0);
        dd.scroll_offset = new_offset.clamp(0, max_offset) as usize;
    }
}

fn apply_drag(
    pointer_id: u64,
    x: f32,
    y: f32,
    y_id_for_xy: Option<u32>,
    state: &InteractionState,
    edits: &mut Vec<ParamEdit>,
) {
    let Some(drag) = state.drag_for(pointer_id) else {
        return;
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

            edits.push(ParamEdit::Set {
                id: drag.param_id,
                normalized: norm_x,
            });
            if let Some(y_id) = y_id_for_xy {
                edits.push(ParamEdit::Set {
                    id: y_id,
                    normalized: norm_y,
                });
            }
        }
        WidgetType::Slider => {
            if let Some((pid, new_norm)) = state.update_slider_drag(pointer_id, x) {
                edits.push(ParamEdit::Set {
                    id: pid,
                    normalized: f32::from_f64(new_norm),
                });
            }
        }
        _ => {
            if let Some((pid, new_norm)) = state.update_drag(pointer_id, y) {
                edits.push(ParamEdit::Set {
                    id: pid,
                    normalized: f32::from_f64(new_norm),
                });
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
