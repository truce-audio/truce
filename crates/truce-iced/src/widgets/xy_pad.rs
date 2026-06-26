//! XY pad widget for controlling two parameters simultaneously.

use std::fmt::Debug;
use std::marker::PhantomData;

use crate::iced::widget::Canvas;
use crate::iced::widget::canvas::{self, Event, Frame, Geometry, Path, Stroke};
use crate::iced::{Color, Element, Length, Point, Rectangle, Renderer, Size, Theme, mouse};

use crate::param_cache::ParamCache;
use crate::param_message::{Message, ParamMessage};
use crate::theme;
use truce_core::Float;
use truce_params::Params;

/// Builder for an XY pad controlling two parameters.
pub struct XYPadWidget<'a, M> {
    x_id: u32,
    y_id: u32,
    x_value: f64,
    y_value: f64,
    label: Option<&'a str>,
    /// Width / height passed to `Canvas` via iced `Length`. Default
    /// to `Length::Fixed(120)` for a square pad; `.fill()` swaps
    /// both to `Length::Fill` so the pad stretches with its
    /// parent container. The draw + update programs read the
    /// runtime `bounds` so the pad's coordinate math follows
    /// whatever size iced computes.
    width: Length,
    height: Length,
    font: crate::iced::Font,
    _phantom: PhantomData<M>,
}

impl<'a, M: Clone + Debug + 'static> XYPadWidget<'a, M> {
    pub fn new(
        x_id: impl Into<u32>,
        y_id: impl Into<u32>,
        params: &'a ParamCache<impl Params>,
    ) -> Self {
        let x_id = x_id.into();
        let y_id = y_id.into();
        Self {
            x_id,
            y_id,
            x_value: params.get(x_id),
            y_value: params.get(y_id),
            label: None,
            width: Length::Fixed(120.0),
            height: Length::Fixed(120.0 + LABEL_H),
            font: params.font(),
            _phantom: PhantomData,
        }
    }

    #[must_use]
    pub fn label(mut self, label: &'a str) -> Self {
        self.label = Some(label);
        self
    }

    /// Set a fixed square size. Equivalent to
    /// `.width(Length::Fixed(s)).height(Length::Fixed(s + label))`.
    #[must_use]
    pub fn size(mut self, size: f32) -> Self {
        self.width = Length::Fixed(size);
        self.height = Length::Fixed(size + LABEL_H);
        self
    }

    /// Make the pad stretch to fill its parent container in both
    /// axes. The pad's coordinate math reads the runtime bounds
    /// so dragging works at whatever size iced lays out.
    #[must_use]
    pub fn fill(mut self) -> Self {
        self.width = Length::Fill;
        self.height = Length::Fill;
        self
    }

    /// Explicit width override (use `Length::Fill` to stretch
    /// horizontally only).
    #[must_use]
    pub fn width(mut self, width: Length) -> Self {
        self.width = width;
        self
    }

    /// Explicit height override (use `Length::Fill` to stretch
    /// vertically only).
    #[must_use]
    pub fn height(mut self, height: Length) -> Self {
        self.height = height;
        self
    }

    #[must_use]
    pub fn font(mut self, font: crate::iced::Font) -> Self {
        self.font = font;
        self
    }

    #[must_use]
    pub fn into_element(self) -> Element<'a, Message<M>> {
        let program = XYPadProgram {
            x_id: self.x_id,
            y_id: self.y_id,
            x_value: f32::from_f64(self.x_value),
            y_value: f32::from_f64(self.y_value),
            label: self.label.unwrap_or("").to_string(),
            font: self.font,
        };

        Canvas::new(program)
            .width(self.width)
            .height(self.height)
            .into()
    }
}

/// Pixels reserved at the bottom of the canvas for the label.
const LABEL_H: f32 = 16.0;

impl<'a, M: Clone + Debug + 'static> From<XYPadWidget<'a, M>> for Element<'a, Message<M>> {
    fn from(xy: XYPadWidget<'a, M>) -> Self {
        xy.into_element()
    }
}

