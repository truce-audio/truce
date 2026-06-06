//! Automatic iced UI generation from a `GridLayout`.
//!
//! Maps each `GridWidget` to the appropriate iced widget, arranging them
//! in a grid-like layout using iced's `Column`/`Row`/`Container`.

use std::fmt::Debug;

use iced::widget::{Column, Row, container, row, text};
use iced::{Element, Length, alignment};

use truce_gui::layout::{GridLayout, WidgetKind};
use truce_params::Params;

use crate::param_cache::ParamCache;
use crate::param_message::Message;
use crate::theme;
use crate::widgets;

/// Generate an iced `Element` from a `GridLayout`.
///
/// This is the zero-custom-code path: the plugin defines a layout and
/// truce-iced generates the full UI automatically.
//
// Grid spans widen `u32 as f32`; cell counts fit in editor pixel
// dimensions, well below 2^23.
#[allow(clippy::cast_precision_loss)]
#[must_use]
pub fn auto_view<'a, M: Clone + Debug + 'static, P: Params>(
    layout: &GridLayout,
    params: &'a ParamCache<P>,
) -> Element<'a, Message<M>> {
    let max_row = layout
        .widgets
        .iter()
        .map(|w| w.row + w.row_span)
        .max()
        .unwrap_or(0);

    let mut main_col: Column<'a, Message<M>> = Column::new().spacing(8).padding(15);

    // Header band - drawn whenever either title slot is set.
    if !layout.titles.is_empty() {
        let mut header_row = row![].spacing(8).align_y(alignment::Vertical::Center);
        if let Some(t) = layout.titles.title {
            header_row = header_row.push(text(t).size(16));
        }
        if let Some(s) = layout.titles.subtitle {
            header_row = header_row.push(text(s).size(10).color(theme::TEXT_DIM));
        }
        let header = container(header_row)
            .padding(8)
            .style(|_theme: &iced::Theme| container::Style {
                background: Some(theme::HEADER_BG.into()),
                ..Default::default()
            })
            .width(Length::Fill);
        main_col = main_col.push(header);
    }

    // Build rows
    for r in 0..max_row {
        // Section label
        if let Some((_, label)) = layout.sections.iter().find(|(row, _)| *row == r) {
            main_col = main_col.push(
                text(*label)
                    .size(11)
                    .color(theme::TEXT_DIM)
                    .width(Length::Fill)
                    .align_x(alignment::Horizontal::Center),
            );
        }

        // Collect widgets in this row, sorted by column
        let mut row_widgets: Vec<_> = layout.widgets.iter().filter(|w| w.row == r).collect();
        row_widgets.sort_by_key(|w| w.col);

        if row_widgets.is_empty() {
            continue;
        }

        let mut row_elem: Row<'a, Message<M>> =
            Row::new().spacing(10).align_y(alignment::Vertical::Top);

        for widget in &row_widgets {
            let kind = widget.widget.unwrap_or(WidgetKind::Knob);
            let elem: Element<'a, Message<M>> = match kind {
                WidgetKind::Knob => widgets::knob(widget.param_id, params)
                    .label(widget.label)
                    .size(layout.cell_size)
                    .into(),
                WidgetKind::Slider => widgets::param_slider(widget.param_id, params)
                    .label(widget.label)
                    .width(layout.cell_size)
                    .into(),
                WidgetKind::Toggle => widgets::param_toggle(widget.param_id, params)
                    .label(widget.label)
                    .into(),
                // Iced has no native cycle-on-click control distinct from
                // a pick_list, so `Dropdown` and the deprecated
                // `Selector` collapse to the same iced widget. The
                // variants stay separate in
                // `truce_gui::layout::WidgetKind` because the built-in /
                // egui backends do distinguish them.
                WidgetKind::Selector | WidgetKind::Dropdown => {
                    widgets::param_dropdown(widget.param_id, params)
                        .label(widget.label)
                        .into()
                }
                WidgetKind::Meter => {
                    let fallback = [widget.param_id];
                    let ids = widget.meter_ids.as_deref().unwrap_or(&fallback);
                    widgets::meter(ids, params)
                        .label(widget.label)
                        .size(
                            layout.cell_size * widget.col_span as f32,
                            layout.cell_size * widget.row_span as f32,
                        )
                        .into()
                }
                WidgetKind::XYPad => {
                    let y_id = widget.param_id_y.unwrap_or(widget.param_id);
                    widgets::xy_pad(widget.param_id, y_id, params)
                        .label(widget.label)
                        .size(layout.cell_size * widget.col_span as f32)
                        .into()
                }
            };

            row_elem = row_elem.push(elem);
        }

        main_col = main_col.push(row_elem);
    }

    main_col.into()
}
