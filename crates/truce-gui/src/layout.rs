//! Simple layout helpers for positioning widgets.

/// A widget definition in the layout.
#[derive(Clone, Debug)]
pub enum WidgetDef {
    /// Rotary knob (default for continuous params).
    Knob { param_id: u32, label: &'static str },
    /// Horizontal slider.
    Slider { param_id: u32, label: &'static str },
    /// Toggle button (on/off).
    Toggle { param_id: u32, label: &'static str },
}

impl WidgetDef {
    pub fn param_id(&self) -> u32 {
        match self {
            WidgetDef::Knob { param_id, .. } => *param_id,
            WidgetDef::Slider { param_id, .. } => *param_id,
            WidgetDef::Toggle { param_id, .. } => *param_id,
        }
    }

    pub fn label(&self) -> &'static str {
        match self {
            WidgetDef::Knob { label, .. } => label,
            WidgetDef::Slider { label, .. } => label,
            WidgetDef::Toggle { label, .. } => label,
        }
    }
}

/// A widget definition for the layout — either explicit type or auto-detected.
#[derive(Clone, Debug)]
pub struct KnobDef {
    pub param_id: u32,
    pub label: &'static str,
    /// Explicit widget type override. None = auto-detect from param range.
    pub widget: Option<WidgetKind>,
    /// How many grid columns this widget spans. Default = 1.
    pub span: u32,
    /// Second parameter ID for XY pad (Y axis). Ignored for other widgets.
    pub param_id_y: Option<u32>,
    /// Multiple meter IDs for multi-channel level meter. Ignored for other widgets.
    pub meter_ids: Option<Vec<u32>>,
}

/// Explicit widget type for layout overrides.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WidgetKind {
    Knob,
    Slider,
    Toggle,
    Selector,
    /// Dropdown list — click to open a popup showing all options.
    Dropdown,
    /// Level meter. Shows one bar per meter ID. Supports mono, stereo, or multi-channel.
    Meter,
    /// XY pad. Controls two params — X param stored in `param_id`, Y param in `xy_param_y`.
    XYPad,
}

impl KnobDef {
    /// Knob (default for continuous params, auto-detected anyway).
    pub fn knob(param_id: impl Into<u32>, label: &'static str) -> Self {
        Self { param_id: param_id.into(), label, widget: Some(WidgetKind::Knob), span: 1, param_id_y: None, meter_ids: None }
    }

    /// Horizontal slider.
    pub fn slider(param_id: impl Into<u32>, label: &'static str) -> Self {
        Self { param_id: param_id.into(), label, widget: Some(WidgetKind::Slider), span: 1, param_id_y: None, meter_ids: None }
    }

    /// Toggle button.
    pub fn toggle(param_id: impl Into<u32>, label: &'static str) -> Self {
        Self { param_id: param_id.into(), label, widget: Some(WidgetKind::Toggle), span: 1, param_id_y: None, meter_ids: None }
    }

    /// Selector (click-to-cycle for enum params).
    pub fn selector(param_id: impl Into<u32>, label: &'static str) -> Self {
        Self { param_id: param_id.into(), label, widget: Some(WidgetKind::Selector), span: 1, param_id_y: None, meter_ids: None }
    }

    /// Dropdown list (click to open a popup showing all options).
    pub fn dropdown(param_id: impl Into<u32>, label: &'static str) -> Self {
        Self { param_id: param_id.into(), label, widget: Some(WidgetKind::Dropdown), span: 1, param_id_y: None, meter_ids: None }
    }

    /// Level meter with one or more channels (display-only, reads from Plugin::get_meter()).
    pub fn meter(ids: &[u32], label: &'static str) -> Self {
        Self {
            param_id: ids.first().copied().unwrap_or(0),
            label,
            widget: Some(WidgetKind::Meter),
            span: 1,
            param_id_y: None,
            meter_ids: Some(ids.to_vec()),
        }
    }

    /// XY pad controlling two parameters.
    pub fn xy_pad(param_x: impl Into<u32>, param_y: impl Into<u32>, label: &'static str) -> Self {
        Self { param_id: param_x.into(), label, widget: Some(WidgetKind::XYPad), span: 2, param_id_y: Some(param_y.into()), meter_ids: None }
    }

