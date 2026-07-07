//! XY pad widget for controlling two parameters simultaneously.

use std::fmt::Debug;
use std::marker::PhantomData;

use iced_core::renderer::Quad;
use iced_core::widget::{Tree, tree};
use iced_core::{
    Border, Clipboard, Event, Layout, Length, Renderer as _, Shadow, Shell, Widget, layout, mouse,
    renderer,
};

use crate::iced::widget::{column, text};
use crate::iced::{Alignment, Color, Element, Rectangle, Renderer, Size, Theme};
use crate::param_cache::ParamCache;
use crate::param_message::{Message, ParamMessage};
use crate::theme;
use truce_core::Float;
use truce_params::Params;

/// Radius of the draggable dot (the dot is `2 * radius` px). The dot is
/// painted in the editor's shared coordinate space, so at a value
/// extreme its center sits on the pad edge and its outer half spills
/// past the border - drawn unclipped over the neighboring gap rather
/// than shrinking the pad or growing its footprint.
const DOT_RADIUS: f32 = 5.0;

/// Builder for an XY pad controlling two parameters.
pub struct XYPadWidget<'a, M> {
    x_id: u32,
    y_id: u32,
    x_value: f64,
    y_value: f64,
    label: Option<&'a str>,
    /// The pad's size. Defaults to `Length::Fixed(120)` square; `.fill()`
    /// swaps both to `Length::Fill` so the pad stretches with its parent
    /// container. The label (when set) is a sibling below the pad, so
    /// this is the pad rect alone, not the pad-plus-label.
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
            height: Length::Fixed(120.0),
            font: params.font(),
            _phantom: PhantomData,
        }
    }

    #[must_use]
    pub fn label(mut self, label: &'a str) -> Self {
        self.label = Some(label);
        self
    }

    /// Set a fixed square pad size.
    #[must_use]
    pub fn size(mut self, size: f32) -> Self {
        self.width = Length::Fixed(size);
        self.height = Length::Fixed(size);
        self
    }

    /// Make the pad stretch to fill its parent container in both axes.
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
        let pad: Element<'a, Message<M>> = Element::new(XYPad {
            x_id: self.x_id,
            y_id: self.y_id,
            x_value: f32::from_f64(self.x_value),
            y_value: f32::from_f64(self.y_value),
            width: self.width,
            height: self.height,
            _phantom: PhantomData,
        });

        match self.label {
            Some(label) if !label.is_empty() => column![pad]
                .push(text(label).size(10).color(theme::TEXT_DIM).font(self.font))
                .width(self.width)
                .spacing(2)
                .align_x(Alignment::Center)
                .into(),
            _ => pad,
        }
    }
}

impl<'a, M: Clone + Debug + 'static> From<XYPadWidget<'a, M>> for Element<'a, Message<M>> {
    fn from(xy: XYPadWidget<'a, M>) -> Self {
        xy.into_element()
    }
}

/// The pad itself: a custom widget so the dot can paint past the pad's
/// bounds. A `canvas` draws into a frame anchored at its bounds and so
/// can't render left of / above its origin; painting via `fill_quad`
/// puts the dot in the editor's shared coordinate space instead, where
/// it spills over the border unclipped in every direction.
struct XYPad<M> {
    x_id: u32,
    y_id: u32,
    x_value: f32,
    y_value: f32,
    width: Length,
    height: Length,
    _phantom: PhantomData<M>,
}

#[derive(Default)]
struct XYPadState {
    dragging: bool,
}

