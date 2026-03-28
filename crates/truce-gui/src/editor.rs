//! Built-in editor using the CPU render backend.
//!
//! Renders parameter widgets via `RenderBackend`. Uses tiny-skia and blits
//! RGBA pixels to a CALayer. For GPU rendering, see the `truce-gpu` crate
//! which provides `GpuEditor` wrapping this editor with wgpu.

use std::ffi::c_void;
use std::sync::Arc;

use truce_core::editor::{Editor, EditorContext, RawWindowHandle};
use truce_params::Params;

use crate::backend_cpu::CpuBackend;
use crate::interaction::{DropdownState, InteractionState};
use crate::layout::{GridLayout, Layout, PluginLayout, compute_section_offsets,
                     GRID_GAP, GRID_PADDING, GRID_HEADER_H, GRID_SECTION_H};
use crate::platform::{PlatformView, ViewCallbacks};
use crate::render::RenderBackend;
use crate::theme::Theme;
use crate::widgets;

/// Built-in editor that renders parameter widgets to a pixel buffer.
///
/// Uses the CPU backend (tiny-skia) for software rasterization. When
/// `open()` is called, creates a platform view and blits pixels at ~60fps.
pub struct BuiltinEditor<P: Params> {
    params: Arc<P>,
    layout: Layout,
    theme: Theme,
    backend: Option<CpuBackend>,
    interaction: InteractionState,
    context: Option<EditorContext>,
    view: Option<PlatformView>,
    /// Leaked self-pointer for C callbacks. Cleaned up on close().
    self_ptr: *mut c_void,
}

// Raw window handles and self_ptr are only accessed from the host UI thread.
unsafe impl<P: Params> Send for BuiltinEditor<P> {}

