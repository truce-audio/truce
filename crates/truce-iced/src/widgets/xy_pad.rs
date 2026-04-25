//! XY pad widget for controlling two parameters simultaneously.

use std::fmt::Debug;
use std::marker::PhantomData;

use iced::widget::canvas::{self, Event, Frame, Geometry, Path, Stroke};
use iced::widget::Canvas;
use iced::{mouse, Color, Element, Length, Point, Rectangle, Renderer, Size, Theme};

use crate::param_message::{Message, ParamMessage};
use crate::param_state::ParamState;
use crate::theme;
use truce_params::Params;

/// Builder for an XY pad controlling two parameters.
pub struct XYPadWidget<'a, M> {
    x_id: u32,
    y_id: u32,
    x_value: f64,
    y_value: f64,
    label: Option<&'a str>,
    size: f32,
    font: iced::Font,
    _phantom: PhantomData<M>,
}

impl<'a, M: Clone + Debug + 'static> XYPadWidget<'a, M> {
    pub fn new(
        x_id: impl Into<u32>,
        y_id: impl Into<u32>,
        params: &'a ParamState<impl Params>,
    ) -> Self {
        let x_id = x_id.into();
        let y_id = y_id.into();
        Self {
            x_id,
            y_id,
            x_value: params.get(x_id),
            y_value: params.get(y_id),
            label: None,
            size: 120.0,
            font: params.font(),
            _phantom: PhantomData,
        }
    }

    pub fn label(mut self, label: &'a str) -> Self {
        self.label = Some(label);
        self
    }

    pub fn size(mut self, size: f32) -> Self {
        self.size = size;
        self
    }

    pub fn font(mut self, font: iced::Font) -> Self {
        self.font = font;
        self
    }

    pub fn into_element(self) -> Element<'a, Message<M>> {
        let total_h = self.size + if self.label.is_some() { 16.0 } else { 0.0 };
        let program = XYPadProgram {
            x_id: self.x_id,
            y_id: self.y_id,
            x_value: self.x_value as f32,
            y_value: self.y_value as f32,
            label: self.label.unwrap_or("").to_string(),
            pad_size: self.size,
            font: self.font,
        };

        Canvas::new(program)
            .width(Length::Fixed(self.size))
            .height(Length::Fixed(total_h))
            .into()
    }
}

impl<'a, M: Clone + Debug + 'static> From<XYPadWidget<'a, M>> for Element<'a, Message<M>> {
    fn from(xy: XYPadWidget<'a, M>) -> Self {
        xy.into_element()
    }
}

// ---------------------------------------------------------------------------
// Canvas program
// ---------------------------------------------------------------------------

struct XYPadProgram {
    x_id: u32,
    y_id: u32,
    x_value: f32,
    y_value: f32,
    label: String,
    pad_size: f32,
    font: iced::Font,
}

#[derive(Default)]
struct XYPadState {
    dragging: bool,
}

impl<M: Clone + Debug + 'static> canvas::Program<Message<M>> for XYPadProgram {
    type State = XYPadState;

    fn draw(
        &self,
        _state: &Self::State,
        renderer: &Renderer,
        _theme: &Theme,
        bounds: Rectangle,
        _cursor: mouse::Cursor,
    ) -> Vec<Geometry> {
        let mut frame = Frame::new(renderer, bounds.size());
        let s = self.pad_size;

        // Background
        let bg = Path::rectangle(Point::ORIGIN, Size::new(s, s));
        frame.fill(&bg, theme::SURFACE);

        // Border
        frame.stroke(
            &bg,
            Stroke::default().with_color(theme::ACCENT).with_width(1.0),
        );

        // Crosshair position (Y inverted: 0 = bottom, 1 = top)
        let px = self.x_value * s;
        let py = (1.0 - self.y_value) * s;

        // Crosshair lines
        let h_line = Path::line(Point::new(0.0, py), Point::new(s, py));
        let v_line = Path::line(Point::new(px, 0.0), Point::new(px, s));
        let crosshair_stroke = Stroke::default()
            .with_color(Color {
                a: 0.3,
                ..theme::KNOB_FILL
            })
            .with_width(1.0);
        frame.stroke(&h_line, crosshair_stroke);
        frame.stroke(&v_line, crosshair_stroke);

        // Dot at intersection
        let dot = Path::circle(Point::new(px, py), 5.0);
        frame.fill(&dot, theme::KNOB_FILL);

        // Label
        if !self.label.is_empty() {
            frame.fill_text(iced::widget::canvas::Text {
                content: self.label.clone(),
                position: Point::new(s / 2.0, s + 2.0),
                color: theme::TEXT_DIM,
                size: iced::Pixels(10.0),
                horizontal_alignment: iced::alignment::Horizontal::Center,
                vertical_alignment: iced::alignment::Vertical::Top,
                font: self.font,
                ..Default::default()
            });
        }

        vec![frame.into_geometry()]
    }

    fn update(
        &self,
        state: &mut Self::State,
        event: Event,
        bounds: Rectangle,
        cursor: mouse::Cursor,
    ) -> (canvas::event::Status, Option<Message<M>>) {
        let s = self.pad_size;

        match event {
            Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left))
                if cursor.position_in(bounds).is_some() => {
                    state.dragging = true;
                    return (
                        canvas::event::Status::Captured,
                        Some(Message::Param(ParamMessage::Batch(vec![
                            ParamMessage::BeginEdit(self.x_id),
                            ParamMessage::BeginEdit(self.y_id),
                        ]))),
                    );
                }
            Event::Mouse(mouse::Event::CursorMoved { .. })
                if state.dragging => {
                    if let Some(pos) = cursor.position_in(bounds) {
                        let x_norm = (pos.x / s).clamp(0.0, 1.0) as f64;
                        let y_norm = (1.0 - pos.y / s).clamp(0.0, 1.0) as f64;
                        return (
                            canvas::event::Status::Captured,
                            Some(Message::Param(ParamMessage::Batch(vec![
                                ParamMessage::SetNormalized(self.x_id, x_norm),
                                ParamMessage::SetNormalized(self.y_id, y_norm),
                            ]))),
                        );
                    }
                }
            Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left))
                if state.dragging => {
                    state.dragging = false;
                    return (
                        canvas::event::Status::Captured,
                        Some(Message::Param(ParamMessage::Batch(vec![
                            ParamMessage::EndEdit(self.x_id),
                            ParamMessage::EndEdit(self.y_id),
                        ]))),
                    );
                }
            _ => {}
        }

        (canvas::event::Status::Ignored, None)
    }

    fn mouse_interaction(
        &self,
        state: &Self::State,
        bounds: Rectangle,
        cursor: mouse::Cursor,
    ) -> mouse::Interaction {
        if state.dragging {
            return mouse::Interaction::Grabbing;
        }
        if cursor.position_in(bounds).is_some() {
            return mouse::Interaction::Crosshair;
        }
        mouse::Interaction::default()
    }
}
