//! Simple layout helpers for positioning widgets.

// ---------------------------------------------------------------------------
// Rows-layout shared constants
// ---------------------------------------------------------------------------
//
// Coordinates the rows-layout uses to step through `Row`s. `widgets::draw_rows`
// (paint side) and `interaction::build_regions` (hit-test side) walk the
// rows in lock-step, so they have to agree on these step sizes - drift
// would make hover / drag rectangles miss the painted widget.

/// Pixel height of the title-bar header `widgets::draw_header` paints
/// at the top of the editor.
pub const HEADER_HEIGHT: f32 = 20.0;

/// Y-offset of the first row below the header. The 4-pixel gap between
/// `HEADER_HEIGHT` and `ROWS_LAYOUT_TOP` is the breathing room between
/// the title bar and the first row of widgets.
pub const ROWS_LAYOUT_TOP: f32 = 24.0;

/// Vertical pixels reserved for a section label (`Row::label`) drawn
/// above its row.
pub const ROWS_SECTION_LABEL_HEIGHT: f32 = 14.0;

/// Horizontal gap between adjacent widgets in a row. The full pitch
/// between widget origins is `knob_size + ROWS_COLUMN_GAP`.
pub const ROWS_COLUMN_GAP: f32 = 7.0;

/// Vertical gap below a row. The full pitch between row origins is
/// `knob_size + ROWS_ROW_GAP`.
pub const ROWS_ROW_GAP: f32 = 19.0;

// ---------------------------------------------------------------------------
// Dropdown widget shared constants
// ---------------------------------------------------------------------------

/// Pixel height of the dropdown button box (the closed state - clicking
/// this opens the popup). Both `widgets::draw_dropdown` (paint side) and
/// `interaction::open_dropdown` (popup-anchor math) need to agree.
pub const DROPDOWN_BOX_HEIGHT: f32 = 20.0;

use truce_core::cast::len_u32;

/// A widget definition for the layout - either explicit type or auto-detected.
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
    /// Dropdown list - click to open a popup showing all options.
    Dropdown,
    /// Level meter. Shows one bar per meter ID. Supports mono, stereo, or multi-channel.
    Meter,
    /// XY pad. Controls two params - X param stored in `param_id`, Y param in `xy_param_y`.
    XYPad,
}

impl KnobDef {
    /// Knob (default for continuous params, auto-detected anyway).
    pub fn knob(param_id: impl Into<u32>, label: &'static str) -> Self {
        Self {
            param_id: param_id.into(),
            label,
            widget: Some(WidgetKind::Knob),
            span: 1,
            param_id_y: None,
            meter_ids: None,
        }
    }

    /// Horizontal slider.
    pub fn slider(param_id: impl Into<u32>, label: &'static str) -> Self {
        Self {
            param_id: param_id.into(),
            label,
            widget: Some(WidgetKind::Slider),
            span: 1,
            param_id_y: None,
            meter_ids: None,
        }
    }

    /// Toggle button.
    pub fn toggle(param_id: impl Into<u32>, label: &'static str) -> Self {
        Self {
            param_id: param_id.into(),
            label,
            widget: Some(WidgetKind::Toggle),
            span: 1,
            param_id_y: None,
            meter_ids: None,
        }
    }

    /// Dropdown list (click to open a popup showing all options).
    pub fn dropdown(param_id: impl Into<u32>, label: &'static str) -> Self {
        Self {
            param_id: param_id.into(),
            label,
            widget: Some(WidgetKind::Dropdown),
            span: 1,
            param_id_y: None,
            meter_ids: None,
        }
    }

    /// Level meter with one or more channels (display-only, reads from `Plugin::get_meter()`).
    #[must_use]
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
        Self {
            param_id: param_x.into(),
            label,
            widget: Some(WidgetKind::XYPad),
            span: 2,
            param_id_y: Some(param_y.into()),
            meter_ids: None,
        }
    }

    /// Set the column span for this widget (default 1).
    #[must_use]
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
    pub titles: HeaderTitles,
    pub rows: Vec<KnobRow>,
    pub width: u32,
    pub height: u32,
    pub knob_size: f32,
}