    /// Set the column span for this widget (default 1).
    pub fn with_span(mut self, span: u32) -> Self {
        self.span = span;
        self
    }
}

/// A row of widgets with an optional section label.
#[derive(Clone, Debug)]
pub struct KnobRow {
    pub label: Option<&'static str>,
    pub knobs: Vec<KnobDef>,
}

/// Layout configuration for a plugin UI.
#[derive(Clone, Debug)]
pub struct PluginLayout {
    pub title: &'static str,
    pub version: &'static str,
    pub rows: Vec<KnobRow>,
    pub width: u32,
    pub height: u32,
    pub knob_size: f32,
}

impl PluginLayout {
    /// Calculate default window size based on the layout.
    pub fn compute_size(rows: &[KnobRow], knob_size: f32) -> (u32, u32) {
        let header_h = 30.0;
        let row_h = knob_size + 30.0;
        let section_label_h = 18.0;
        let padding = 10.0;

        let max_knobs = rows.iter()
            .map(|r| r.knobs.iter().map(|k| k.span.max(1) as usize).sum::<usize>())
            .max().unwrap_or(1);
        let w = (max_knobs as f32 * (knob_size + 10.0) + 20.0).max(300.0);

        let mut h = header_h + padding;
        for row in rows {
            if row.label.is_some() {
                h += section_label_h;
            }
            h += row_h + padding;
        }

        (w as u32, h as u32)
    }

    /// Calculate default window size and return a PluginLayout.
    pub fn build(
        title: &'static str,
        version: &'static str,
        rows: Vec<KnobRow>,
        knob_size: f32,
    ) -> Self {
        let (w, h) = Self::compute_size(&rows, knob_size);
        Self {
            title,
            version,
            rows,
            width: w,
            height: h,
            knob_size,
        }
    }
}

// ---------------------------------------------------------------------------
// Grid Layout
// ---------------------------------------------------------------------------

/// Sentinel value for auto-placed grid widgets.
pub const AUTO: u32 = u32::MAX;

// Grid spacing constants.
pub const GRID_GAP: f32 = 30.0;
pub const GRID_PADDING: f32 = 15.0;
pub const GRID_HEADER_H: f32 = 30.0;
pub const GRID_SECTION_H: f32 = 18.0;

/// A widget placed in a grid layout.
#[derive(Clone, Debug)]
pub struct GridWidget {
    /// Grid column (0-indexed, or AUTO for auto-flow).
    pub col: u32,
    /// Grid row (0-indexed, or AUTO for auto-flow).
    pub row: u32,
    /// Columns spanned (default 1).
    pub col_span: u32,
    /// Rows spanned (default 1).
    pub row_span: u32,
    /// Parameter ID (or first meter ID for meters).
    pub param_id: u32,
    /// Display label.
    pub label: &'static str,
    /// Widget type override. None = auto-detect from param range.
    pub widget: Option<WidgetKind>,
    /// Second param for XY pad (Y axis).
    pub param_id_y: Option<u32>,
    /// Multiple meter IDs for multi-channel level meter.
    pub meter_ids: Option<Vec<u32>>,
}

impl GridWidget {
    pub fn knob(param_id: impl Into<u32>, label: &'static str) -> Self {
        Self {
            col: AUTO, row: AUTO, col_span: 1, row_span: 1,
            param_id: param_id.into(), label, widget: Some(WidgetKind::Knob),
            param_id_y: None, meter_ids: None,
        }
    }

    pub fn slider(param_id: impl Into<u32>, label: &'static str) -> Self {
        Self {
            col: AUTO, row: AUTO, col_span: 1, row_span: 1,
            param_id: param_id.into(), label, widget: Some(WidgetKind::Slider),
            param_id_y: None, meter_ids: None,
        }
    }

    pub fn toggle(param_id: impl Into<u32>, label: &'static str) -> Self {
        Self {
            col: AUTO, row: AUTO, col_span: 1, row_span: 1,
            param_id: param_id.into(), label, widget: Some(WidgetKind::Toggle),
            param_id_y: None, meter_ids: None,
        }
    }

    pub fn selector(param_id: impl Into<u32>, label: &'static str) -> Self {
        Self {
            col: AUTO, row: AUTO, col_span: 1, row_span: 1,
            param_id: param_id.into(), label, widget: Some(WidgetKind::Selector),
            param_id_y: None, meter_ids: None,
        }
    }