impl<P: Params + 'static> BuiltinEditor<P> {
    pub fn new(params: Arc<P>, layout: PluginLayout) -> Self {
        Self {
            params,
            layout: Layout::Rows(layout),
            theme: Theme::dark(),
            backend: None,
            interaction: InteractionState::new(),
            context: None,
            view: None,
            self_ptr: std::ptr::null_mut(),
        }
    }

    pub fn new_with_layout(params: Arc<P>, layout: Layout) -> Self {
        Self {
            params,
            layout,
            theme: Theme::dark(),
            backend: None,
            interaction: InteractionState::new(),
            context: None,
            view: None,
            self_ptr: std::ptr::null_mut(),
        }
    }

    pub fn new_grid(params: Arc<P>, layout: GridLayout) -> Self {
        Self {
            params,
            layout: Layout::Grid(layout),
            theme: Theme::dark(),
            backend: None,
            interaction: InteractionState::new(),
            context: None,
            view: None,
            self_ptr: std::ptr::null_mut(),
        }
    }

    pub fn with_theme(mut self, theme: Theme) -> Self {
        self.theme = theme;
        self
    }

    /// Render the full UI to the internal CPU pixel buffer.
    pub fn render(&mut self) {
        let (w, h) = (self.layout.width(), self.layout.height());
        let backend = self
            .backend
            .get_or_insert_with(|| CpuBackend::new(w, h).expect("Failed to create backend"));
        // SAFETY: we split the borrow — backend is a separate field from layout/params/etc.
        let backend_ptr = backend as *mut CpuBackend;
        self.render_widgets(unsafe { &mut *backend_ptr });
    }

    /// Render all widgets to any `RenderBackend`.
    fn render_widgets(&mut self, backend: &mut dyn RenderBackend) {
        if matches!(self.layout, Layout::Grid(_)) {
            self.render_grid_inner(backend);
        } else {
            self.render_rows_inner(backend);
        }
    }

    fn render_rows_inner(&mut self, backend: &mut dyn RenderBackend) {
        let pl = match &self.layout {
            Layout::Rows(pl) => pl,
            _ => return,
        };
        let w = pl.width;
        let knob_size = pl.knob_size;
        let title = pl.title;
        let version = pl.version;

        backend.clear(self.theme.background);
        let theme = &self.theme;

        widgets::draw_header(backend, 0.0, 0.0, w as f32, 30.0, title, version, theme);

        let pl = match &self.layout {
            Layout::Rows(pl) => pl,
            _ => return,
        };
        let mut y = 35.0;
        let mut render_widget_idx = 0usize;

        for row in &pl.rows {
            if let Some(label) = row.label {
                widgets::draw_section_label(backend, 0.0, y, w as f32, label, theme);
                y += 18.0;
            }

            let total_cols: u32 = row.knobs.iter().map(|k| k.span.max(1)).sum();
            let total_w = total_cols as f32 * (knob_size + 10.0) - 10.0;
            let start_x = (w as f32 - total_w) / 2.0;

            let mut col = 0u32;
            for knob_def in row.knobs.iter() {
                let span = knob_def.span.max(1);
                let x = start_x + col as f32 * (knob_size + 10.0);
                let widget_w = span as f32 * (knob_size + 10.0) - 10.0;

                let (normalized, value_text) = if let Some(ref ctx) = self.context {
                    let n = (ctx.get_param)(knob_def.param_id) as f32;
                    let t = (ctx.format_param)(knob_def.param_id);
                    (n, t)
                } else {
                    let n = self.params.get_normalized(knob_def.param_id).unwrap_or(0.0) as f32;
                    let p = self.params.get_plain(knob_def.param_id).unwrap_or(0.0);
                    let t = self
                        .params
                        .format_value(knob_def.param_id, p)
                        .unwrap_or_else(|| format!("{:.1}", p));
                    (n, t)
                };

                let region_idx = render_widget_idx;
                render_widget_idx += 1;
                let is_hovered = self.interaction.hover_idx == Some(region_idx);

                let wtype = resolve_widget_type(knob_def.widget, knob_def.param_id, &*self.params);

                match wtype {
                    widgets::WidgetType::Toggle => widgets::draw_toggle(
                        backend, x, y, widget_w, knob_size,
                        normalized, knob_def.label, &value_text,
                        theme, is_hovered,
                    ),
                    widgets::WidgetType::Slider => widgets::draw_slider(
                        backend, x, y, widget_w, knob_size,
                        normalized, knob_def.label, &value_text,
                        theme, is_hovered,
                    ),
                    widgets::WidgetType::Selector => widgets::draw_selector(
                        backend, x, y, widget_w, knob_size,
                        normalized, knob_def.label, &value_text,
                        theme, is_hovered,
                    ),
                    widgets::WidgetType::Dropdown => {
                        let is_open = self.interaction.dropdown.as_ref()
                            .map_or(false, |dd| dd.region_idx == region_idx);
                        widgets::draw_dropdown(
                            backend, x, y, widget_w, knob_size,
                            normalized, knob_def.label, &value_text,
                            theme, is_hovered, is_open,
                        );
                        // Store the button box bottom for popup positioning
                        let anchor_cy = y + knob_size / 2.0 - 8.0;
                        if let Some(region) = self.interaction.knob_regions.get_mut(region_idx) {
                            region.dropdown_anchor_y = anchor_cy + 10.0; // cy + box_h/2
                        }
                    },
                    widgets::WidgetType::Meter => {
                        let default_ids = vec![knob_def.param_id];
                        let ids = knob_def.meter_ids.as_deref()
                            .unwrap_or(&default_ids);
                        let levels: Vec<f32> = if let Some(ref ctx) = self.context {
                            ids.iter().map(|&id| (ctx.get_meter)(id)).collect()
                        } else {
                            vec![0.0; ids.len()]
                        };
                        widgets::draw_meter(
                            backend, x, y, widget_w, knob_size,
                            &levels, knob_def.label, theme,
                        );
                    },
                    widgets::WidgetType::XYPad => {
                        let val_y_id = knob_def.param_id_y.unwrap_or(knob_def.param_id);
                        let (vx, vy) = if let Some(ref ctx) = self.context {
                            ((ctx.get_param)(knob_def.param_id) as f32,
                             (ctx.get_param)(val_y_id) as f32)
                        } else {
                            (self.params.get_normalized(knob_def.param_id).unwrap_or(0.0) as f32,
                             self.params.get_normalized(val_y_id).unwrap_or(0.0) as f32)
                        };
                        let infos = self.params.param_infos();
                        let x_name = infos.iter().find(|i| i.id == knob_def.param_id)
                            .map(|i| i.name).unwrap_or(knob_def.label);
                        let y_name = infos.iter().find(|i| i.id == val_y_id)
                            .map(|i| i.name).unwrap_or("");
                        widgets::draw_xy_pad(
                            backend, x, y, widget_w, knob_size,
                            vx, vy, x_name, y_name, theme, is_hovered,
                        );
                    },
                    widgets::WidgetType::Knob => widgets::draw_knob(
                        backend, x, y, knob_size, normalized,
                        knob_def.label, &value_text, theme, is_hovered,
                    ),
                }
                col += span;
            }

            y += knob_size + 30.0;
        }

        // Dropdown popup overlay (rendered last, on top of everything)
        self.render_dropdown_popup(backend);
    }

    fn render_grid_inner(&mut self, backend: &mut dyn RenderBackend) {
        let grid = match &self.layout {
            Layout::Grid(g) => g,
            _ => return,
        };
        let w = grid.width;
        let title = grid.title;
        let version = grid.version;

        backend.clear(self.theme.background);
        let theme = &self.theme;

        widgets::draw_header(backend, 0.0, 0.0, w as f32, 30.0, title, version, theme);

        let grid = match &self.layout {
            Layout::Grid(g) => g,
            _ => return,
        };

        let section_offsets = compute_section_offsets(grid);

        // Section labels
        for &(row_idx, label) in &grid.sections {
            let y = GRID_HEADER_H + GRID_PADDING
                + row_idx as f32 * (grid.cell_size + GRID_GAP)
                + section_offsets[row_idx as usize]
                - GRID_SECTION_H;
            widgets::draw_section_label(backend, 0.0, y, w as f32, label, theme);
        }

        // Widgets
        for (idx, gw) in grid.widgets.iter().enumerate() {
            let x = GRID_PADDING + gw.col as f32 * (grid.cell_size + GRID_GAP);
            let y = GRID_HEADER_H + GRID_PADDING
                + gw.row as f32 * (grid.cell_size + GRID_GAP)
                + section_offsets[gw.row as usize];
            let widget_w = gw.col_span as f32 * (grid.cell_size + GRID_GAP) - GRID_GAP;
            let widget_h = gw.row_span as f32 * (grid.cell_size + GRID_GAP) - GRID_GAP;

            let (normalized, value_text) = if let Some(ref ctx) = self.context {
                let n = (ctx.get_param)(gw.param_id) as f32;
                let t = (ctx.format_param)(gw.param_id);
                (n, t)
            } else {
                let n = self.params.get_normalized(gw.param_id).unwrap_or(0.0) as f32;
                let p = self.params.get_plain(gw.param_id).unwrap_or(0.0);
                let t = self
                    .params
                    .format_value(gw.param_id, p)
                    .unwrap_or_else(|| format!("{:.1}", p));
                (n, t)
            };

            let is_hovered = self.interaction.hover_idx == Some(idx);
            let wtype = resolve_widget_type(gw.widget, gw.param_id, &*self.params);

            match wtype {
                widgets::WidgetType::Toggle => widgets::draw_toggle(
                    backend, x, y, widget_w, widget_h,
                    normalized, gw.label, &value_text, theme, is_hovered,
                ),
                widgets::WidgetType::Slider => widgets::draw_slider(
                    backend, x, y, widget_w, widget_h,
                    normalized, gw.label, &value_text, theme, is_hovered,
                ),
                widgets::WidgetType::Selector => widgets::draw_selector(
                    backend, x, y, widget_w, widget_h,
                    normalized, gw.label, &value_text, theme, is_hovered,
                ),
                widgets::WidgetType::Dropdown => {
                    let is_open = self.interaction.dropdown.as_ref()
                        .map_or(false, |dd| dd.region_idx == idx);
                    widgets::draw_dropdown(
                        backend, x, y, widget_w, widget_h,
                        normalized, gw.label, &value_text,
                        theme, is_hovered, is_open,
                    );
                    // Store the button box bottom for popup positioning
                    let anchor_cy = y + widget_h / 2.0 - 8.0;
                    if let Some(region) = self.interaction.knob_regions.get_mut(idx) {
                        region.dropdown_anchor_y = anchor_cy + 10.0; // cy + box_h/2
                    }
                },
                widgets::WidgetType::Meter => {
                    let default_ids = vec![gw.param_id];
                    let ids = gw.meter_ids.as_deref().unwrap_or(&default_ids);
                    let levels: Vec<f32> = if let Some(ref ctx) = self.context {
                        ids.iter().map(|&id| (ctx.get_meter)(id)).collect()
                    } else {
                        vec![0.0; ids.len()]
                    };
                    widgets::draw_meter(
                        backend, x, y, widget_w, widget_h,
                        &levels, gw.label, theme,
                    );
                },
                widgets::WidgetType::XYPad => {
                    let val_y_id = gw.param_id_y.unwrap_or(gw.param_id);
                    let (vx, vy) = if let Some(ref ctx) = self.context {
                        ((ctx.get_param)(gw.param_id) as f32,
                         (ctx.get_param)(val_y_id) as f32)
                    } else {
                        (self.params.get_normalized(gw.param_id).unwrap_or(0.0) as f32,
                         self.params.get_normalized(val_y_id).unwrap_or(0.0) as f32)
                    };
                    let infos = self.params.param_infos();
                    let x_name = infos.iter().find(|i| i.id == gw.param_id)
                        .map(|i| i.name).unwrap_or(gw.label);
                    let y_name = infos.iter().find(|i| i.id == val_y_id)
                        .map(|i| i.name).unwrap_or("");
                    widgets::draw_xy_pad(
                        backend, x, y, widget_w, widget_h,
                        vx, vy, x_name, y_name, theme, is_hovered,
                    );
                },
                widgets::WidgetType::Knob => {
                    let knob_size = widget_w.min(widget_h);
                    let kx = x + (widget_w - knob_size) / 2.0;
                    let ky = y + (widget_h - knob_size) / 2.0;
                    widgets::draw_knob(
                        backend, kx, ky, knob_size, normalized,
                        gw.label, &value_text, theme, is_hovered,
                    );
                },
            }
        }

        // Dropdown popup overlay (rendered last, on top of everything)
        self.render_dropdown_popup(backend);
    }

    /// Draw the dropdown popup overlay if one is open.
    fn render_dropdown_popup(&self, backend: &mut dyn RenderBackend) {
        if let Some(ref dd) = self.interaction.dropdown {
            let (px, py, pw, _) = dd.popup_rect;
            widgets::draw_dropdown_popup(
                backend,
                px,
                py,
                pw,
                &dd.options,
                dd.selected,
                dd.hover_option,
                dd.scroll_offset,
                dd.visible_count,
                &self.theme,
            );
        }
    }

    /// Get the raw pixel data after rendering (RGBA premultiplied).
    pub fn pixel_data(&self) -> Option<&[u8]> {
        self.backend.as_ref().map(|b| b.data())
    }

    /// Get the KnobDef at a flattened index (Rows layout only).
    fn knob_def_at(&self, idx: usize) -> Option<&crate::layout::KnobDef> {
        if let Layout::Rows(pl) = &self.layout {
            let mut i = 0;
            for row in &pl.rows {
                for kd in &row.knobs {
                    if i == idx { return Some(kd); }
                    i += 1;
                }
            }
        }
        None
    }

    /// Get the Y-axis param ID for an XY pad at the given region index.
    fn param_id_y_at(&self, idx: usize) -> Option<u32> {
        match &self.layout {
            Layout::Rows(_) => self.knob_def_at(idx).and_then(|kd| kd.param_id_y),
            Layout::Grid(g) => g.widgets.get(idx).and_then(|w| w.param_id_y),
        }
    }

    // --- Public API for external backends (truce-gpu) ---

    /// Whether the editor has an active context.
    pub fn has_context(&self) -> bool {
        self.context.is_some()
    }

    /// Take the editor context, leaving `None` in its place.
    /// Used by hot-reload to preserve the context when swapping editors.
    pub fn take_context(&mut self) -> Option<EditorContext> {
        self.context.take()
    }

    /// Set the editor context (host callbacks) without opening the CPU view.
    pub fn set_context(&mut self, context: EditorContext) {
        self.context = Some(context);
        match &self.layout {
            Layout::Rows(pl) => self.interaction.build_regions(pl),
            Layout::Grid(gl) => self.interaction.build_regions_grid(gl),
        }
    }

    /// Render all widgets to an external `RenderBackend`.
    ///
    /// Used by `truce-gpu` to draw through the GPU backend instead of
    /// the internal CPU backend.
    pub fn render_to(&mut self, backend: &mut dyn RenderBackend) {
        unsafe { update_interaction(self) };
        self.render_widgets(backend);
    }

    // --- Mouse event handlers (public for external backends) ---

    pub fn on_mouse_down(&mut self, x: f32, y: f32) {
        // If a dropdown popup is open, check if the click is inside it
        if self.interaction.dropdown_is_open() {
            if let Some(option_idx) = self.interaction.dropdown_popup_hit(x, y) {
                // Select the clicked option
                let dd = self.interaction.dropdown.as_ref().unwrap();
                let param_id = dd.param_id;
                let count = dd.options.len();
                let new_norm = if count <= 1 {
                    0.0
                } else {
                    option_idx as f64 / (count - 1) as f64
                };
                self.params.set_normalized(param_id, new_norm);
                if let Some(ref ctx) = self.context {
                    (ctx.begin_edit)(param_id);
                    (ctx.set_param)(param_id, new_norm);
                    (ctx.end_edit)(param_id);
                }
                self.interaction.dropdown_close();
                return;
            }
            // Click outside popup — close it. If the click landed on the
            // same dropdown button, just close (don't reopen).
            let open_region = self.interaction.dropdown.as_ref().unwrap().region_idx;
            self.interaction.dropdown_close();
            if let Some(idx) = self.interaction.hit_test(x, y) {
                if idx == open_region
                    && self.interaction.widget_type_at(idx) == Some(crate::widgets::WidgetType::Dropdown)
                {
                    return;
                }
            }
            // Fall through to check if they clicked another widget
        }

        if let Some(idx) = self.interaction.hit_test(x, y) {
            let param_id = self.interaction.knob_regions[idx].param_id;
            let wtype = self.interaction.widget_type_at(idx);
            if wtype == Some(crate::widgets::WidgetType::Toggle) {
                let norm = self.params.get_normalized(param_id).unwrap_or(0.0);
                let new_norm = if norm > 0.5 { 0.0 } else { 1.0 };
                self.params.set_normalized(param_id, new_norm);
                if let Some(ref ctx) = self.context {
                    (ctx.begin_edit)(param_id);
                    (ctx.set_param)(param_id, new_norm);
                    (ctx.end_edit)(param_id);
                }
            } else if wtype == Some(crate::widgets::WidgetType::Selector) {
                if let Some(info) = self.params.param_infos().into_iter().find(|i| i.id == param_id) {
                    let plain = self.params.get_plain(param_id).unwrap_or(0.0);
                    let max = info.range.max();
                    let next = if plain >= max { 0.0 } else { plain + 1.0 };
                    let new_norm = info.range.normalize(next);
                    self.params.set_normalized(param_id, new_norm);
                    if let Some(ref ctx) = self.context {
                        (ctx.begin_edit)(param_id);
                        (ctx.set_param)(param_id, new_norm);
                        (ctx.end_edit)(param_id);
                    }
                }
            } else if wtype == Some(crate::widgets::WidgetType::Dropdown) {
                // Open the dropdown popup
                if let Some(info) = self.params.param_infos().into_iter().find(|i| i.id == param_id) {
                    let count = info.range.step_count().max(1) as usize;
                    if count == 0 { return; }
                    let options: Vec<String> = (0..count)
                        .map(|i| {
                            let norm = if count <= 1 { 0.0 } else { i as f64 / (count - 1) as f64 };
                            let plain = info.range.denormalize(norm);
                            self.params.format_value(param_id, plain)
                                .unwrap_or_else(|| format!("{:.0}", plain))
                        })
                        .collect();
                    let current_norm = self.params.get_normalized(param_id).unwrap_or(0.0);
                    let selected = (current_norm * (count - 1).max(1) as f64).round() as usize;
                    let region = &self.interaction.knob_regions[idx];

                    let item_h = 18.0f32;
                    let padding = 4.0f32;
                    let window_w = self.layout.width() as f32;
                    let window_h = self.layout.height() as f32;

                    let anchor_below = region.dropdown_anchor_y; // bottom of button box
                    let anchor_above = anchor_below - 20.0;      // top of button box (box_h=20)
                    let popup_w = region.w.max(80.0);
                    let full_popup_h = options.len() as f32 * item_h + padding * 2.0;

                    // Vertical: prefer below, flip above if needed, pin if neither fits
                    let (popup_y, avail_h) = if anchor_below + full_popup_h <= window_h {
                        // Fits below
                        (anchor_below, full_popup_h)
                    } else if anchor_above - full_popup_h >= 0.0 {
                        // Fits above
                        (anchor_above - full_popup_h, full_popup_h)
                    } else {
                        // Neither fits — use whichever side has more space, clamp height
                        let space_below = window_h - anchor_below;
                        let space_above = anchor_above;
                        if space_below >= space_above {
                            (anchor_below, space_below.max(item_h + padding * 2.0))
                        } else {
                            let h = space_above.max(item_h + padding * 2.0);
                            (anchor_above - h, h)
                        }
                    };

                    // Clamp visible count based on available height
                    let visible_count = ((avail_h - padding * 2.0) / item_h)
                        .floor()
                        .max(1.0) as usize;
                    let visible_count = visible_count.min(options.len());
                    let popup_h = visible_count as f32 * item_h + padding * 2.0;

                    // Horizontal: clamp to window bounds
                    let popup_x = region.x.clamp(0.0, (window_w - popup_w).max(0.0));

                    // Scroll so the selected item is visible
                    let scroll_offset = if selected >= visible_count {
                        selected - visible_count + 1
                    } else {
                        0
                    };

                    self.interaction.dropdown = Some(DropdownState {
                        region_idx: idx,
                        param_id,
                        popup_rect: (popup_x, popup_y, popup_w, popup_h),
                        options,
                        selected,
                        hover_option: None,
                        scroll_offset,
                        visible_count,
                    });
                }
            } else {
                let norm = self.params.get_normalized(param_id).unwrap_or(0.0);
                self.interaction.begin_drag(idx, norm, y);
                if let Some(ref ctx) = self.context {
                    (ctx.begin_edit)(param_id);
                    if wtype == Some(crate::widgets::WidgetType::XYPad) {
                        if let Some(y_id) = self.param_id_y_at(idx) {
                            (ctx.begin_edit)(y_id);
                        }
                    }
                }
            }
        }
    }

    pub fn on_mouse_dragged(&mut self, x: f32, y: f32) {
        if let Some(drag) = &self.interaction.dragging {
            if drag.widget_type == crate::widgets::WidgetType::XYPad {
                let pad_margin = 4.0;
                let label_h = 18.0;
                let pad_x = drag.region_x + pad_margin;
                let pad_w = drag.region_w - pad_margin * 2.0;
                let pad_y_start = drag.region_y + pad_margin;
                let pad_h = drag.region_h - pad_margin * 2.0 - label_h;

                let norm_x = ((x - pad_x) / pad_w).clamp(0.0, 1.0) as f64;
                let norm_y = (1.0 - (y - pad_y_start) / pad_h).clamp(0.0, 1.0) as f64;

                let param_id = drag.param_id;
                let region_idx = drag.region_idx;
                self.params.set_normalized(param_id, norm_x);
                if let Some(ref ctx) = self.context {
                    (ctx.set_param)(param_id, norm_x);
                }

                if let Some(y_id) = self.param_id_y_at(region_idx) {
                    self.params.set_normalized(y_id, norm_y);
                    if let Some(ref ctx) = self.context {
                        (ctx.set_param)(y_id, norm_y);
                    }
                }
            } else if drag.widget_type == crate::widgets::WidgetType::Slider {
                if let Some((param_id, new_norm)) = self.interaction.update_slider_drag(x) {
                    self.params.set_normalized(param_id, new_norm);
                    if let Some(ref ctx) = self.context {
                        (ctx.set_param)(param_id, new_norm);
                    }
                }
            } else {
                if let Some((param_id, new_norm)) = self.interaction.update_drag(y) {
                    self.params.set_normalized(param_id, new_norm);
                    if let Some(ref ctx) = self.context {
                        (ctx.set_param)(param_id, new_norm);
                    }
                }
            }
        }
    }

    pub fn on_mouse_up(&mut self, _x: f32, _y: f32) {
        if let Some(drag) = &self.interaction.dragging {
            let param_id = drag.param_id;
            let was_xy = drag.widget_type == crate::widgets::WidgetType::XYPad;
            let region_idx = drag.region_idx;
            self.interaction.end_drag();
            if let Some(ref ctx) = self.context {
                (ctx.end_edit)(param_id);
                if was_xy {
                    if let Some(y_id) = self.param_id_y_at(region_idx) {
                        (ctx.end_edit)(y_id);
                    }
                }
            }
        }
    }

    pub fn on_double_click(&mut self, x: f32, y: f32) {
        if let Some(idx) = self.interaction.hit_test(x, y) {
            let param_id = self.interaction.knob_regions[idx].param_id;
            // Reset to default value
            let infos = self.params.param_infos();
            if let Some(info) = infos.iter().find(|i| i.id == param_id) {
                let default_norm = info.range.normalize(info.default_plain);
                self.params.set_normalized(param_id, default_norm);
                if let Some(ref ctx) = self.context {
                    (ctx.begin_edit)(param_id);
                    (ctx.set_param)(param_id, default_norm);
                    (ctx.end_edit)(param_id);
                }
            }
        }
    }

    pub fn on_scroll(&mut self, x: f32, y: f32, delta_y: f32) {
        // If a dropdown popup is open and the cursor is over it, scroll the popup
        if self.interaction.dropdown_is_open() {
            if self.interaction.dropdown_popup_hit(x, y).is_some()
                || self.interaction.dropdown.as_ref().map_or(false, |dd| {
                    let (px, py, pw, ph) = dd.popup_rect;
                    x >= px && x <= px + pw && y >= py && y <= py + ph
                })
            {
                let delta = if delta_y > 0.0 { -1 } else { 1 };
                self.interaction.dropdown_scroll(delta);
                return;
            }
        }

        if let Some(idx) = self.interaction.hit_test(x, y) {
            let param_id = self.interaction.knob_regions[idx].param_id;
            let norm = self.params.get_normalized(param_id).unwrap_or(0.0);
            let step = delta_y as f64 / 200.0; // 200 pixels of scroll = full range
            let new_norm = (norm + step).clamp(0.0, 1.0);
            self.params.set_normalized(param_id, new_norm);
            if let Some(ref ctx) = self.context {
                (ctx.begin_edit)(param_id);
                (ctx.set_param)(param_id, new_norm);
                (ctx.end_edit)(param_id);
            }
        }
    }

    pub fn on_mouse_moved(&mut self, x: f32, y: f32) -> bool {
        // Update dropdown popup hover if open
        if self.interaction.dropdown_is_open() {
            self.interaction.dropdown_update_hover(x, y);
        }
        self.interaction.hover_idx = self.interaction.hit_test(x, y);
        self.interaction.hover_idx.is_some() || self.interaction.dropdown_is_open()
    }
}

