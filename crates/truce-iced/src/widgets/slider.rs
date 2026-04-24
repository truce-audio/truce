//! Horizontal slider widget rendered via iced Canvas with relative drag.

use std::fmt::Debug;
use std::marker::PhantomData;

use iced::widget::canvas::{self, Event, Frame, Geometry, Path, Stroke, Text};
use iced::widget::Canvas;
use iced::{alignment, mouse, Color, Element, Length, Point, Rectangle, Renderer, Theme};

use crate::param_message::{Message, ParamMessage};
use crate::param_state::ParamState;
use crate::theme;
use truce_params::Params;

const TRACK_HEIGHT: f32 = 4.0;
const THUMB_RADIUS: f32 = 6.0;

/// Builder for a parameter-bound horizontal slider.
pub struct SliderWidget<'a, M> {
    id: u32,
    value: f64,
    display: String,
    label: Option<&'a str>,
    width: f32,
    font: iced::Font,
    _phantom: PhantomData<M>,
}

impl<'a, M: Clone + Debug + 'static> SliderWidget<'a, M> {
    pub fn new(id: impl Into<u32>, params: &'a ParamState<impl Params>) -> Self {
        let id = id.into();
        Self {
            id,
            value: params.get(id),
            display: params.label(id).to_string(),
            label: None,
            width: 120.0,
            font: params.font(),
            _phantom: PhantomData,
        }
    }

    pub fn label(mut self, label: &'a str) -> Self {
        self.label = Some(label);
        self
    }

    pub fn width(mut self, width: f32) -> Self {
        self.width = width;
        self
    }

    pub fn font(mut self, font: iced::Font) -> Self {
        self.font = font;
        self
    }

    pub fn into_element(self) -> Element<'a, Message<M>> {
        let total_h = THUMB_RADIUS * 2.0 + 30.0;
        let program = SliderProgram {
            id: self.id,
            value: self.value as f32,
            display: self.display,
            label: self.label.unwrap_or("").to_string(),
            font: self.font,
        };

        Canvas::new(program)
            .width(Length::Fixed(self.width))
            .height(Length::Fixed(total_h))
            .into()
    }
}

impl<'a, M: Clone + Debug + 'static> From<SliderWidget<'a, M>> for Element<'a, Message<M>> {
    fn from(s: SliderWidget<'a, M>) -> Self {
        s.into_element()
    }
}

// ---------------------------------------------------------------------------
// Canvas program
// ---------------------------------------------------------------------------

struct SliderProgram {
    id: u32,
    value: f32,
    display: String,
    label: String,
    font: iced::Font,
}

#[derive(Default)]
struct SliderState {
    dragging: bool,
    start_value: f32,
    start_x: f32,
}

impl<M: Clone + Debug + 'static> canvas::Program<Message<M>> for SliderProgram {
    type State = SliderState;

    fn draw(
        &self,
        _state: &Self::State,
        renderer: &Renderer,
        _theme: &Theme,
        bounds: Rectangle,
        _cursor: mouse::Cursor,
    ) -> Vec<Geometry> {
        let mut frame = Frame::new(renderer, bounds.size());

        let margin = THUMB_RADIUS;
        let track_y = THUMB_RADIUS;
        let track_left = margin;
        let track_right = bounds.width - margin;
        let track_width = track_right - track_left;

        // Track background
        let track_bg = Path::line(
            Point::new(track_left, track_y),
            Point::new(track_right, track_y),
        );
        frame.stroke(
            &track_bg,
            Stroke::default()
                .with_color(theme::KNOB_TRACK)
                .with_width(TRACK_HEIGHT)
                .with_line_cap(iced::widget::canvas::LineCap::Round),
        );

        // Filled portion
        let fill_x = track_left + self.value * track_width;
        if self.value > 0.001 {
            let track_fill =
                Path::line(Point::new(track_left, track_y), Point::new(fill_x, track_y));
            frame.stroke(
                &track_fill,
                Stroke::default()
                    .with_color(theme::KNOB_FILL)
                    .with_width(TRACK_HEIGHT)
                    .with_line_cap(iced::widget::canvas::LineCap::Round),
            );
        }

        // Thumb
        let thumb = Path::circle(Point::new(fill_x, track_y), THUMB_RADIUS);
        frame.fill(&thumb, theme::KNOB_POINTER);

        // Value text
        let text_y = THUMB_RADIUS * 2.0 + 4.0;
        let cx = bounds.width / 2.0;
        frame.fill_text(Text {
            content: self.display.clone(),
            position: Point::new(cx, text_y),
            color: Color::from_rgb(0.90, 0.90, 0.92),
            size: iced::Pixels(11.0),
            horizontal_alignment: alignment::Horizontal::Center,
            vertical_alignment: alignment::Vertical::Top,
            font: self.font,
            ..Text::default()
        });

        // Label text
        if !self.label.is_empty() {
            let label_y = text_y + 14.0;
            frame.fill_text(Text {
                content: self.label.clone(),
                position: Point::new(cx, label_y),
                color: theme::TEXT_DIM,
                size: iced::Pixels(10.0),
                horizontal_alignment: alignment::Horizontal::Center,
                vertical_alignment: alignment::Vertical::Top,
                font: self.font,
                ..Text::default()
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
        match event {
            Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)) => {
                if let Some(pos) = cursor.position_in(bounds) {
                    let track_top = 0.0;
                    let track_bottom = THUMB_RADIUS * 2.0;
                    if pos.y >= track_top && pos.y <= track_bottom {
                        state.dragging = true;
                        state.start_value = self.value;
                        state.start_x = pos.x;
                        return (
                            canvas::event::Status::Captured,
                            Some(Message::Param(ParamMessage::BeginEdit(self.id))),
                        );
                    }
                }
            }
            Event::Mouse(mouse::Event::CursorMoved { .. }) => {
                if state.dragging {
                    if let Some(pos) = cursor.position() {
                        let current_x = pos.x - bounds.x;
                        let track_width = bounds.width - THUMB_RADIUS * 2.0;
                        let delta = (current_x - state.start_x) / track_width;
                        let new_value = (state.start_value + delta).clamp(0.0, 1.0);
                        return (
                            canvas::event::Status::Captured,
                            Some(Message::Param(ParamMessage::SetNormalized(
                                self.id,
                                new_value as f64,
                            ))),
                        );
                    }
                }
            }
            Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left)) => {
                if state.dragging {
                    state.dragging = false;
                    return (
                        canvas::event::Status::Captured,
                        Some(Message::Param(ParamMessage::EndEdit(self.id))),
                    );
                }
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
        if let Some(pos) = cursor.position_in(bounds) {
            if pos.y <= THUMB_RADIUS * 2.0 {
                return mouse::Interaction::Grab;
            }
        }
        mouse::Interaction::default()
    }
}