impl PluginLayout {
    /// Calculate default window size based on the layout.
    // Window dimensions in logical pixels stay well below 2^23, so the
    // f32 ↔ u32 narrowings are invisible in practice.
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        clippy::cast_precision_loss
    )]
    #[must_use]
    pub fn compute_size(rows: &[KnobRow], knob_size: f32, titles: &HeaderTitles) -> (u32, u32) {
        let header_h = if titles.is_empty() { 0.0 } else { 21.0 };
        let row_h = knob_size + 19.0;
        let section_label_h = 14.0;
        let padding = 7.0;

        let max_knobs = rows
            .iter()
            .map(|r| {
                r.knobs
                    .iter()
                    .map(|k| k.span.max(1) as usize)
                    .sum::<usize>()
            })
            .max()
            .unwrap_or(1);
        let w = max_knobs as f32 * (knob_size + 7.0) + 13.0;

        let mut h = header_h + padding;
        for row in rows {
            if row.label.is_some() {
                h += section_label_h;
            }
            h += row_h + padding;
        }

        (w as u32, h as u32)
    }

    /// Build a Rows-style layout with the given header titles.
    /// Either or both [`HeaderTitles`] slots can be empty (use
    /// [`HeaderTitles::none`] for a layout with no header band).
    #[must_use]
    pub fn build(titles: HeaderTitles, rows: Vec<KnobRow>, knob_size: f32) -> Self {
        let (w, h) = Self::compute_size(&rows, knob_size, &titles);
        Self {
            titles,
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

// Grid spacing constants. All dimensions in this module are in logical
// points - the rendering backend (`CpuBackend` / `WgpuBackend`)
// multiplies by the display scale factor at raster time.
pub const GRID_GAP: f32 = 19.0;
pub const GRID_PADDING: f32 = 10.0;
pub const GRID_HEADER_H: f32 = 21.0;
pub const GRID_SECTION_H: f32 = 14.0;

/// Slack added before flooring a requested size to a whole cell count
/// in `refit_cols` / `refit_rows`. Large enough to absorb float error
/// on an exactly cell-aligned request (so it doesn't drop a column),
/// far smaller than one cell so it never rounds a partial cell up.
const SNAP_EPS: f32 = 1e-3;

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
            col: AUTO,
            row: AUTO,
            col_span: 1,
            row_span: 1,
            param_id: param_id.into(),
            label,
            widget: Some(WidgetKind::Knob),
            param_id_y: None,
            meter_ids: None,
        }
    }

    pub fn slider(param_id: impl Into<u32>, label: &'static str) -> Self {
        Self {
            col: AUTO,
            row: AUTO,
            col_span: 1,
            row_span: 1,
            param_id: param_id.into(),
            label,
            widget: Some(WidgetKind::Slider),
            param_id_y: None,
            meter_ids: None,
        }
    }

    pub fn toggle(param_id: impl Into<u32>, label: &'static str) -> Self {
        Self {
            col: AUTO,
            row: AUTO,
            col_span: 1,
            row_span: 1,
            param_id: param_id.into(),
            label,
            widget: Some(WidgetKind::Toggle),
            param_id_y: None,
            meter_ids: None,
        }
    }

    pub fn dropdown(param_id: impl Into<u32>, label: &'static str) -> Self {
        Self {
            col: AUTO,
            row: AUTO,
            col_span: 1,
            row_span: 1,
            param_id: param_id.into(),
            label,
            widget: Some(WidgetKind::Dropdown),
            param_id_y: None,
            meter_ids: None,
        }
    }

    #[must_use]
    pub fn meter(ids: &[u32], label: &'static str) -> Self {
        Self {
            col: AUTO,
            row: AUTO,
            col_span: 1,
            row_span: 1,
            param_id: ids.first().copied().unwrap_or(0),
            label,
            widget: Some(WidgetKind::Meter),
            param_id_y: None,
            meter_ids: Some(ids.to_vec()),
        }
    }

    pub fn xy_pad(param_x: impl Into<u32>, param_y: impl Into<u32>, label: &'static str) -> Self {
        Self {
            col: AUTO,
            row: AUTO,
            col_span: 2,
            row_span: 2,
            param_id: param_x.into(),
            label,
            widget: Some(WidgetKind::XYPad),
            param_id_y: Some(param_y.into()),
            meter_ids: None,
        }
    }

    /// Set the column span.
    #[must_use]
    pub fn cols(mut self, n: u32) -> Self {
        self.col_span = n;
        self
    }

    /// Set the row span.
    #[must_use]
    pub fn rows(mut self, n: u32) -> Self {
        self.row_span = n;
        self
    }

    /// Set explicit grid position (overrides auto-flow for this widget).
    #[must_use]
    pub fn at(mut self, col: u32, row: u32) -> Self {
        self.col = col;
        self.row = row;
        self
    }
}

/// A group of widgets with an optional section label.
///
/// Used as input to `GridLayout::build()`. Bare `GridWidget`s convert into
/// ungrouped sections via `From`, so orphan widgets only need `.into()`.
#[derive(Clone, Debug)]
pub struct Section {
    pub label: Option<&'static str>,
    pub widgets: Vec<GridWidget>,
}

/// Create a labeled section of widgets for `GridLayout::build()`.
#[must_use]
pub fn section(label: &'static str, widgets: Vec<GridWidget>) -> Section {
    Section {
        label: Some(label),
        widgets,
    }
}

/// Wrap bare widgets into an unlabeled section (no section header).
#[must_use]
pub fn widgets(widgets: Vec<GridWidget>) -> Section {
    Section {
        label: None,
        widgets,
    }
}

// -- Short constructors for GridWidget (free functions) --

/// Rotary knob widget.
pub fn knob(param_id: impl Into<u32>, label: &'static str) -> GridWidget {
    GridWidget::knob(param_id, label)
}

/// Horizontal slider widget.
pub fn slider(param_id: impl Into<u32>, label: &'static str) -> GridWidget {
    GridWidget::slider(param_id, label)
}

/// Toggle switch widget.
pub fn toggle(param_id: impl Into<u32>, label: &'static str) -> GridWidget {
    GridWidget::toggle(param_id, label)
}

/// Dropdown list widget.
pub fn dropdown(param_id: impl Into<u32>, label: &'static str) -> GridWidget {
    GridWidget::dropdown(param_id, label)
}

/// Level meter widget.
pub fn meter<I: Into<u32> + Copy>(ids: &[I], label: &'static str) -> GridWidget {
    let u32_ids: Vec<u32> = ids.iter().map(|id| (*id).into()).collect();
    GridWidget::meter(&u32_ids, label)
}

/// XY pad controlling two parameters.
pub fn xy_pad(param_x: impl Into<u32>, param_y: impl Into<u32>, label: &'static str) -> GridWidget {
    GridWidget::xy_pad(param_x, param_y, label)
}

impl From<GridWidget> for Section {
    fn from(w: GridWidget) -> Self {
        Section {
            label: None,
            widgets: vec![w],
        }
    }
}

/// Title band drawn above a layout. The `title` slot renders
/// larger / brighter on the left of the band; the `subtitle` slot
/// renders smaller / dimmer on the right. Each slot is independently
/// optional - set either, both, or neither.
///
/// Use [`HeaderTitles::title`] / [`HeaderTitles::subtitle`] /
/// [`HeaderTitles::pair`] for the common cases; build the struct
/// directly only when you want to set a non-default combination
/// (e.g. via `..` syntax over an existing instance).
#[derive(Clone, Debug, Default)]
pub struct HeaderTitles {
    pub title: Option<&'static str>,
    pub subtitle: Option<&'static str>,
}