// ---------------------------------------------------------------------------
// C callbacks — thin wrappers that cast the context pointer back to &mut Self
// ---------------------------------------------------------------------------

/// Update interaction regions and live param values.
///
/// # Safety
/// The editor must be valid and not concurrently accessed.
pub unsafe fn update_interaction<P: Params + 'static>(editor: &mut BuiltinEditor<P>) {
    match &editor.layout {
        Layout::Rows(pl) => {
            editor.interaction.build_regions(pl);
            let mut flat_idx = 0usize;
            for row in &pl.rows {
                for knob_def in &row.knobs {
                    if let Some(region) = editor.interaction.knob_regions.get_mut(flat_idx) {
                        region.widget_type = resolve_widget_type(
                            knob_def.widget, knob_def.param_id, &*editor.params,
                        );
                    }
                    flat_idx += 1;
                }
            }
        }
        Layout::Grid(gl) => {
            editor.interaction.build_regions_grid(gl);
            for (idx, gw) in gl.widgets.iter().enumerate() {
                if let Some(region) = editor.interaction.knob_regions.get_mut(idx) {
                    region.widget_type = resolve_widget_type(
                        gw.widget, gw.param_id, &*editor.params,
                    );
                }
            }
        }
    }
    for region in &mut editor.interaction.knob_regions {
        if let Some(ref ctx) = editor.context {
            region.normalized_value = (ctx.get_param)(region.param_id) as f32;
        } else {
            region.normalized_value =
                editor.params.get_normalized(region.param_id).unwrap_or(0.0) as f32;
        }
    }
}