    pub fn dropdown(param_id: impl Into<u32>, label: &'static str) -> Self {
        Self {
            col: AUTO, row: AUTO, col_span: 1, row_span: 1,
            param_id: param_id.into(), label, widget: Some(WidgetKind::Dropdown),
            param_id_y: None, meter_ids: None,
        }
    }

    pub fn meter(ids: &[u32], label: &'static str) -> Self {
        Self {
            col: AUTO, row: AUTO, col_span: 1, row_span: 1,
            param_id: ids.first().copied().unwrap_or(0), label,
            widget: Some(WidgetKind::Meter),
            param_id_y: None, meter_ids: Some(ids.to_vec()),
        }
    }

    pub fn xy_pad(param_x: impl Into<u32>, param_y: impl Into<u32>, label: &'static str) -> Self {
        Self {
            col: AUTO, row: AUTO, col_span: 2, row_span: 2,
            param_id: param_x.into(), label, widget: Some(WidgetKind::XYPad),
            param_id_y: Some(param_y.into()), meter_ids: None,
        }
    }

    /// Set the column span.
    pub fn cols(mut self, n: u32) -> Self {
        self.col_span = n;
        self
    }

    /// Set the row span.
    pub fn rows(mut self, n: u32) -> Self {
        self.row_span = n;
        self
    }