impl HeaderTitles {
    /// Both slots empty - no header band is drawn.
    #[must_use]
    pub const fn none() -> Self {
        Self {
            title: None,
            subtitle: None,
        }
    }

    /// Title only; subtitle slot stays empty.
    #[must_use]
    pub const fn title(s: &'static str) -> Self {
        Self {
            title: Some(s),
            subtitle: None,
        }
    }

    /// Subtitle only; title slot stays empty.
    #[must_use]
    pub const fn subtitle(s: &'static str) -> Self {
        Self {
            title: None,
            subtitle: Some(s),
        }
    }

    /// Both slots set.
    #[must_use]
    pub const fn pair(title: &'static str, subtitle: &'static str) -> Self {
        Self {
            title: Some(title),
            subtitle: Some(subtitle),
        }
    }

    /// `true` when neither slot is set - caller should skip the
    /// header band entirely.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.title.is_none() && self.subtitle.is_none()
    }
}

/// Grid-based layout for a plugin UI.
#[derive(Clone, Debug)]
pub struct GridLayout {
    /// Header band titles. Both slots default to `None`, in which
    /// case no header is drawn and the grid starts at `y = 0`
    /// (plus padding).
    pub titles: HeaderTitles,
    /// Number of columns in the grid.
    pub cols: u32,
    /// Section labels positioned above specific rows: (`row_index`, label).
    pub sections: Vec<(u32, &'static str)>,
    /// All widgets placed in the grid.
    pub widgets: Vec<GridWidget>,
    /// Cell size in logical points (width and height of one grid cell).
    pub cell_size: f32,
    /// Computed width in logical points.
    pub width: u32,
    /// Computed height in logical points.
    pub height: u32,
    /// Pre-flow widget snapshot - copy of `widgets` before
    /// `auto_flow_with_breaks` ran. Lets [`Self::with_cols`] reset
    /// and re-flow against a different column count without
    /// losing AUTO-vs-explicit placement.
    original_widgets: Vec<GridWidget>,
    /// Pre-flow section breaks - `(widget_index, label)` pairs as
    /// passed to `auto_flow_with_breaks` originally. Stored so
    /// re-flow recovers the same section labels.
    original_breaks: Vec<(usize, &'static str)>,
    /// Whether the host is allowed to drive a resize. Editors honour
    /// this via `Editor::can_resize`; `false` (default) keeps the
    /// layout at its built `cols` for fixed-size plugins.
    pub resizable: bool,
    /// Whether the standalone host may maximize the window. Editors
    /// honour this via `Editor::can_maximize`; `false` (default)
    /// removes the maximize affordance so maximizing can't grow the
    /// window past the grid's `max_size` into an empty margin. Only
    /// meaningful for `resizable` layouts - a fixed-size layout is
    /// pinned regardless. Opt in with `.maximizable(true)` for layouts
    /// that render correctly at any size.
    pub maximizable: bool,
    /// Lower clamp on host-driven resize requests, as `(cols, rows)`
    /// cell counts. Surfaced via `Editor::min_size` (converted to
    /// logical points by `compute_size` at the requested cell
    /// extent). Defaults to `(1, 1)`.
    ///
    /// The call shape (`.min_size((a, b))`) mirrors `truce-egui` /
    /// `truce-iced` / `truce-slint` / `truce-vizia` so cross-backend
    /// `editor()` impls stay symmetric; the *unit* is different
    /// because the grid is fundamentally cell-snapped (pixels
    /// without a cell boundary would just be rounded to one anyway).
    pub min_size: (u32, u32),
    /// Upper clamp on host-driven resize requests, as `(cols, rows)`
    /// cell counts. Defaults to `(u32::MAX, u32::MAX)` - effectively
    /// unbounded. Same units + call-shape contract as
    /// [`Self::min_size`].
    pub max_size: (u32, u32),
    /// Declared row extent. `compute_size` takes the larger of
    /// this and the widgets' rightmost row edge, the same way
    /// `cols` works on the width axis - so `refit_rows` can grow
    /// the grid past the rightmost widget's row with empty
    /// trailing space.
    pub rows: u32,
}

/// Default cell size in logical points when `GridLayout::build` is
/// called without `.with_cell_size(...)`. Matches the scaffolded
/// plugin's pre-refactor value so untouched scaffolds render the
/// same as before.
pub const GRID_DEFAULT_CELL_SIZE: f32 = 50.0;

impl GridLayout {
    /// Build a grid layout from sections containing widgets. No
    /// header is drawn, `cols` defaults to the widest section's
    /// widget count (extended to fit any explicitly-positioned
    /// widget), and `cell_size` defaults to
    /// [`GRID_DEFAULT_CELL_SIZE`]. Override any of those via
    /// [`Self::with_titles`] / [`Self::with_cols`] /
    /// [`Self::with_cell_size`].
    ///
    /// Each entry is either a `Section` (created with `section("LABEL", vec![...])`)
    /// or a bare `GridWidget` (auto-wrapped via `From`). Example:
    ///
    /// ```ignore
    /// GridLayout::build(vec![
    ///     section("LOW", vec![
    ///         GridWidget::knob(P::Freq, "Freq"),
    ///         GridWidget::knob(P::Gain, "Gain"),
    ///     ]),
    ///     GridWidget::knob(P::Output, "Output").into(),
    /// ])
    /// ```
    #[must_use]
    pub fn build(entries: Vec<Section>) -> Self {
        let mut widgets = Vec::new();
        let mut breaks = Vec::new();
        let mut max_widgets_per_section = 0usize;
        for s in entries {
            if let Some(label) = s.label {
                breaks.push((widgets.len(), label));
            }
            max_widgets_per_section = max_widgets_per_section.max(s.widgets.len());
            widgets.extend(s.widgets);
        }
        // Account for explicitly-positioned widgets that reach
        // beyond the widest auto-flow row - the grid still has to
        // be wide enough to seat them.
        let max_explicit_col = widgets
            .iter()
            .filter(|w| w.col != AUTO)
            .map(|w| w.col + w.col_span)
            .max()
            .unwrap_or(0);
        let cols = len_u32(max_widgets_per_section)
            .max(max_explicit_col)
            .max(1);

        let mut layout = Self {
            titles: HeaderTitles::none(),
            cols,
            sections: Vec::new(),
            widgets: widgets.clone(),
            cell_size: GRID_DEFAULT_CELL_SIZE,
            width: 0,
            height: 0,
            resizable: false,
            maximizable: false,
            min_size: (1, 1),
            max_size: (u32::MAX, u32::MAX),
            rows: 1,
            original_widgets: widgets,
            original_breaks: breaks,
        };
        layout.flow_and_size();
        layout
    }

    /// Override the default column count (which is the widest
    /// section's widget count, or whatever explicit positions
    /// require - whichever is larger). Use to force wrapping:
    /// `.with_cols(2)` on a 4-widget section produces a 2×2 grid.
    /// Recomputes auto-flow placement and window size.
    #[must_use]
    pub fn with_cols(mut self, cols: u32) -> Self {
        self.cols = cols.max(1);
        self.flow_and_size();
        self
    }

    /// Override the default cell size ([`GRID_DEFAULT_CELL_SIZE`]).
    /// The cell is square - this is both the width and height of
    /// one grid cell in logical points.
    #[must_use]
    pub fn with_cell_size(mut self, cell_size: f32) -> Self {
        self.cell_size = cell_size;
        let (w, h) = self.compute_size();
        self.width = w;
        self.height = h;
        self
    }

    /// Like [`Self::with_cols`] but accepts the cell size in the
    /// same call - useful when both are non-default. Equivalent to
    /// `.with_cell_size(s).with_cols(c)`.
    #[must_use]
    pub fn with_grid(mut self, cols: u32, cell_size: f32) -> Self {
        self = self.with_cell_size(cell_size);
        self.with_cols(cols)
    }

    /// Set both header slots at once. Replaces any previously
    /// configured titles. Recomputes the height to account for the
    /// extra band - width stays the same since the header spans the
    /// full grid width.
    ///
    /// ```ignore
    /// use truce_gui_types::layout::{GridLayout, HeaderTitles};
    /// GridLayout::build(sections).with_titles(HeaderTitles::pair("EQ", "v0.1"))
    /// ```
    #[must_use]
    pub fn with_titles(mut self, titles: HeaderTitles) -> Self {
        self.titles = titles;
        let (w, h) = self.compute_size();
        self.width = w;
        self.height = h;
        self
    }

    /// Set the title slot (left, larger / brighter), preserving any
    /// previously configured subtitle.
    ///
    /// ```ignore
    /// GridLayout::build(sections).with_title("EQ")
    /// ```
    #[must_use]
    pub fn with_title(mut self, title: &'static str) -> Self {
        self.titles.title = Some(title);
        let (w, h) = self.compute_size();
        self.width = w;
        self.height = h;
        self
    }

    /// Set the subtitle slot (right, smaller / dimmer), preserving
    /// any previously configured title.
    ///
    /// ```ignore
    /// GridLayout::build(sections).with_subtitle("v0.1")
    /// ```
    #[must_use]
    pub fn with_subtitle(mut self, subtitle: &'static str) -> Self {
        self.titles.subtitle = Some(subtitle);
        let (w, h) = self.compute_size();
        self.width = w;
        self.height = h;
        self
    }

    /// Opt the layout into host-driven resize. Defaults to `false`
    /// so existing plugins stay pinned at their built column count.
    /// When `true`, the editor honours `Editor::set_size` by snapping
    /// the requested width to the nearest whole cell + gap and
    /// re-flowing widgets through [`Self::refit_cols`].
    #[must_use]
    pub fn resizable(mut self, value: bool) -> Self {
        self.resizable = value;
        // `compute_size` reads `resizable` to decide whether the
        // declared `cols`/`rows` extend the natural size past the
        // flowed content, so refresh the cached dimensions - keeps
        // the result independent of where `.resizable()` lands in the
        // builder chain.
        let (w, h) = self.compute_size();
        self.width = w;
        self.height = h;
        self
    }

    /// Opt into the standalone host's maximize button. Defaults to
    /// `false`: maximize is removed on a `resizable` layout so it can't
    /// grow the window past the grid's `max_size` and leave an empty
    /// margin around the clamped editor (edge-drag resize within bounds
    /// is unaffected). Pass `true` for layouts that render correctly at
    /// any size. Only the standalone host consults this (plugin formats
    /// let the DAW own the window frame), and only when
    /// `resizable(true)`.
    #[must_use]
    pub fn maximizable(mut self, value: bool) -> Self {
        self.maximizable = value;
        self
    }

    /// Lower clamp on host-driven resize requests, as
    /// `(min_cols, min_rows)` - **cell counts, not pixels**.
    /// Default `(1, 1)`. Set this to keep the editor wide enough
    /// for the widest explicitly-positioned widget so column drops
    /// don't clip content, and tall enough for the bottommost
    /// widget's row.
    ///
    /// Call shape mirrors `truce-egui` / `truce-iced` /
    /// `truce-slint` / `truce-vizia`'s `min_size` (single tuple
    /// builder) so cross-backend `editor()` impls stay symmetric;
    /// the unit is cells because the grid is fundamentally
    /// cell-snapped. `Editor::min_size` reports the corresponding
    /// pixel size so hosts still see logical-point bounds.
    #[must_use]
    pub fn min_size(mut self, cells: (u32, u32)) -> Self {
        self.min_size = (cells.0.max(1), cells.1.max(1));
        self
    }

    /// Upper clamp on host-driven resize requests, as
    /// `(max_cols, max_rows)` - cell counts, not pixels. Default
    /// `(u32::MAX, u32::MAX)`. Cap this when the layout looks
    /// awkward past a certain stretch on either axis.
    ///
    /// Same units / call-shape contract as [`Self::min_size`].
    #[must_use]
    pub fn max_size(mut self, cells: (u32, u32)) -> Self {
        self.max_size = (cells.0.max(1), cells.1.max(1));
        self
    }

    /// Pin the natural row extent (in **cells**, not pixels). Same
    /// role on the height axis as [`Self::with_cols`] on the width
    /// axis: the layout's height is the larger of this and the
    /// rightmost row edge across all widgets, so the editor reserves
    /// N cell rows of vertical space even when the widgets don't
    /// fill them all.
    ///
    /// Distinct from [`Self::min_size`] / [`Self::max_size`]
    /// (resize *bounds* in logical points): `with_rows` declares the
    /// *natural* row count the editor opens at, while `min_size` /
    /// `max_size` clamp how far the host can resize away from that.
    #[must_use]
    pub fn with_rows(mut self, rows: u32) -> Self {
        self.rows = rows.max(1);
        let (w, h) = self.compute_size();
        self.width = w;
        self.height = h;
        self
    }

    /// Snap `target_h` to a whole row count and refresh the cached
    /// dimensions. Mirror of `refit_cols` on the height axis. The
    /// row count is derived from the height left over after the
    /// header band, sections, padding, and trailing label - so the
    /// snap is "row-precise" (each row is exactly `cell_size + gap`
    /// tall).
    // Same bounded casts as `refit_cols`.
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        clippy::cast_precision_loss
    )]
    pub fn refit_rows(&mut self, target_h: u32) -> (u32, u32) {
        let step = self.cell_size + GRID_GAP;
        if step <= 0.0 {
            let (w, h) = self.compute_size();
            self.width = w;
            self.height = h;
            return (w, h);
        }
        // Solve `compute_size`'s height formula for `rows`:
        //   h = header + 2*PADDING + rows*(cell_size + GAP) - GAP
        //         + section_count*GRID_SECTION_H + bottom_label_h
        let section_count = f32::from(u16::try_from(self.sections.len()).unwrap_or(u16::MAX));
        let bottom_label_h = 22.0;
        let overhead = self.header_height()
            + GRID_PADDING * 2.0
            + section_count * GRID_SECTION_H
            + bottom_label_h
            - GRID_GAP;
        let usable = (target_h as f32 - overhead).max(0.0);
        // Floor (not round) so the snapped height never exceeds the
        // request - see `refit_cols` for why overshoot makes the window
        // creep in hosts that skip `gui_adjust_size`.
        let raw = (usable / step + SNAP_EPS).floor();
        let new_rows = (raw.max(1.0) as u32).clamp(self.min_size.1.max(1), self.max_size.1.max(1));
        self.rows = new_rows;
        // Auto-flow doesn't care about `rows` (it's purely a
        // bookkeeping value for `compute_size`'s height), so we
        // skip the full `flow_and_size` round-trip and just
        // re-read the grid height. Report the requested height as the
        // canvas (see `refit_cols`); preserve the canvas width the
        // preceding `refit_cols` set rather than snapping it back to the
        // cell-aligned grid width, and clamp to `max_size` so the editor
        // stays self-enforcing regardless of host/WM behaviour.
        let (_, grid_h) = self.compute_size();
        self.height = if self.max_size.1 == u32::MAX {
            target_h.max(grid_h)
        } else {
            let max_h = self.max_snapped_size().1;
            target_h.clamp(grid_h, max_h)
        };
        (self.width, self.height)
    }