unsafe extern "C" fn cb_render<P: Params + 'static>(
    ctx: *mut c_void,
    out_w: *mut u32,
    out_h: *mut u32,
) -> *const u8 {
    let editor = &mut *(ctx as *mut BuiltinEditor<P>);
    update_interaction(editor);
    editor.render();
    let backend = match editor.backend.as_ref() {
        Some(b) => b,
        None => return std::ptr::null(),
    };
    *out_w = backend.width();
    *out_h = backend.height();
    backend.data().as_ptr()
}

unsafe extern "C" fn cb_mouse_down<P: Params + 'static>(ctx: *mut c_void, x: f32, y: f32) {
    let editor = &mut *(ctx as *mut BuiltinEditor<P>);
    editor.on_mouse_down(x, y);
}

unsafe extern "C" fn cb_mouse_dragged<P: Params + 'static>(ctx: *mut c_void, x: f32, y: f32) {
    let editor = &mut *(ctx as *mut BuiltinEditor<P>);
    editor.on_mouse_dragged(x, y);
}

unsafe extern "C" fn cb_mouse_up<P: Params + 'static>(ctx: *mut c_void, x: f32, y: f32) {
    let editor = &mut *(ctx as *mut BuiltinEditor<P>);
    editor.on_mouse_up(x, y);
}

