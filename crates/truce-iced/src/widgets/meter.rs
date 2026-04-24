//! Level meter widget rendered via iced Canvas (display-only).

use std::fmt::Debug;
use std::marker::PhantomData;

use iced::widget::canvas::{self, Frame, Geometry, Path};
use iced::widget::Canvas;
use iced::{mouse, Element, Length, Point, Rectangle, Renderer, Size, Theme};

use truce_core::meter_display;
use truce_params::Params;

use crate::param_message::Message;
use crate::param_state::ParamState;
use crate::theme;

/// Builder for a multi-channel level meter.
pub struct MeterWidget<'a, M> {
    _ids: Vec<u32>,
    values: Vec<f32>,
    label: Option<&'a str>,
    width: f32,
    height: f32,
    font: iced::Font,
    _phantom: PhantomData<M>,
}

impl<'a, M: Clone + Debug + 'static> MeterWidget<'a, M> {
    pub fn new(ids: &[u32], params: &'a ParamState<impl Params>) -> Self {
        let values: Vec<f32> = ids.iter().map(|&id| params.meter(id)).collect();
        Self {
            _ids: ids.to_vec(),
            values,
            label: None,
            width: 16.0,
            height: 80.0,
            font: params.font(),
            _phantom: PhantomData,
        }
    }

    pub fn label(mut self, label: &'a str) -> Self {
        self.label = Some(label);
        self
    }

    pub fn size(mut self, width: f32, height: f32) -> Self {
        self.width = width;
        self.height = height;
        self
    }

    pub fn font(mut self, font: iced::Font) -> Self {
        self.font = font;
        self
    }

    pub fn into_element(self) -> Element<'a, Message<M>> {
        let total_h = self.height;
        // `label` and `font` are accepted on the builder for API symmetry
        // with knob/dropdown but the meter currently renders bars only —
        // text labels are drawn by the surrounding layout, not the canvas.
        let _ = (self.label, self.font);
        let program = MeterProgram {
            values: self.values,
            meter_height: self.height,
        };

        Canvas::new(program)
            .width(Length::Fixed(self.width))
            .height(Length::Fixed(total_h))
            .into()
    }
}

impl<'a, M: Clone + Debug + 'static> From<MeterWidget<'a, M>> for Element<'a, Message<M>> {
    fn from(m: MeterWidget<'a, M>) -> Self {
        m.into_element()
    }
}

// ---------------------------------------------------------------------------
// Canvas program
// ---------------------------------------------------------------------------

struct MeterProgram {
    values: Vec<f32>,
    meter_height: f32,
}

impl<M: Clone + Debug + 'static> canvas::Program<Message<M>> for MeterProgram {
    type State = ();

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

        for (i, &value) in self.values.iter().enumerate() {
            let x = i as f32 * (bar_w + bar_gap);
            let display = meter_display(value);
            let fill_h = (display * self.meter_height).clamp(0.0, self.meter_height);

            // Background
            let bg = Path::rectangle(Point::new(x, 0.0), Size::new(bar_w, self.meter_height));
            frame.fill(&bg, iced::Color::from_rgb(0.165, 0.165, 0.188));

            // Fill (blue, red when clipping)
            if fill_h > 0.0 {
                let color = if display > 0.95 {
                    theme::METER_CLIP
                } else {
                    theme::KNOB_FILL
                };
                let bar = Path::rectangle(
                    Point::new(x, self.meter_height - fill_h),
                    Size::new(bar_w, fill_h),
                );
                frame.fill(&bar, color);
            }
        }

        vec![frame.into_geometry()]
    }

    fn update(
        &self,
        _state: &mut Self::State,
        _event: canvas::Event,
        _bounds: Rectangle,
        _cursor: mouse::Cursor,
    ) -> (canvas::event::Status, Option<Message<M>>) {
        // Display-only, no interaction
        (canvas::event::Status::Ignored, None)
    }
}
