/// Declarative layout DSL for plugin GUIs.
///
/// # Example
/// ```ignore
/// use truce_gui_types::layout;
///
/// fn gui_layout() -> truce_gui_types::layout::PluginLayout {
///     layout!("MY PLUGIN", "V1.0", 50.0, {
///         row {
///             knob(ID_GAIN, "Gain")
///             slider(ID_PAN, "Pan")
///             toggle(ID_BYPASS, "Bypass")
///             meter(&[METER_L, METER_R], "Level")
///         }
///         section("FILTER") {
///             knob(ID_CUTOFF, "Cutoff")
///             knob(ID_RESO, "Reso")
///         }
///     })
/// }
/// ```
#[macro_export]
macro_rules! layout {
    ($title:expr, $subtitle:expr, $knob_size:expr, { $($body:tt)* }) => {{
        let rows = $crate::__layout_rows!( [] $($body)* );
        $crate::layout::PluginLayout::build(
            $crate::layout::HeaderTitles::pair($title, $subtitle),
            rows,
            $knob_size,
        )
    }};
}

#[macro_export]
#[doc(hidden)]
macro_rules! __layout_rows {
    ( [ $($rows:expr),* ] ) => {
        vec![ $($rows),* ]
    };

    ( [ $($rows:expr),* ] row { $($widgets:tt)* } $($rest:tt)* ) => {
        $crate::__layout_rows!(
            [ $($rows,)* $crate::layout::KnobRow {
                label: None,
                knobs: $crate::__layout_widgets!( [] $($widgets)* ),
            } ]
            $($rest)*
        )
    };

    ( [ $($rows:expr),* ] section($label:expr) { $($widgets:tt)* } $($rest:tt)* ) => {
        $crate::__layout_rows!(
            [ $($rows,)* $crate::layout::KnobRow {
                label: Some($label),
                knobs: $crate::__layout_widgets!( [] $($widgets)* ),
            } ]
            $($rest)*
        )
    };
}

#[macro_export]
#[doc(hidden)]
macro_rules! __layout_widgets {
    // Done
    ( [ $($w:expr),* ] ) => {
        vec![ $($w),* ]
    };

    // --- .span(N) variants MUST come before plain variants ---

    ( [ $($w:expr),* ] knob($id:expr, $label:expr) .span($n:expr) $($rest:tt)* ) => {
        $crate::__layout_widgets!( [ $($w,)* $crate::layout::KnobDef::knob($id, $label).with_span($n) ] $($rest)* )
    };
    ( [ $($w:expr),* ] slider($id:expr, $label:expr) .span($n:expr) $($rest:tt)* ) => {
        $crate::__layout_widgets!( [ $($w,)* $crate::layout::KnobDef::slider($id, $label).with_span($n) ] $($rest)* )
    };
    ( [ $($w:expr),* ] toggle($id:expr, $label:expr) .span($n:expr) $($rest:tt)* ) => {
        $crate::__layout_widgets!( [ $($w,)* $crate::layout::KnobDef::toggle($id, $label).with_span($n) ] $($rest)* )
    };
    ( [ $($w:expr),* ] selector($id:expr, $label:expr) .span($n:expr) $($rest:tt)* ) => {
        $crate::__layout_widgets!( [ $($w,)* $crate::layout::KnobDef::selector($id, $label).with_span($n) ] $($rest)* )
    };
    ( [ $($w:expr),* ] meter($ids:expr, $label:expr) .span($n:expr) $($rest:tt)* ) => {
        $crate::__layout_widgets!( [ $($w,)* $crate::layout::KnobDef::meter($ids, $label).with_span($n) ] $($rest)* )
    };
    ( [ $($w:expr),* ] xy_pad($x:expr, $y:expr, $label:expr) .span($n:expr) $($rest:tt)* ) => {
        $crate::__layout_widgets!( [ $($w,)* $crate::layout::KnobDef::xy_pad($x, $y, $label).with_span($n) ] $($rest)* )
    };

    // --- Plain variants (no .span) ---

    ( [ $($w:expr),* ] knob($id:expr, $label:expr) $($rest:tt)* ) => {
        $crate::__layout_widgets!( [ $($w,)* $crate::layout::KnobDef::knob($id, $label) ] $($rest)* )
    };
    ( [ $($w:expr),* ] slider($id:expr, $label:expr) $($rest:tt)* ) => {
        $crate::__layout_widgets!( [ $($w,)* $crate::layout::KnobDef::slider($id, $label) ] $($rest)* )
    };
    ( [ $($w:expr),* ] toggle($id:expr, $label:expr) $($rest:tt)* ) => {
        $crate::__layout_widgets!( [ $($w,)* $crate::layout::KnobDef::toggle($id, $label) ] $($rest)* )
    };
    ( [ $($w:expr),* ] selector($id:expr, $label:expr) $($rest:tt)* ) => {
        $crate::__layout_widgets!( [ $($w,)* $crate::layout::KnobDef::selector($id, $label) ] $($rest)* )
    };
    ( [ $($w:expr),* ] meter($ids:expr, $label:expr) $($rest:tt)* ) => {
        $crate::__layout_widgets!( [ $($w,)* $crate::layout::KnobDef::meter($ids, $label) ] $($rest)* )
    };
    ( [ $($w:expr),* ] xy_pad($x:expr, $y:expr, $label:expr) $($rest:tt)* ) => {
        $crate::__layout_widgets!( [ $($w,)* $crate::layout::KnobDef::xy_pad($x, $y, $label) ] $($rest)* )
    };
}