unsafe extern "C" fn cb_scroll<P: Params + 'static>(
    ctx: *mut c_void,
    x: f32,
    y: f32,
    delta_y: f32,
) {
    let editor = &mut *(ctx as *mut BuiltinEditor<P>);
    editor.on_scroll(x, y, delta_y);
}

unsafe extern "C" fn cb_double_click<P: Params + 'static>(ctx: *mut c_void, x: f32, y: f32) {
    let editor = &mut *(ctx as *mut BuiltinEditor<P>);
    editor.on_double_click(x, y);
}

unsafe extern "C" fn cb_mouse_moved<P: Params + 'static>(ctx: *mut c_void, x: f32, y: f32) -> u8 {
    let editor = &mut *(ctx as *mut BuiltinEditor<P>);
    editor.on_mouse_moved(x, y) as u8
}

// ---------------------------------------------------------------------------
// Editor trait implementation
// ---------------------------------------------------------------------------

/// Resolve widget type: explicit override > auto-detect from param range.
fn resolve_widget_type<P: Params>(
    widget: Option<crate::layout::WidgetKind>,
    param_id: u32,
    params: &P,
) -> widgets::WidgetType {
    match widget {
        Some(crate::layout::WidgetKind::Knob) => widgets::WidgetType::Knob,
        Some(crate::layout::WidgetKind::Slider) => widgets::WidgetType::Slider,
        Some(crate::layout::WidgetKind::Toggle) => widgets::WidgetType::Toggle,
        Some(crate::layout::WidgetKind::Selector) => widgets::WidgetType::Selector,
        Some(crate::layout::WidgetKind::Dropdown) => widgets::WidgetType::Dropdown,
        Some(crate::layout::WidgetKind::Meter) => widgets::WidgetType::Meter,
        Some(crate::layout::WidgetKind::XYPad) => widgets::WidgetType::XYPad,
        None => {
            let param_info = params.param_infos().into_iter()
                .find(|i| i.id == param_id);
            match param_info.as_ref().map(|i| &i.range) {
                Some(truce_params::ParamRange::Discrete { min: 0, max: 1 }) => widgets::WidgetType::Toggle,
                Some(truce_params::ParamRange::Enum { .. }) => widgets::WidgetType::Selector,
                _ => widgets::WidgetType::Knob,
            }
        }
    }
}

