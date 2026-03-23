//! Selector widget wrapping iced's pick_list for enum parameters.

use std::fmt::Debug;
use std::marker::PhantomData;

use iced::widget::{column, pick_list, text};
use iced::Element;

use crate::param_message::{Message, ParamMessage};
use crate::param_state::ParamState;
use crate::theme;
use truce_params::Params;

/// Builder for a parameter-bound selector (pick list).
pub struct SelectorWidget<'a, M> {
    id: u32,
    _value: f64,
    options: Vec<String>,
    selected: Option<String>,
    label: Option<&'a str>,
    _phantom: PhantomData<M>,
}

impl<'a, M: Clone + Debug + 'static> SelectorWidget<'a, M> {
    pub fn new(id: u32, params: &'a ParamState<impl Params>) -> Self {
        let value = params.get(id);
        let infos = params.params().param_infos();
        let info = infos.iter().find(|i| i.id == id);

        let (options, selected) = if let Some(info) = info {
            let count = info.range.step_count().max(1) as usize;
            let opts: Vec<String> = (0..count)
                .map(|i| {
                    let norm = if count <= 1 {
                        0.0
                    } else {
                        i as f64 / (count - 1) as f64
                    };
                    let plain = info.range.denormalize(norm);
                    params
                        .params()
                        .format_value(id, plain)
                        .unwrap_or_else(|| format!("{:.0}", plain))
                })
                .collect();
            let sel_idx = (value * (count - 1).max(1) as f64).round() as usize;
            let selected = opts.get(sel_idx).cloned();
            (opts, selected)
        } else {
            (vec![], None)
        };

        Self {
            id,
            _value: value,
            options,
            selected,
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
        let count = self.options.len();
        let options = self.options.clone();

        let pl = pick_list(self.options, self.selected, move |selected: String| {
            let idx = options.iter().position(|o| *o == selected).unwrap_or(0);
            let norm = if count <= 1 {
                0.0
            } else {
                idx as f64 / (count - 1) as f64
            };
            Message::Param(ParamMessage::SetNormalized(id, norm))
        });

        let mut col = column![pl]
            .spacing(4)
            .align_x(iced::Alignment::Center);

        if let Some(label) = self.label {
            col = col.push(text(label).size(10).color(theme::TEXT_DIM));
        }

        col.into()
    }
}

impl<'a, M: Clone + Debug + 'static> From<SelectorWidget<'a, M>> for Element<'a, Message<M>> {
    fn from(s: SelectorWidget<'a, M>) -> Self {
        s.into_element()
    }
}