// Canvas program

struct XYPadProgram {
    x_id: u32,
    y_id: u32,
    x_value: f32,
    y_value: f32,
    label: String,
    font: crate::iced::Font,
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
        // Reserve the bottom strip of the canvas for the label;
        // the pad rectangle sits above it. When the parent makes
        // us tiny, the floor of 1.0 keeps the math sane.
        let label_h = if self.label.is_empty() { 0.0 } else { LABEL_H };
        let pad_w = bounds.width;
        let pad_h = (bounds.height - label_h).max(1.0);

        // Background
        let bg = Path::rectangle(Point::ORIGIN, Size::new(pad_w, pad_h));
        frame.fill(&bg, theme::SURFACE);

        // Border
        frame.stroke(
            &bg,
            Stroke::default().with_color(theme::ACCENT).with_width(1.0),
        );

        // Crosshair position (Y inverted: 0 = bottom, 1 = top)
        let px = self.x_value * pad_w;
        let py = (1.0 - self.y_value) * pad_h;

        // Crosshair lines
        let h_line = Path::line(Point::new(0.0, py), Point::new(pad_w, py));
        let v_line = Path::line(Point::new(px, 0.0), Point::new(px, pad_h));
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

        // Label below the pad rect.
        if !self.label.is_empty() {
            frame.fill_text(crate::iced::widget::canvas::Text {
                content: self.label.clone(),
                position: Point::new(pad_w / 2.0, pad_h + 2.0),
                color: theme::TEXT_DIM,
                size: crate::iced::Pixels(10.0),
                align_x: crate::iced::alignment::Horizontal::Center.into(),
                align_y: crate::iced::alignment::Vertical::Top,
                font: self.font,
                ..Default::default()
            });
        }

        vec![frame.into_geometry()]
    }

    fn update(
        &self,
        state: &mut Self::State,
        event: &Event,
        bounds: Rectangle,
        cursor: mouse::Cursor,
    ) -> Option<canvas::Action<Message<M>>> {
        // Match the pad rect carved out by `draw` so the
        // click-through and drag math agree even when the parent
        // layout sized us non-square.
        let label_h = if self.label.is_empty() { 0.0 } else { LABEL_H };
        let pad_w = bounds.width.max(1.0);
        let pad_h = (bounds.height - label_h).max(1.0);

        match event {
            Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left))
                if cursor.position_in(bounds).is_some() =>
            {
                state.dragging = true;
                return Some(
                    canvas::Action::publish(Message::Param(ParamMessage::Batch(vec![
                        ParamMessage::BeginEdit(self.x_id),
                        ParamMessage::BeginEdit(self.y_id),
                    ])))
                    .and_capture(),
                );
            }
            Event::Mouse(mouse::Event::CursorMoved { .. }) if state.dragging => {
                // While dragging we want updates even when the cursor
                // leaves the pad - `cursor.position_in(bounds)` returns
                // `None` outside, freezing the indicator. Use the
                // window-space position and clamp into the pad rect
                // ourselves (mirrors `KnobProgram::update`).
                if let Some(pos) = cursor.position() {
                    let x_norm = f64::from(((pos.x - bounds.x) / pad_w).clamp(0.0, 1.0));
                    let y_norm = f64::from((1.0 - (pos.y - bounds.y) / pad_h).clamp(0.0, 1.0));
                    return Some(
                        canvas::Action::publish(Message::Param(ParamMessage::Batch(vec![
                            ParamMessage::SetNormalized(self.x_id, x_norm),
                            ParamMessage::SetNormalized(self.y_id, y_norm),
                        ])))
                        .and_capture(),
                    );
                }
            }
            Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left)) if state.dragging => {
                state.dragging = false;
                return Some(
                    canvas::Action::publish(Message::Param(ParamMessage::Batch(vec![
                        ParamMessage::EndEdit(self.x_id),
                        ParamMessage::EndEdit(self.y_id),
                    ])))
                    .and_capture(),
                );
            }
            _ => {}
        }

        None
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
