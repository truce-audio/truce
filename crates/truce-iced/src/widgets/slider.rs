//! Horizontal slider widget wrapping iced's built-in slider.

use std::fmt::Debug;
use std::marker::PhantomData;

use iced::widget::{column, slider, text};
use iced::{Element, Length};

use crate::param_message::{Message, ParamMessage};
use crate::param_state::ParamState;
use crate::theme;
use truce_params::Params;

/// Builder for a parameter-bound horizontal slider.
pub struct SliderWidget<'a, M> {
    id: u32,
    value: f64,
    display: String,
    label: Option<&'a str>,
    width: f32,
    _phantom: PhantomData<M>,
}

impl<'a, M: Clone + Debug + 'static> SliderWidget<'a, M> {
    pub fn new(id: u32, params: &'a ParamState<impl Params>) -> Self {
        Self {
            id,
            value: params.get(id),
            display: params.label(id).to_string(),
            label: None,
            width: 120.0,
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

    pub fn into_element(self) -> Element<'a, Message<M>> {
        let id = self.id;
        let val = self.value as f32;

        let s = slider(0.0..=1.0, val, move |v| {
            Message::Param(ParamMessage::SetNormalized(id, v as f64))
        })
        .width(Length::Fixed(self.width));

        let display = self.display;
        let mut col = column![s, text(display).size(11)]
            .spacing(2)
            .align_x(iced::Alignment::Center);

        if let Some(label) = self.label {
            col = col.push(text(label).size(10).color(theme::TEXT_DIM));
        }

        col.into()
    }
}

impl<'a, M: Clone + Debug + 'static> From<SliderWidget<'a, M>> for Element<'a, Message<M>> {
    fn from(s: SliderWidget<'a, M>) -> Self {
        s.into_element()
    }
}
