//! Toggle switch widget wrapping iced's toggler.

use std::fmt::Debug;
use std::marker::PhantomData;

use iced::widget::{column, text, toggler};
use iced::Element;

use crate::param_message::{Message, ParamMessage};
use crate::param_state::ParamState;
use crate::theme;
use truce_params::Params;

/// Builder for a parameter-bound toggle switch.
pub struct ToggleWidget<'a, M> {
    id: u32,
    value: bool,
    label: Option<&'a str>,
    _phantom: PhantomData<M>,
}

impl<'a, M: Clone + Debug + 'static> ToggleWidget<'a, M> {
    pub fn new(id: impl Into<u32>, params: &'a ParamState<impl Params>) -> Self {
        let id = id.into();
        Self {
            id,
            value: params.get(id) >= 0.5,
            label: None,
            _phantom: PhantomData,
        }
    }

    pub fn label(mut self, label: &'a str) -> Self {
        self.label = Some(label);
        self
    }

    pub fn into_element(self) -> Element<'a, Message<M>> {
        let id = self.id;

        let t = toggler(self.value)
            .on_toggle(move |on| {
                Message::Param(ParamMessage::SetNormalized(id, if on { 1.0 } else { 0.0 }))
            })
            .size(18.0);

        let mut col = column![t]
            .spacing(4)
            .align_x(iced::Alignment::Center);

        if let Some(label) = self.label {
            col = col.push(text(label).size(10).color(theme::TEXT_DIM));
        }

        col.into()
    }
}

impl<'a, M: Clone + Debug + 'static> From<ToggleWidget<'a, M>> for Element<'a, Message<M>> {
    fn from(t: ToggleWidget<'a, M>) -> Self {
        t.into_element()
    }
}
