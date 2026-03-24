//! Rotary knob widget rendered via iced Canvas.

use std::fmt::Debug;
use std::marker::PhantomData;

use iced::widget::canvas::{self, path::Arc, Event, Frame, Geometry, Path, Stroke, Text};
use iced::widget::Canvas;
use iced::{
    alignment, mouse, Color, Element, Length, Point, Rectangle, Renderer, Theme,
};

use crate::param_message::{Message, ParamMessage};
use crate::param_state::ParamState;
use crate::theme;
use truce_params::Params;

const START_ANGLE: f32 = std::f32::consts::PI * 0.75;
const END_ANGLE: f32 = std::f32::consts::PI * 2.25;
const DRAG_SENSITIVITY: f32 = 200.0;

/// Builder for a rotary knob widget.
pub struct KnobWidget<'a, M> {
    id: u32,
    value: f64,
    display: String,
    label: Option<&'a str>,
    size: f32,
    _phantom: PhantomData<M>,
}

impl<'a, M: Clone + Debug + 'static> KnobWidget<'a, M> {
    pub fn new(id: impl Into<u32>, params: &'a ParamState<impl Params>) -> Self {
        let id = id.into();
        Self {
            id,
            value: params.get(id),
            display: params.label(id).to_string(),
            label: None,
            size: 60.0,
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

    /// Convert into an iced Element.
    pub fn into_element(self) -> Element<'a, Message<M>> {
        let total_h = self.size + 30.0; // Extra space for label + value text
        let program = KnobProgram {
            id: self.id,
            value: self.value as f32,
            display: self.display,
            label: self.label.unwrap_or("").to_string(),
        };

        Canvas::new(program)
            .width(Length::Fixed(self.size))
            .height(Length::Fixed(total_h))
            .into()
    }
}

impl<'a, M: Clone + Debug + 'static> From<KnobWidget<'a, M>> for Element<'a, Message<M>> {
    fn from(knob: KnobWidget<'a, M>) -> Self {
        knob.into_element()
    }
}

// ---------------------------------------------------------------------------
// Canvas program
// ---------------------------------------------------------------------------

struct KnobProgram {
    id: u32,
    value: f32,
    display: String,
    label: String,
}

#[derive(Default)]
struct KnobState {
    dragging: bool,
    start_value: f32,
    start_y: f32,
}

impl<M: Clone + Debug + 'static> canvas::Program<Message<M>> for KnobProgram {
    type State = KnobState;

    fn draw(
        &self,
        _state: &Self::State,
        renderer: &Renderer,
        _theme: &Theme,
        bounds: Rectangle,
        _cursor: mouse::Cursor,
    ) -> Vec<Geometry> {
        let mut frame = Frame::new(renderer, bounds.size());

        let cx = bounds.width / 2.0;
        let cy = bounds.width / 2.0; // Square knob area
        let radius = (bounds.width / 2.0 - 5.0).max(8.0);

        // Track arc (full range background)
        let track = Path::new(|b| {
            b.arc(Arc {
                center: Point::new(cx, cy),
                radius,
                start_angle: iced::Radians(START_ANGLE),
                end_angle: iced::Radians(END_ANGLE),
            });
        });
        frame.stroke(
            &track,
            Stroke::default()
                .with_color(theme::KNOB_TRACK)
                .with_width(3.0),
        );

        // Value arc
        let value_angle = START_ANGLE + self.value * (END_ANGLE - START_ANGLE);
        if self.value > 0.001 {
            let value_path = Path::new(|b| {
                b.arc(Arc {
                    center: Point::new(cx, cy),
                    radius,
                    start_angle: iced::Radians(START_ANGLE),
                    end_angle: iced::Radians(value_angle),
                });
            });
            frame.stroke(
                &value_path,
                Stroke::default()
                    .with_color(theme::KNOB_FILL)
                    .with_width(3.0),
            );
        }

        // Pointer line
        let pointer_len = radius * 0.65;
        let px = cx + pointer_len * value_angle.cos();
        let py = cy + pointer_len * value_angle.sin();
        let pointer = Path::line(Point::new(cx, cy), Point::new(px, py));
        frame.stroke(
            &pointer,
            Stroke::default()
                .with_color(theme::KNOB_POINTER)
                .with_width(2.0),
        );

        // Center dot
        let dot = Path::circle(Point::new(cx, cy), 3.0);
        frame.fill(&dot, theme::KNOB_POINTER);

        // Value text
        let value_y = bounds.width + 2.0;
        frame.fill_text(Text {
            content: self.display.clone(),
            position: Point::new(cx, value_y),
            color: Color::from_rgb(0.90, 0.90, 0.92),
            size: iced::Pixels(11.0),
            horizontal_alignment: alignment::Horizontal::Center,
            vertical_alignment: alignment::Vertical::Top,
            ..Text::default()
        });

        // Label text
        if !self.label.is_empty() {
            let label_y = value_y + 14.0;
            frame.fill_text(Text {
                content: self.label.clone(),
                position: Point::new(cx, label_y),
                color: theme::TEXT_DIM,
                size: iced::Pixels(10.0),
                horizontal_alignment: alignment::Horizontal::Center,
                vertical_alignment: alignment::Vertical::Top,
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
                    let cx = bounds.width / 2.0;
                    let cy = bounds.width / 2.0;
                    let dx = pos.x - cx;
                    let dy = pos.y - cy;
                    let dist = (dx * dx + dy * dy).sqrt();
                    let radius = bounds.width / 2.0 - 5.0;

                    if dist <= radius + 5.0 {
                        state.dragging = true;
                        state.start_value = self.value;
                        state.start_y = pos.y;
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
                        let delta = (state.start_y - (pos.y - bounds.y)) / DRAG_SENSITIVITY;
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
            let cx = bounds.width / 2.0;
            let cy = bounds.width / 2.0;
            let dx = pos.x - cx;
            let dy = pos.y - cy;
            let dist = (dx * dx + dy * dy).sqrt();
            if dist <= bounds.width / 2.0 {
                return mouse::Interaction::Grab;
            }
        }
        mouse::Interaction::default()
    }
}
