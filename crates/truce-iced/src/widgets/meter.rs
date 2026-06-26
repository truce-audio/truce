//! Level meter widget rendered via iced Canvas (display-only).

use std::fmt::Debug;
use std::marker::PhantomData;

use crate::iced::widget::Canvas;
use crate::iced::widget::canvas::{self, Frame, Geometry, Path};
use crate::iced::{Element, Length, Point, Rectangle, Renderer, Size, Theme, mouse};

use truce_core::meter_display;
use truce_params::Params;

use crate::param_cache::ParamCache;
use crate::param_message::Message;
use crate::theme;

/// Builder for a multi-channel level meter.
pub struct MeterWidget<'a, M> {
    values: Vec<f32>,
    label: Option<&'a str>,
    width: Length,
    height: Length,
    font: crate::iced::Font,
    _phantom: PhantomData<M>,
}

impl<'a, M: Clone + Debug + 'static> MeterWidget<'a, M> {
    #[must_use]
    pub fn new(ids: &[u32], params: &'a ParamCache<impl Params>) -> Self {
        let values: Vec<f32> = ids.iter().map(|&id| params.meter(id)).collect();
        Self {
            values,
            label: None,
            width: Length::Fixed(16.0),
            height: Length::Fixed(80.0),
            font: params.font(),
            _phantom: PhantomData,
        }
    }

    #[must_use]
    pub fn label(mut self, label: &'a str) -> Self {
        self.label = Some(label);
        self
    }

    /// Set explicit width and height in logical pixels. Use
    /// [`Self::width`] / [`Self::height`] / [`Self::fill`] when the
    /// meter should stretch with the surrounding layout.
    #[must_use]
    pub fn size(mut self, width: f32, height: f32) -> Self {
        self.width = Length::Fixed(width);
        self.height = Length::Fixed(height);
        self
    }

    /// Replace the meter's width with an iced [`Length`] (e.g.
    /// `Length::Fixed(16.0)` for a narrow band, `Length::Fill` to
    /// claim the row's remaining width).
    #[must_use]
    pub fn width(mut self, width: Length) -> Self {
        self.width = width;
        self
    }

    /// Replace the meter's height with an iced [`Length`].
    /// `Length::Fill` lets the meter stretch vertically with the
    /// editor window.
    #[must_use]
    pub fn height(mut self, height: Length) -> Self {
        self.height = height;
        self
    }

    /// Shortcut for `.height(Length::Fill)` so the meter
    /// stretches vertically with its parent layout.
    #[must_use]
    pub fn fill(mut self) -> Self {
        self.height = Length::Fill;
        self
    }

    #[must_use]
    pub fn font(mut self, font: crate::iced::Font) -> Self {
        self.font = font;
        self
    }

    #[must_use]
    pub fn into_element(self) -> Element<'a, Message<M>> {
        // `label` and `font` are accepted on the builder for API symmetry
        // with knob/dropdown but the meter currently renders bars only -
        // text labels are drawn by the surrounding layout, not the canvas.
        let _ = (self.label, self.font);
        let program = MeterProgram {
            values: self.values,
        };

        Canvas::new(program)
            .width(self.width)
            .height(self.height)
            .into()
    }
}

impl<'a, M: Clone + Debug + 'static> From<MeterWidget<'a, M>> for Element<'a, Message<M>> {
    fn from(m: MeterWidget<'a, M>) -> Self {
        m.into_element()
    }
}

// Canvas program

struct MeterProgram {
    values: Vec<f32>,
}

impl<M: Clone + Debug + 'static> canvas::Program<Message<M>> for MeterProgram {
    type State = ();

    // `usize as f32` for channel-count layout; channel counts are
    // tiny (typically <= 64).
    #[allow(clippy::cast_precision_loss)]
    fn draw(
        &self,
        _state: &Self::State,
        renderer: &Renderer,
        _theme: &Theme,
        bounds: Rectangle,
        _cursor: mouse::Cursor,
    ) -> Vec<Geometry> {
        let mut frame = Frame::new(renderer, bounds.size());
        let channels = self.values.len().max(1);
        let bar_gap = 2.0;
        let total_gap = bar_gap * (channels as f32 - 1.0).max(0.0);
        let bar_w = ((bounds.width - total_gap) / channels as f32).max(4.0);

        // Drive the bar height off `bounds.height` (not a cached
        // builder value) so `Length::Fill` makes the meter
        // stretch vertically with its parent layout.
        let meter_h = bounds.height;
        for (i, &value) in self.values.iter().enumerate() {
            let x = i as f32 * (bar_w + bar_gap);
            let display = meter_display(value);
            let fill_h = (display * meter_h).clamp(0.0, meter_h);

            // Background
            let bg = Path::rectangle(Point::new(x, 0.0), Size::new(bar_w, meter_h));
            frame.fill(&bg, crate::iced::Color::from_rgb(0.165, 0.165, 0.188));

            // Fill (blue, red when clipping)
            if fill_h > 0.0 {
                let color = if display > 0.95 {
                    theme::METER_CLIP
                } else {
                    theme::KNOB_FILL
                };
                let bar =
                    Path::rectangle(Point::new(x, meter_h - fill_h), Size::new(bar_w, fill_h));
                frame.fill(&bar, color);
            }
        }

        vec![frame.into_geometry()]
    }

    fn update(
        &self,
        _state: &mut Self::State,
        _event: &canvas::Event,
        _bounds: Rectangle,
        _cursor: mouse::Cursor,
    ) -> Option<canvas::Action<Message<M>>> {
        // Display-only, no interaction
        None
    }
}