// ---------------------------------------------------------------------------
// Grid layout DSL
// ---------------------------------------------------------------------------

/// Declarative grid layout DSL for plugin GUIs.
///
/// Defaults: no header, `cols` = max widgets per section,
/// `cell_size` = `GRID_DEFAULT_CELL_SIZE`. Override any of those
/// via the `cols:` / `cell:` keyword args, or set the header band
/// with `title: "..."` and / or `subtitle: "..."` (each is
/// independently optional).
///
/// # Example
/// ```ignore
/// use truce_gui_types::grid;
///
/// // Minimal — auto-cols, default cell size, no header.
/// fn gui_layout() -> truce_gui_types::layout::GridLayout {
///     grid!({
///         knob(ID_GAIN, "Gain")
///         slider(ID_PAN, "Pan")
///     })
/// }
///
/// // Force wrapping: 4 widgets on a 2-col grid.
/// fn wrapped() -> truce_gui_types::layout::GridLayout {
///     grid!(cols: 2, {
///         knob(ID_GAIN, "Gain")
///         slider(ID_PAN, "Pan")
///         toggle(ID_BYPASS, "Bypass")
///         meter(&[METER_L, METER_R], "Level")
///     })
/// }
///
/// // Header — title + subtitle, or either one alone.
/// fn with_header() -> truce_gui_types::layout::GridLayout {
///     grid!(title: "MY PLUGIN", subtitle: "V1.0", cols: 4, cell: 50.0, {
///         knob(ID_GAIN, "Gain")
///     })
/// }
///
/// fn title_only() -> truce_gui_types::layout::GridLayout {
///     grid!(title: "MY PLUGIN", { knob(ID_GAIN, "Gain") })
/// }
/// ```
#[macro_export]
macro_rules! grid {
    // Minimal: just the body.
    ({ $($body:tt)* }) => {{
        $crate::layout::GridLayout::build($crate::__grid_sections!($($body)*))
    }};
    // cols only.
    (cols: $cols:expr, { $($body:tt)* }) => {{
        $crate::layout::GridLayout::build($crate::__grid_sections!($($body)*))
            .with_cols($cols)
    }};
    // cell only.
    (cell: $cell:expr, { $($body:tt)* }) => {{
        $crate::layout::GridLayout::build($crate::__grid_sections!($($body)*))
            .with_cell_size($cell)
    }};
    // cols + cell.
    (cols: $cols:expr, cell: $cell:expr, { $($body:tt)* }) => {{
        $crate::layout::GridLayout::build($crate::__grid_sections!($($body)*))
            .with_grid($cols, $cell)
    }};

    // --- Header arms — title and subtitle both optional ---

    // title + subtitle + body.
    (title: $title:expr, subtitle: $subtitle:expr, { $($body:tt)* }) => {{
        $crate::layout::GridLayout::build($crate::__grid_sections!($($body)*))
            .with_titles($crate::layout::HeaderTitles::pair($title, $subtitle))
    }};
    // title + subtitle + cols.
    (title: $title:expr, subtitle: $subtitle:expr, cols: $cols:expr, { $($body:tt)* }) => {{
        $crate::layout::GridLayout::build($crate::__grid_sections!($($body)*))
            .with_cols($cols)
            .with_titles($crate::layout::HeaderTitles::pair($title, $subtitle))
    }};
    // title + subtitle + cell.
    (title: $title:expr, subtitle: $subtitle:expr, cell: $cell:expr, { $($body:tt)* }) => {{
        $crate::layout::GridLayout::build($crate::__grid_sections!($($body)*))
            .with_cell_size($cell)
            .with_titles($crate::layout::HeaderTitles::pair($title, $subtitle))
    }};
    // title + subtitle + cols + cell — full form.
    (title: $title:expr, subtitle: $subtitle:expr, cols: $cols:expr, cell: $cell:expr, { $($body:tt)* }) => {{
        $crate::layout::GridLayout::build($crate::__grid_sections!($($body)*))
            .with_grid($cols, $cell)
            .with_titles($crate::layout::HeaderTitles::pair($title, $subtitle))
    }};

    // title only — any combination of cols / cell, body required.
    (title: $title:expr, { $($body:tt)* }) => {{
        $crate::layout::GridLayout::build($crate::__grid_sections!($($body)*))
            .with_title($title)
    }};
    (title: $title:expr, cols: $cols:expr, { $($body:tt)* }) => {{
        $crate::layout::GridLayout::build($crate::__grid_sections!($($body)*))
            .with_cols($cols)
            .with_title($title)
    }};
    (title: $title:expr, cell: $cell:expr, { $($body:tt)* }) => {{
        $crate::layout::GridLayout::build($crate::__grid_sections!($($body)*))
            .with_cell_size($cell)
            .with_title($title)
    }};
    (title: $title:expr, cols: $cols:expr, cell: $cell:expr, { $($body:tt)* }) => {{
        $crate::layout::GridLayout::build($crate::__grid_sections!($($body)*))
            .with_grid($cols, $cell)
            .with_title($title)
    }};

    // subtitle only — same shape.
    (subtitle: $subtitle:expr, { $($body:tt)* }) => {{
        $crate::layout::GridLayout::build($crate::__grid_sections!($($body)*))
            .with_subtitle($subtitle)
    }};
    (subtitle: $subtitle:expr, cols: $cols:expr, { $($body:tt)* }) => {{
        $crate::layout::GridLayout::build($crate::__grid_sections!($($body)*))
            .with_cols($cols)
            .with_subtitle($subtitle)
    }};
    (subtitle: $subtitle:expr, cell: $cell:expr, { $($body:tt)* }) => {{
        $crate::layout::GridLayout::build($crate::__grid_sections!($($body)*))
            .with_cell_size($cell)
            .with_subtitle($subtitle)
    }};
    (subtitle: $subtitle:expr, cols: $cols:expr, cell: $cell:expr, { $($body:tt)* }) => {{
        $crate::layout::GridLayout::build($crate::__grid_sections!($($body)*))
            .with_grid($cols, $cell)
            .with_subtitle($subtitle)
    }};
}