    /// Logical-point size of one resize step on either axis -
    /// `cell_size + GRID_GAP`, the same `step` `refit_cols` /
    /// `refit_rows` snap to. Both axes share it. Drives the
    /// standalone X11 host's WM resize-increment hint so edge-drags
    /// snap to whole cells. Floors at 1 so a degenerate step never
    /// produces a zero increment.
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    #[must_use]
    pub fn resize_step(&self) -> u32 {
        (self.cell_size + GRID_GAP).round().max(1.0) as u32
    }

    /// `Editor::min_size` value: the pixel size of the layout at
    /// the smallest allowed `(cols, rows)` extent declared by
    /// [`Self::min_size`]. Hosts see this via the format-specific
    /// resize-hint RPC (CLAP `gui_get_resize_hints`, VST3
    /// `checkSizeConstraint`, etc.).
    #[must_use]
    pub fn min_snapped_size(&self) -> (u32, u32) {
        let mut probe = self.clone();
        probe.cols = self.min_size.0.max(1);
        probe.rows = self.min_size.1.max(1);
        probe.flow_and_size();
        (probe.width, probe.height)
    }

    /// `Editor::max_size` value. `u32::MAX` per axis means "no
    /// cap" and probes at 64 cells - well past any plugin window
    /// a host would render, and small enough that the layout math
    /// doesn't overflow.
    #[must_use]
    pub fn max_snapped_size(&self) -> (u32, u32) {
        let cap = 64u32;
        let cols = if self.max_size.0 == u32::MAX {
            cap
        } else {
            self.max_size.0.max(1)
        };
        let rows = if self.max_size.1 == u32::MAX {
            cap
        } else {
            self.max_size.1.max(1)
        };
        let mut probe = self.clone();
        probe.cols = cols;
        probe.rows = rows;
        probe.flow_and_size();
        (probe.width, probe.height)
    }