impl<P: Params + 'static> Editor for BuiltinEditor<P> {
    fn size(&self) -> (u32, u32) {
        (self.layout.width(), self.layout.height())
    }

    fn scale_factor(&self) -> f64 {
        // BuiltinEditor reports size in logical points. The NSView is created
        // in points and AppKit handles Retina scaling. Return 1.0 so format
        // wrappers pass the logical size to the host unchanged.
        1.0
    }

    fn open(&mut self, parent: RawWindowHandle, context: EditorContext) {
        let (w, h) = self.size();
        self.backend = CpuBackend::new(w, h);
        self.context = Some(context);

        // Build interaction regions
        match &self.layout {
            Layout::Rows(pl) => self.interaction.build_regions(pl),
            Layout::Grid(gl) => self.interaction.build_regions_grid(gl),
        }

        // Render initial frame
        self.render();

        // Create platform view if we have a parent window
        let parent_ptr = match parent {
            RawWindowHandle::AppKit(ptr) => ptr,
            #[allow(unused)]
            _ => std::ptr::null_mut(),
        };

        if !parent_ptr.is_null() {
            let self_ptr = self as *mut BuiltinEditor<P> as *mut c_void;
            self.self_ptr = self_ptr;

            let callbacks = ViewCallbacks {
                render: Some(cb_render::<P>),
                mouse_down: Some(cb_mouse_down::<P>),
                mouse_dragged: Some(cb_mouse_dragged::<P>),
                mouse_up: Some(cb_mouse_up::<P>),
                scroll: Some(cb_scroll::<P>),
                double_click: Some(cb_double_click::<P>),
                mouse_moved: Some(cb_mouse_moved::<P>),
            };

            self.view = unsafe { PlatformView::new(parent_ptr, w, h, self_ptr, &callbacks) };
        }
    }

    fn close(&mut self) {
        self.view = None;
        self.context = None;
        self.backend = None;
        self.self_ptr = std::ptr::null_mut();
    }

    fn idle(&mut self) {
        // Platform view handles its own repaint timer.
        // If no platform view (standalone mode), render for external consumption.
        if self.view.is_none() {
            self.render();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::{GridLayout, GridWidget, Layout};
    use crate::widgets::WidgetType;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::Arc;
    use truce_params::{ParamInfo, ParamRange, ParamFlags, ParamUnit, Params};

    // -- Mock Params with one enum param (4 options) and one float --

    struct TestParams {
        values: [AtomicU64; 2],
    }

    impl TestParams {
        fn new() -> Self {
            Self {
                values: [
                    AtomicU64::new(0.0f64.to_bits()),
                    AtomicU64::new(0.0f64.to_bits()),
                ],
            }
        }
    }

    impl Params for TestParams {
        fn param_infos(&self) -> Vec<ParamInfo> {
            vec![
                ParamInfo {
                    id: 0,
                    name: "Mode",
                    short_name: "Mode",
                    group: "",
                    range: ParamRange::Enum { count: 4 },
                    default_plain: 0.0,
                    flags: ParamFlags::AUTOMATABLE,
                    unit: ParamUnit::None,
                },
                ParamInfo {
                    id: 1,
                    name: "Gain",
                    short_name: "Gain",
                    group: "",
                    range: ParamRange::Linear { min: 0.0, max: 1.0 },
                    default_plain: 0.5,
                    flags: ParamFlags::AUTOMATABLE,
                    unit: ParamUnit::None,
                },
            ]
        }

        fn count(&self) -> usize { 2 }

        fn get_normalized(&self, id: u32) -> Option<f64> {
            self.values.get(id as usize)
                .map(|v| f64::from_bits(v.load(Ordering::Relaxed)))
        }

        fn set_normalized(&self, id: u32, value: f64) {
            if let Some(v) = self.values.get(id as usize) {
                v.store(value.to_bits(), Ordering::Relaxed);
            }
        }

        fn get_plain(&self, id: u32) -> Option<f64> {
            let norm = self.get_normalized(id)?;
            let info = self.param_infos().into_iter().find(|i| i.id == id)?;
            Some(info.range.denormalize(norm))
        }

        fn set_plain(&self, id: u32, value: f64) {
            if let Some(info) = self.param_infos().into_iter().find(|i| i.id == id) {
                self.set_normalized(id, info.range.normalize(value));
            }
        }

        fn format_value(&self, _id: u32, value: f64) -> Option<String> {
            Some(format!("{:.0}", value))
        }

        fn parse_value(&self, _id: u32, _text: &str) -> Option<f64> { None }
        fn snap_smoothers(&self) {}
        fn set_sample_rate(&self, _: f64) {}

        fn collect_values(&self) -> (Vec<u32>, Vec<f64>) {
            let ids = vec![0, 1];
            let vals: Vec<f64> = ids.iter().map(|&id| {
                self.get_plain(id).unwrap_or(0.0)
            }).collect();
            (ids, vals)
        }

        fn restore_values(&self, values: &[(u32, f64)]) {
            for &(id, val) in values {
                self.set_plain(id, val);
            }
        }

        fn default_for_gui() -> Self { Self::new() }
    }

    // -- Helpers --

    /// Build a BuiltinEditor with a dropdown at position 0 and a knob at position 1.
    fn make_editor() -> BuiltinEditor<TestParams> {
        let params = Arc::new(TestParams::new());
        let layout = GridLayout::build("TEST", "V0.1", 2, 80.0, vec![
            GridWidget::dropdown(0u32, "Mode"),
            GridWidget::knob(1u32, "Gain"),
        ], vec![]);
        let mut editor = BuiltinEditor::new_grid(params, layout);
        // Build interaction regions (normally done in open/render)
        if let Layout::Grid(ref gl) = editor.layout {
            editor.interaction.build_regions_grid(gl);
            for (idx, gw) in gl.widgets.iter().enumerate() {
                if let Some(region) = editor.interaction.knob_regions.get_mut(idx) {
                    region.widget_type = resolve_widget_type(
                        gw.widget, gw.param_id, &*editor.params,
                    );
                }
            }
        }
        // Render once to populate dropdown_anchor_y
        editor.render();
        editor
    }

    /// Build an editor with section breaks to test anchor stability.
    fn make_editor_with_sections() -> BuiltinEditor<TestParams> {
        let params = Arc::new(TestParams::new());
        let layout = GridLayout::build("TEST", "V0.1", 2, 80.0, vec![
            GridWidget::knob(1u32, "Gain"),
            GridWidget::knob(1u32, "Gain 2"),
            GridWidget::dropdown(0u32, "Mode"),   // row 1, after a section break
            GridWidget::knob(1u32, "Gain 3"),
        ], vec![
            (0, "SECTION A"),
            (2, "SECTION B"),
        ]);
        let mut editor = BuiltinEditor::new_grid(params, layout);
        if let Layout::Grid(ref gl) = editor.layout {
            editor.interaction.build_regions_grid(gl);
            for (idx, gw) in gl.widgets.iter().enumerate() {
                if let Some(region) = editor.interaction.knob_regions.get_mut(idx) {
                    region.widget_type = resolve_widget_type(
                        gw.widget, gw.param_id, &*editor.params,
                    );
                }
            }
        }
        editor.render();
        editor
    }

    /// Find the center of the first dropdown widget's region.
    fn dropdown_center(editor: &BuiltinEditor<TestParams>) -> (f32, f32) {
        let region = editor.interaction.knob_regions.iter()
            .find(|r| r.widget_type == WidgetType::Dropdown)
            .expect("no dropdown in layout");
        (region.x + region.w / 2.0, region.y + region.h / 2.0)
    }

    // -- Tests: dropdown close-on-reclick --

    #[test]
    fn dropdown_click_opens() {
        let mut editor = make_editor();
        let (dx, dy) = dropdown_center(&editor);

        editor.on_mouse_down(dx, dy);
        assert!(editor.interaction.dropdown_is_open());
    }

    #[test]
    fn dropdown_click_toggles_closed() {
        let mut editor = make_editor();
        let (dx, dy) = dropdown_center(&editor);

        // Open
        editor.on_mouse_down(dx, dy);
        editor.on_mouse_up(dx, dy);
        assert!(editor.interaction.dropdown_is_open());

        // Click same button again — should close, not reopen
        editor.on_mouse_down(dx, dy);
        assert!(!editor.interaction.dropdown_is_open());
    }

    #[test]
    fn dropdown_click_outside_closes() {
        let mut editor = make_editor();
        let (dx, dy) = dropdown_center(&editor);

        editor.on_mouse_down(dx, dy);
        editor.on_mouse_up(dx, dy);
        assert!(editor.interaction.dropdown_is_open());

        // Click far away
        editor.on_mouse_down(0.0, 0.0);
        assert!(!editor.interaction.dropdown_is_open());
    }

    #[test]
    fn dropdown_click_option_selects_and_closes() {
        let mut editor = make_editor();
        let (dx, dy) = dropdown_center(&editor);

        editor.on_mouse_down(dx, dy);
        editor.on_mouse_up(dx, dy);
        assert!(editor.interaction.dropdown_is_open());

        // Click the second option (index 1) inside the popup
        let dd = editor.interaction.dropdown.as_ref().unwrap();
        let (px, py, _, _) = dd.popup_rect;
        let item_h = 18.0f32;
        let padding = 4.0f32;
        let option_y = py + padding + item_h + item_h / 2.0; // middle of second item

        editor.on_mouse_down(px + 10.0, option_y);

        assert!(!editor.interaction.dropdown_is_open());
        // Enum{count:4} → step_count=3 → 3 options. Index 1 → norm = 1/2 = 0.5
        let norm = editor.params.get_normalized(0).unwrap();
        assert!((norm - 0.5).abs() < 0.01, "expected 0.5, got {norm}");
    }

    // -- Tests: dropdown anchor positioning --

    #[test]
    fn dropdown_anchor_set_after_render() {
        let editor = make_editor();
        let region = editor.interaction.knob_regions.iter()
            .find(|r| r.widget_type == WidgetType::Dropdown)
            .unwrap();

        // Anchor should be within the widget region (below y, above y+h)
        assert!(region.dropdown_anchor_y > region.y,
            "anchor {} should be below region.y {}", region.dropdown_anchor_y, region.y);
        assert!(region.dropdown_anchor_y < region.y + region.h,
            "anchor {} should be above region bottom {}",
            region.dropdown_anchor_y, region.y + region.h);
    }

    #[test]
    fn dropdown_popup_uses_anchor() {
        let mut editor = make_editor();
        let (dx, dy) = dropdown_center(&editor);

        editor.on_mouse_down(dx, dy);
        editor.on_mouse_up(dx, dy);

        let dd = editor.interaction.dropdown.as_ref().unwrap();
        let region = &editor.interaction.knob_regions[dd.region_idx];

        // popup_rect.1 (popup_y) must equal the stored anchor
        assert_eq!(dd.popup_rect.1, region.dropdown_anchor_y);
    }

    #[test]
    fn dropdown_anchor_gap_stable_with_sections() {
        let editor_plain = make_editor();
        let editor_sections = make_editor_with_sections();

        let r_plain = editor_plain.interaction.knob_regions.iter()
            .find(|r| r.widget_type == WidgetType::Dropdown)
            .unwrap();
        let r_sections = editor_sections.interaction.knob_regions.iter()
            .find(|r| r.widget_type == WidgetType::Dropdown)
            .unwrap();

        // The gap from widget vertical center to anchor should be identical
        // regardless of section offsets shifting the absolute Y position.
        let gap_plain = r_plain.dropdown_anchor_y - (r_plain.y + r_plain.h / 2.0);
        let gap_sections = r_sections.dropdown_anchor_y - (r_sections.y + r_sections.h / 2.0);
        assert!(
            (gap_plain - gap_sections).abs() < 0.1,
            "gap_plain={gap_plain}, gap_sections={gap_sections}"
        );
    }
}