#[macro_export]
#[doc(hidden)]
macro_rules! __grid_sections {
    ($($body:tt)*) => {{
        let mut _widgets: Vec<$crate::layout::GridWidget> = Vec::new();
        let mut _breaks: Vec<(usize, &'static str)> = Vec::new();
        $crate::__grid_items!(_widgets, _breaks, $($body)*);
        // Convert flat widgets + breaks into Section vec for build()
        let mut _sections: Vec<$crate::layout::Section> = Vec::new();
        let mut _cur_widgets: Vec<$crate::layout::GridWidget> = Vec::new();
        let mut _cur_label: Option<&'static str> = None;
        for (i, w) in _widgets.into_iter().enumerate() {
            if let Some(&(_, label)) = _breaks.iter().find(|(idx, _)| *idx == i) {
                if !_cur_widgets.is_empty() || _cur_label.is_some() {
                    _sections.push($crate::layout::Section {
                        label: _cur_label,
                        widgets: std::mem::take(&mut _cur_widgets),
                    });
                }
                _cur_label = Some(label);
            }
            _cur_widgets.push(w);
        }
        if !_cur_widgets.is_empty() || _cur_label.is_some() {
            _sections.push($crate::layout::Section {
                label: _cur_label,
                widgets: _cur_widgets,
            });
        }
        _sections
    }};
}

#[macro_export]
#[doc(hidden)]
macro_rules! __grid_items {
    // Base cases
    ($w:ident, $b:ident) => {};
    ($w:ident, $b:ident,) => {};

    // Section break
    ($w:ident, $b:ident, section($label:expr) $($rest:tt)*) => {
        $b.push(($w.len(), $label));
        $crate::__grid_items!($w, $b, $($rest)*);
    };

    // Widget types — dispatch to modifier parser
    ($w:ident, $b:ident, knob($id:expr, $label:expr) $($rest:tt)*) => {
        $crate::__grid_mods!($w, $b, $crate::layout::GridWidget::knob($id, $label), $($rest)*);
    };
    ($w:ident, $b:ident, slider($id:expr, $label:expr) $($rest:tt)*) => {
        $crate::__grid_mods!($w, $b, $crate::layout::GridWidget::slider($id, $label), $($rest)*);
    };
    ($w:ident, $b:ident, toggle($id:expr, $label:expr) $($rest:tt)*) => {
        $crate::__grid_mods!($w, $b, $crate::layout::GridWidget::toggle($id, $label), $($rest)*);
    };
    ($w:ident, $b:ident, selector($id:expr, $label:expr) $($rest:tt)*) => {
        $crate::__grid_mods!($w, $b, $crate::layout::GridWidget::selector($id, $label), $($rest)*);
    };
    ($w:ident, $b:ident, meter($ids:expr, $label:expr) $($rest:tt)*) => {
        $crate::__grid_mods!($w, $b, $crate::layout::GridWidget::meter($ids, $label), $($rest)*);
    };
    ($w:ident, $b:ident, xy_pad($x:expr, $y:expr, $label:expr) $($rest:tt)*) => {
        $crate::__grid_mods!($w, $b, $crate::layout::GridWidget::xy_pad($x, $y, $label), $($rest)*);
    };
}

#[macro_export]
#[doc(hidden)]
macro_rules! __grid_mods {
    // .cols(N).rows(M)
    ($w:ident, $b:ident, $widget:expr, .cols($c:expr) .rows($r:expr) $($rest:tt)*) => {
        $w.push($widget.cols($c).rows($r));
        $crate::__grid_items!($w, $b, $($rest)*);
    };
    // .rows(M).cols(N)
    ($w:ident, $b:ident, $widget:expr, .rows($r:expr) .cols($c:expr) $($rest:tt)*) => {
        $w.push($widget.cols($c).rows($r));
        $crate::__grid_items!($w, $b, $($rest)*);
    };
    // .cols(N) only
    ($w:ident, $b:ident, $widget:expr, .cols($c:expr) $($rest:tt)*) => {
        $w.push($widget.cols($c));
        $crate::__grid_items!($w, $b, $($rest)*);
    };
    // .rows(M) only
    ($w:ident, $b:ident, $widget:expr, .rows($r:expr) $($rest:tt)*) => {
        $w.push($widget.rows($r));
        $crate::__grid_items!($w, $b, $($rest)*);
    };
    // No modifiers
    ($w:ident, $b:ident, $widget:expr, $($rest:tt)*) => {
        $w.push($widget);
        $crate::__grid_items!($w, $b, $($rest)*);
    };
}