    /// Set explicit grid position (overrides auto-flow for this widget).
    pub fn at(mut self, col: u32, row: u32) -> Self {
        self.col = col;
        self.row = row;
        self
    }
}

/// Grid-based layout for a plugin UI.
#[derive(Clone, Debug)]
pub struct GridLayout {
    pub title: &'static str,
    pub version: &'static str,
    /// Number of columns in the grid.
    pub cols: u32,
    /// Section labels positioned above specific rows: (row_index, label).
    pub sections: Vec<(u32, &'static str)>,
    /// All widgets placed in the grid.
    pub widgets: Vec<GridWidget>,
    /// Cell size in pixels (width and height of one grid cell).
    pub cell_size: f32,
    /// Computed pixel width.
    pub width: u32,
    /// Computed pixel height.
    pub height: u32,
}

impl GridLayout {
    /// Build a grid layout from auto-flow widgets and section breaks.
    ///
    /// `breaks` maps widget indices to section labels: when auto-flow reaches
    /// that index, it starts a new row and records the section label above it.
    pub fn build(
        title: &'static str,
        version: &'static str,
        cols: u32,
        cell_size: f32,
        widgets: Vec<GridWidget>,
        breaks: Vec<(usize, &'static str)>,
    ) -> Self {
        let mut layout = Self {
            title, version, cols,
            sections: Vec::new(),
            widgets, cell_size,
            width: 0, height: 0,
        };
        layout.auto_flow_with_breaks(&breaks);
        let (w, h) = layout.compute_size();
        layout.width = w;
        layout.height = h;
        layout
    }

    /// Compute the window size from the grid.
    pub fn compute_size(&self) -> (u32, u32) {
        let max_row = self.widgets.iter()
            .map(|w| w.row + w.row_span)
            .max().unwrap_or(1);
        let section_count = self.sections.len() as f32;

        let w = GRID_PADDING * 2.0 + self.cols as f32 * (self.cell_size + GRID_GAP) - GRID_GAP;
        let bottom_label_h = 28.0; // label + value text below the last row of widgets
        let h = GRID_HEADER_H + GRID_PADDING
            + max_row as f32 * (self.cell_size + GRID_GAP) - GRID_GAP
            + section_count * GRID_SECTION_H
            + bottom_label_h + GRID_PADDING;

        (w.max(300.0) as u32, h as u32)
    }

    /// Auto-flow placement without section breaks.
    pub fn auto_flow(&mut self) {
        self.auto_flow_with_breaks(&[]);
    }

    /// Auto-flow placement with section breaks.
    ///
    /// Each break is `(widget_index, label)`: when the cursor reaches that
    /// widget index, it advances to the next row and records a section label.
    pub fn auto_flow_with_breaks(&mut self, breaks: &[(usize, &'static str)]) {
        let mut occupied = std::collections::HashSet::new();
        let mut cursor_col: u32 = 0;
        let mut cursor_row: u32 = 0;
        let mut any_emitted = false;

        // First pass: mark cells occupied by explicitly-placed widgets.
        for w in &self.widgets {
            if w.col != AUTO && w.row != AUTO {
                for c in w.col..w.col + w.col_span {
                    for r in w.row..w.row + w.row_span {
                        occupied.insert((c, r));
                    }
                }
            }
        }

        // Second pass: auto-place widgets.
        for (i, w) in self.widgets.iter_mut().enumerate() {
            // Check for section breaks at this widget index.
            for &(break_idx, label) in breaks {
                if break_idx == i {
                    if any_emitted || cursor_col > 0 {
                        cursor_row += 1;
                        cursor_col = 0;
                    }
                    self.sections.push((cursor_row, label));
                    any_emitted = true;
                }
            }

            if w.col != AUTO && w.row != AUTO {
                // Explicitly placed — already marked in first pass.
                any_emitted = true;
                continue;
            }

            // Find next free position that fits this widget.
            loop {
                if cursor_col + w.col_span > self.cols {
                    cursor_col = 0;
                    cursor_row += 1;
                }
                let fits = (0..w.col_span).all(|dc|
                    (0..w.row_span).all(|dr|
                        !occupied.contains(&(cursor_col + dc, cursor_row + dr))
                    )
                );
                if fits { break; }
                cursor_col += 1;
            }

            w.col = cursor_col;
            w.row = cursor_row;

            for c in w.col..w.col + w.col_span {
                for r in w.row..w.row + w.row_span {
                    occupied.insert((c, r));
                }
            }

            cursor_col += w.col_span;
            any_emitted = true;
        }
    }
}

/// Compute cumulative section-label pixel offsets per row.
///
/// `offsets[r]` is the total vertical shift (from section labels) for row `r`.
pub fn compute_section_offsets(layout: &GridLayout) -> Vec<f32> {
    let max_row = layout.widgets.iter()
        .map(|w| w.row + w.row_span)
        .max().unwrap_or(1);
    let mut offsets = vec![0.0f32; max_row as usize + 1];
    let mut cumulative = 0.0;

    for row in 0..=max_row {
        if layout.sections.iter().any(|(r, _)| *r == row) {
            cumulative += GRID_SECTION_H;
        }
        if (row as usize) < offsets.len() {
            offsets[row as usize] = cumulative;
        }
    }
    offsets
}

impl From<PluginLayout> for GridLayout {
    fn from(pl: PluginLayout) -> Self {
        let cols = pl.rows.iter()
            .map(|r| r.knobs.iter().map(|k| k.span.max(1)).sum::<u32>())
            .max().unwrap_or(1);

        let mut widgets = Vec::new();
        let mut sections = Vec::new();
        let mut grid_row = 0u32;

        for row in &pl.rows {
            if let Some(label) = row.label {
                sections.push((grid_row, label));
            }
            let mut col = 0u32;
            for knob in &row.knobs {
                widgets.push(GridWidget {
                    col, row: grid_row,
                    col_span: knob.span.max(1), row_span: 1,
                    param_id: knob.param_id,
                    label: knob.label,
                    widget: knob.widget,
                    param_id_y: knob.param_id_y,
                    meter_ids: knob.meter_ids.clone(),
                });
                col += knob.span.max(1);
            }
            grid_row += 1;
        }

        let mut gl = GridLayout {
            title: pl.title, version: pl.version,
            cols, sections, widgets, cell_size: pl.knob_size,
            width: 0, height: 0,
        };
        let (w, h) = gl.compute_size();
        gl.width = w;
        gl.height = h;
        gl
    }
}

/// Layout variant for editor dispatch.
#[derive(Clone, Debug)]
pub enum Layout {
    Rows(PluginLayout),
    Grid(GridLayout),
}

impl Layout {
    pub fn width(&self) -> u32 {
        match self { Layout::Rows(l) => l.width, Layout::Grid(g) => g.width }
    }
    pub fn height(&self) -> u32 {
        match self { Layout::Rows(l) => l.height, Layout::Grid(g) => g.height }
    }
    pub fn title(&self) -> &str {
        match self { Layout::Rows(l) => l.title, Layout::Grid(g) => g.title }
    }
    pub fn version(&self) -> &str {
        match self { Layout::Rows(l) => l.version, Layout::Grid(g) => g.version }
    }
}