    /// Reflow against a column count derived from the requested
    /// logical width: snap the width to the nearest whole
    /// `cell_size + gap` step (so the grid always ends on a cell
    /// boundary), clamp the result to `[min_cols, max_cols]`, and
    /// re-run auto-flow against the new column count. Returns the
    /// resulting `(width, height)`.
    ///
    /// The cell pixel size stays put — only the column count
    /// changes, so widgets stay at their built size and just
    /// re-pack into a wider or narrower grid. Auto-positioned
    /// widgets reflow naturally; explicitly-positioned widgets
    /// stay at their declared `(col, row)` (so dropping below
    /// their column would clip them, hence the `min_cols`
    /// safeguard).
    ///
    /// `target_w` is interpreted as logical points - same units
    /// `Editor::set_size` works in.
    // Widget grid coords stay under 1000 cells and total pixel
    // dimensions stay under 2^23, so the casts are bounded.
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        clippy::cast_precision_loss
    )]
    pub fn refit_cols(&mut self, target_w: u32) -> (u32, u32) {
        // Solve `compute_size`'s width formula for `cols`:
        //   w = 2*PADDING + cols * (cell_size + GAP) - GAP
        // => cols = (w + GAP - 2*PADDING) / (cell_size + GAP)
        let step = self.cell_size + GRID_GAP;
        if step <= 0.0 {
            let (w, h) = self.compute_size();
            self.width = w;
            self.height = h;
            return (w, h);
        }
        let numerator = target_w as f32 + GRID_GAP - GRID_PADDING * 2.0;
        // Floor (not round) so the snapped width never *exceeds* the
        // requested width. Rounding up reports a window bigger than the
        // host asked for; hosts that skip `gui_adjust_size` and call
        // `set_size` with raw drag dimensions (e.g. Reaper) then grow
        // their frame to fit, which feeds an even larger drag size back
        // in - the window creeps/grows without bound. The small epsilon
        // absorbs float error so an exactly cell-aligned width (the
        // standalone case, where WM resize-increment hints already
        // deliver aligned sizes) still lands on the intended column.
        let raw = (numerator / step + SNAP_EPS).floor();
        let new_cols = (raw.max(1.0) as u32).clamp(self.min_size.0.max(1), self.max_size.0.max(1));
        self.cols = new_cols;
        self.flow_and_size();
        // Report the *requested* width as the canvas, not the cell-aligned
        // grid width. Flooring `cols` above guarantees the grid is never
        // wider than the request, so the leftover (canvas - grid) is
        // right-edge margin the renderer clears to the background. This
        // makes the editor fill the host's window exactly (no creep, no
        // uninitialised-pixel gap in hosts that keep their frame at an
        // un-quantised size), while the clamp below keeps the editor
        // self-enforcing: it never reports/fills past `max_size` even when
        // a host or WM ignores our size hints / skips the wrapper clamp
        // (the standalone hands raw drag sizes straight to `set_size`).
        let grid_w = self.width; // grid at the clamped column count (>= min)
        self.width = if self.max_size.0 == u32::MAX {
            target_w.max(grid_w)
        } else {
            let max_w = self.max_snapped_size().0;
            target_w.clamp(grid_w, max_w)
        };
        (self.width, self.height)
    }

    /// Pixel height of the header band, or `0.0` when neither
    /// title slot is set. Internal helper used by `compute_size`,
    /// `widgets::draw_grid`, and `interaction::build_regions_grid`
    /// to keep the "is there a header?" check in one place.
    pub(crate) fn header_height(&self) -> f32 {
        if self.titles.is_empty() {
            0.0
        } else {
            GRID_HEADER_H
        }
    }

    /// Reset to the pre-flow widget snapshot, run `auto_flow_with_breaks`
    /// against `self.cols`, then recompute window size. Used by
    /// `build`, `with_cols`, and `with_cell_size` so the layout
    /// stays consistent after any configuration change.
    fn flow_and_size(&mut self) {
        self.widgets = self.original_widgets.clone();
        self.sections.clear();
        let breaks: Vec<(usize, &'static str)> = self.original_breaks.clone();
        self.auto_flow_with_breaks(&breaks);
        let (w, h) = self.compute_size();
        self.width = w;
        self.height = h;
    }

    /// Compute the window size from the grid.
    // Window dimensions in logical pixels stay well below 2^23.
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        clippy::cast_precision_loss
    )]
    #[must_use]
    pub fn compute_size(&self) -> (u32, u32) {
        // The natural size always tracks the flowed widget extent so
        // a fixed-size editor is exactly as wide/tall as its content.
        //
        // For *resizable* layouts we additionally let the declared
        // `cols`/`rows` extend the size past that extent: an
        // explicitly-positioned layout has a fixed rightmost edge, so
        // without this arm `refit_cols`/`refit_rows` would bump the
        // declared count but the size wouldn't actually grow and the
        // editor would visibly stop snapping past the last widget.
        //
        // Gating on `resizable` matters because `cols` is the widest
        // section's widget *count*, which overcounts columns whenever
        // widgets stack (explicit `.at` rows) or a spanning widget
        // wraps early - e.g. two knobs stacked in col 0 plus a meter
        // in col 1 is 3 widgets but only 2 columns. Applying the
        // `max` unconditionally padded every such fixed-size built-in
        // with a phantom trailing column.
        let max_widget_col = self
            .widgets
            .iter()
            .map(|w| w.col + w.col_span)
            .max()
            .unwrap_or(1);
        let max_widget_row = self
            .widgets
            .iter()
            .map(|w| w.row + w.row_span)
            .max()
            .unwrap_or(1);
        let (max_col, max_row) = if self.resizable {
            (max_widget_col.max(self.cols), max_widget_row.max(self.rows))
        } else {
            (max_widget_col, max_widget_row)
        };
        let section_count = self.sections.len() as f32;

        let w = GRID_PADDING * 2.0 + max_col as f32 * (self.cell_size + GRID_GAP) - GRID_GAP;
        let bottom_label_h = 22.0; // label + value text below the last row of widgets
        let h = self.header_height() + GRID_PADDING + max_row as f32 * (self.cell_size + GRID_GAP)
            - GRID_GAP
            + section_count * GRID_SECTION_H
            + bottom_label_h
            + GRID_PADDING;

        (w as u32, h as u32)
    }

    /// Auto-flow placement with section breaks. Internal helper:
    /// the public builder API exposes [`Self::with_cols`] /
    /// [`Self::with_cell_size`] / [`Self::with_grid`] which call
    /// `flow_and_size` after their field mutation. Previously
    /// exposed as `pub` (along with a `pub auto_flow()` wrapper for
    /// the no-breaks case), which mixed in-place mutation into the
    /// chainable `mut self -> Self` builder surface - confusing.
    /// Now `pub(crate)`; the no-breaks wrapper is gone since
    /// internal callers always pass an explicit slice.
    ///
    /// Each break is `(widget_index, label)`: when the cursor reaches that
    /// widget index, it advances to the next row and records a section label.
    pub(crate) fn auto_flow_with_breaks(&mut self, breaks: &[(usize, &'static str)]) {
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
            // Check for section breaks at this widget index. A break
            // advances past any previously-occupied row so a new
            // section starts strictly below the prior section's
            // tallest widget. Bumping by just 1 lets a section pack
            // alongside a tall widget from the prior section (the
            // 1x2 KTall + sliders case in the GUI zoo).
            for &(break_idx, label) in breaks {
                if break_idx == i {
                    if any_emitted || cursor_col > 0 {
                        let max_occupied_row = occupied.iter().map(|&(_, r)| r).max().unwrap_or(0);
                        cursor_row = (cursor_row + 1).max(max_occupied_row + 1);
                        cursor_col = 0;
                    }
                    self.sections.push((cursor_row, label));
                    any_emitted = true;
                }
            }

            if w.col != AUTO && w.row != AUTO {
                // Explicitly placed - already marked in first pass.
                any_emitted = true;
                continue;
            }

            // Find next free position that fits this widget.
            loop {
                if cursor_col + w.col_span > self.cols {
                    cursor_col = 0;
                    cursor_row += 1;
                }
                let fits = (0..w.col_span).all(|dc| {
                    (0..w.row_span)
                        .all(|dr| !occupied.contains(&(cursor_col + dc, cursor_row + dr)))
                });
                if fits {
                    break;
                }
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
#[must_use]
pub fn compute_section_offsets(layout: &GridLayout) -> Vec<f32> {
    // Hard cap so a degenerate widget row can't request a multi-gigabyte
    // allocation here and abort the process. Real plugins live well under
    // 64 rows.
    const MAX_ROW_CAP: u32 = 4096;
    let max_row = layout
        .widgets
        .iter()
        .map(|w| w.row.saturating_add(w.row_span))
        .max()
        .unwrap_or(1)
        .min(MAX_ROW_CAP);
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
        let cols = pl
            .rows
            .iter()
            .map(|r| r.knobs.iter().map(|k| k.span.max(1)).sum::<u32>())
            .max()
            .unwrap_or(1);

        let mut widgets = Vec::new();
        let mut sections = Vec::new();

        for (grid_row, row) in pl.rows.iter().enumerate() {
            let grid_row = len_u32(grid_row);
            if let Some(label) = row.label {
                sections.push((grid_row, label));
            }
            let mut col = 0u32;
            for knob in &row.knobs {
                widgets.push(GridWidget {
                    col,
                    row: grid_row,
                    col_span: knob.span.max(1),
                    row_span: 1,
                    param_id: knob.param_id,
                    label: knob.label,
                    widget: knob.widget,
                    param_id_y: knob.param_id_y,
                    meter_ids: knob.meter_ids.clone(),
                });
                col += knob.span.max(1);
            }
        }

        let mut gl = GridLayout {
            titles: pl.titles.clone(),
            cols,
            sections,
            widgets: widgets.clone(),
            cell_size: pl.knob_size,
            width: 0,
            height: 0,
            resizable: false,
            maximizable: false,
            min_size: (1, 1),
            max_size: (u32::MAX, u32::MAX),
            rows: 1,
            // PluginLayout drives placement from `rows` directly,
            // so widgets are already explicitly positioned. The
            // re-flow stash is the same widgets with no breaks -
            // calling `with_cols` would re-run auto-flow against
            // explicit (col,row) values, which is a no-op.
            original_widgets: widgets,
            original_breaks: Vec::new(),
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
    #[must_use]
    pub fn width(&self) -> u32 {
        match self {
            Layout::Rows(l) => l.width,
            Layout::Grid(g) => g.width,
        }
    }
    #[must_use]
    pub fn height(&self) -> u32 {
        match self {
            Layout::Rows(l) => l.height,
            Layout::Grid(g) => g.height,
        }
    }
    /// Title slot of the editor's header band - left, larger /
    /// brighter - or `None` when the layout doesn't set one.
    #[must_use]
    pub fn title(&self) -> Option<&str> {
        match self {
            Layout::Rows(l) => l.titles.title,
            Layout::Grid(g) => g.titles.title,
        }
    }
    /// Subtitle slot of the editor's header band - right, smaller /
    /// dimmer - or `None` when the layout doesn't set one.
    #[must_use]
    pub fn subtitle(&self) -> Option<&str> {
        match self {
            Layout::Rows(l) => l.titles.subtitle,
            Layout::Grid(g) => g.titles.subtitle,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn auto_widgets(n: u32) -> Vec<GridWidget> {
        (0..n).map(|i| GridWidget::knob(i, "k")).collect()
    }

    #[test]
    fn refit_cols_snaps_wider_to_more_cols() {
        let mut g = GridLayout::build(vec![widgets(auto_widgets(8))])
            .with_cols(2)
            .resizable(true)
            .max_size((8, u32::MAX));
        let natural_w = g.width;
        let (new_w, _) = g.refit_cols(natural_w * 2);
        assert_eq!(g.cols, 4);
        assert!(new_w > natural_w);
    }

    #[test]
    fn refit_cols_clamps_to_min_max() {
        let mut g = GridLayout::build(vec![widgets(auto_widgets(8))])
            .with_cols(4)
            .resizable(true)
            .min_size((2, 1))
            .max_size((4, u32::MAX));
        let _ = g.refit_cols(10);
        assert_eq!(g.cols, 2, "min_cols clamp");
        let _ = g.refit_cols(10_000);
        assert_eq!(g.cols, 4, "max_cols clamp");
    }

    #[test]
    fn refit_cols_reflows_widget_positions() {
        // 4 auto-positioned knobs in a 4-column grid sit in a
        // single row; clamping to 1 col packs them into 4 rows.
        let mut g = GridLayout::build(vec![widgets(auto_widgets(4))])
            .with_cols(4)
            .resizable(true)
            .max_size((1, u32::MAX));
        let _ = g.refit_cols(40);
        let mut rows: Vec<u32> = g.widgets.iter().map(|w| w.row).collect();
        rows.sort_unstable();
        assert_eq!(rows, vec![0, 1, 2, 3]);
        assert!(g.widgets.iter().all(|w| w.col == 0));
    }

    #[test]
    fn refit_rows_snaps_taller_to_more_rows() {
        let mut g = GridLayout::build(vec![widgets(auto_widgets(4))])
            .with_cols(4)
            .resizable(true)
            .max_size((u32::MAX, 8));
        let natural_h = g.height;
        let (_, new_h) = g.refit_rows(natural_h * 2);
        assert!(
            g.rows > 1,
            "rows should grow under doubled height; got {}",
            g.rows
        );
        assert!(new_h > natural_h);
    }

    #[test]
    #[allow(
        clippy::cast_precision_loss,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss
    )]
    fn fixed_size_natural_width_matches_flowed_content() {
        // block-drywet shape: 3 widgets but only 2 columns occupied
        // (two knobs stacked in col 0, a meter spanning 2 rows in
        // col 1). The widest-section widget *count* is 3, so the old
        // unconditional `max(self.cols)` padded the window with a
        // phantom trailing column. A fixed-size layout must be
        // exactly as wide as its flowed content.
        let g = GridLayout::build(vec![widgets(vec![
            GridWidget::knob(0u32, "Drive").at(0, 0),
            GridWidget::knob(1u32, "Mix").at(0, 1),
            GridWidget::meter(&[2u32, 3u32], "Level").at(1, 0).rows(2),
        ])])
        .with_title("DRY/WET");
        let max_widget_col = g.widgets.iter().map(|w| w.col + w.col_span).max().unwrap();
        let content_w = (GRID_PADDING * 2.0 + max_widget_col as f32 * (g.cell_size + GRID_GAP)
            - GRID_GAP) as u32;
        assert_eq!(
            g.width, content_w,
            "fixed-size natural width must hug the flowed content, no phantom columns"
        );
    }

    #[test]
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    fn resizable_natural_width_still_extends_to_declared_cols() {
        // The resize affordance is preserved: an opted-in resizable
        // layout still lets `cols` extend the width past the flowed
        // extent so `refit_cols` has room to snap into.
        let g = GridLayout::build(vec![widgets(auto_widgets(2))])
            .with_cols(4)
            .resizable(true);
        assert_eq!(g.cols, 4);
        let two_col_w = (GRID_PADDING * 2.0 + 2.0 * (g.cell_size + GRID_GAP) - GRID_GAP) as u32;
        assert!(
            g.width > two_col_w,
            "resizable layout keeps declared-cols width extension"
        );
    }

    #[test]
    fn set_size_feedback_loop_is_stable() {
        // Reproduce the Windows resize path: set_size runs
        // refit_cols THEN refit_rows, then the editor resizes the
        // window to the result, which re-enters set_size with that
        // size. A stable loop must converge (no per-resize drift /
        // clip). titled => header band is in play.
        let mut g = GridLayout::build(vec![widgets(auto_widgets(4))])
            .with_cols(4)
            .with_title("T")
            .resizable(true)
            .max_size((8, 8));

        // simulate one user drag to an arbitrary larger size
        g.refit_cols(600);
        let (mut w, mut h) = g.refit_rows(400);

        // now feed the editor's own size back in, repeatedly, the way
        // window.resize -> Resized -> set_size does on Windows.
        for i in 0..5 {
            g.refit_cols(w);
            let (w2, h2) = g.refit_rows(h);
            assert_eq!(
                (w2, h2),
                (w, h),
                "feedback iteration {i} drifted: {:?} -> {:?}",
                (w, h),
                (w2, h2)
            );
            w = w2;
            h = h2;
        }
    }

    #[test]
    fn refit_rows_round_trips_through_the_header() {
        // Resizing a titled layout to its own natural height must be
        // a no-op: `refit_rows` has to subtract the same header band
        // `compute_size` adds.
        let mut g = GridLayout::build(vec![widgets(auto_widgets(4))])
            .with_cols(4)
            .with_title("T")
            .resizable(true)
            .max_size((u32::MAX, 8));
        let nat_h = g.height;
        let (_, snapped) = g.refit_rows(nat_h);
        assert_eq!(snapped, nat_h, "resize to natural height must round-trip");
    }

    #[test]
    fn refit_rows_clamps_to_min_max() {
        let mut g = GridLayout::build(vec![widgets(auto_widgets(4))])
            .with_cols(4)
            .resizable(true)
            .min_size((1, 2))
            .max_size((u32::MAX, 4));
        let _ = g.refit_rows(10);
        assert_eq!(g.rows, 2, "min_rows clamp");
        let _ = g.refit_rows(10_000);
        assert_eq!(g.rows, 4, "max_rows clamp");
    }
}