impl<M: Clone + Debug + 'static> Widget<Message<M>, Theme, Renderer> for XYPad<M> {
    fn tag(&self) -> tree::Tag {
        tree::Tag::of::<XYPadState>()
    }

    fn state(&self) -> tree::State {
        tree::State::new(XYPadState::default())
    }

    fn size(&self) -> Size<Length> {
        Size::new(self.width, self.height)
    }

    fn layout(
        &mut self,
        _tree: &mut Tree,
        _renderer: &Renderer,
        limits: &layout::Limits,
    ) -> layout::Node {
        layout::atomic(limits, self.width, self.height)
    }

    fn draw(
        &self,
        _tree: &Tree,
        renderer: &mut Renderer,
        _theme: &Theme,
        _style: &renderer::Style,
        layout: Layout<'_>,
        _cursor: mouse::Cursor,
        _viewport: &Rectangle,
    ) {
        let bounds = layout.bounds();
        let pad_w = bounds.width;
        let pad_h = bounds.height;

        // Background + 1px border in a single quad.
        renderer.fill_quad(
            Quad {
                bounds,
                border: Border {
                    color: theme::ACCENT,
                    width: 1.0,
                    radius: 0.0.into(),
                },
                shadow: Shadow::default(),
                snap: true,
            },
            theme::SURFACE,
        );

        // Crosshair at the value point (Y inverted: 0 = bottom, 1 = top).
        let px = bounds.x + self.x_value.clamp(0.0, 1.0) * pad_w;
        let py = bounds.y + (1.0 - self.y_value.clamp(0.0, 1.0)) * pad_h;
        let crosshair = Color {
            a: 0.3,
            ..theme::KNOB_FILL
        };
        renderer.fill_quad(
            Quad {
                bounds: Rectangle {
                    x: bounds.x,
                    y: py - 0.5,
                    width: pad_w,
                    height: 1.0,
                },
                border: Border::default(),
                shadow: Shadow::default(),
                snap: false,
            },
            crosshair,
        );
        renderer.fill_quad(
            Quad {
                bounds: Rectangle {
                    x: px - 0.5,
                    y: bounds.y,
                    width: 1.0,
                    height: pad_h,
                },
                border: Border::default(),
                shadow: Shadow::default(),
                snap: false,
            },
            crosshair,
        );

        // Dot: a quad with a half-side corner radius renders as a circle.
        // Its bounds can fall outside `bounds` at the value extremes; iced
        // doesn't clip a widget's quads to its layout, so it draws in full.
        renderer.fill_quad(
            Quad {
                bounds: Rectangle {
                    x: px - DOT_RADIUS,
                    y: py - DOT_RADIUS,
                    width: DOT_RADIUS * 2.0,
                    height: DOT_RADIUS * 2.0,
                },
                border: Border {
                    color: Color::TRANSPARENT,
                    width: 0.0,
                    radius: DOT_RADIUS.into(),
                },
                shadow: Shadow::default(),
                snap: false,
            },
            theme::KNOB_FILL,
        );
    }

    fn update(
        &mut self,
        tree: &mut Tree,
        event: &Event,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        _renderer: &Renderer,
        _clipboard: &mut dyn Clipboard,
        shell: &mut Shell<'_, Message<M>>,
        _viewport: &Rectangle,
    ) {
        let state = tree.state.downcast_mut::<XYPadState>();
        let bounds = layout.bounds();
        let pad_w = bounds.width.max(1.0);
        let pad_h = bounds.height.max(1.0);

        match event {
            Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left))
                if cursor.position_in(bounds).is_some() =>
            {
                state.dragging = true;
                shell.publish(Message::Param(ParamMessage::Batch(vec![
                    ParamMessage::BeginEdit(self.x_id),
                    ParamMessage::BeginEdit(self.y_id),
                ])));
                shell.capture_event();
            }
            Event::Mouse(mouse::Event::CursorMoved { .. }) if state.dragging => {
                // Track the cursor even outside the pad while dragging, so
                // the dot follows to the edge rather than freezing.
                if let Some(pos) = cursor.position() {
                    let x_norm = f64::from(((pos.x - bounds.x) / pad_w).clamp(0.0, 1.0));
                    let y_norm = f64::from((1.0 - (pos.y - bounds.y) / pad_h).clamp(0.0, 1.0));
                    shell.publish(Message::Param(ParamMessage::Batch(vec![
                        ParamMessage::SetNormalized(self.x_id, x_norm),
                        ParamMessage::SetNormalized(self.y_id, y_norm),
                    ])));
                    shell.capture_event();
                }
            }
            Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left)) if state.dragging => {
                state.dragging = false;
                shell.publish(Message::Param(ParamMessage::Batch(vec![
                    ParamMessage::EndEdit(self.x_id),
                    ParamMessage::EndEdit(self.y_id),
                ])));
                shell.capture_event();
            }
            _ => {}
        }
    }

    fn mouse_interaction(
        &self,
        tree: &Tree,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        _viewport: &Rectangle,
        _renderer: &Renderer,
    ) -> mouse::Interaction {
        if tree.state.downcast_ref::<XYPadState>().dragging {
            mouse::Interaction::Grabbing
        } else if cursor.position_in(layout.bounds()).is_some() {
            mouse::Interaction::Crosshair
        } else {
            mouse::Interaction::None
        }
    }
}
