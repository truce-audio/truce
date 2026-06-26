//! Selector widget wrapping iced's `pick_list` for enum parameters.

use std::fmt::Debug;
use std::marker::PhantomData;

use crate::iced::Element;
use crate::iced::widget::{column, pick_list, text};

use crate::param_cache::ParamCache;
use crate::param_message::{Message, ParamMessage};
use crate::theme;
use truce_params::Params;

/// Builder for a parameter-bound selector (pick list).
pub struct SelectorWidget<'a, M> {
    id: u32,
    options: Vec<String>,
    selected: Option<String>,
    label: Option<&'a str>,
    _phantom: PhantomData<M>,
}

impl<'a, M: Clone + Debug + 'static> SelectorWidget<'a, M> {
    pub fn new(id: impl Into<u32>, params: &'a ParamCache<impl Params>) -> Self {
        let id = id.into();
        let value = params.get(id);
        let infos = params.params().param_infos();
        let info = infos.iter().find(|i| i.id == id);

        let (options, selected) = if let Some(info) = info {
            let count = info.range.step_count_usize().saturating_add(1);
            let opts: Vec<String> = (0..count)
                .map(|i| {
                    let norm = truce_core::cast::discrete_norm(i, count);
                    let plain = info.range.denormalize(norm);
                    params
                        .params()
                        .format_value(id, plain)
                        .unwrap_or_else(|| format!("{plain:.0}"))
                })
                .collect();
            let sel_idx = truce_core::cast::discrete_index(value, count);
            let selected = opts.get(sel_idx).cloned();
            (opts, selected)
        } else {
            (vec![], None)
        };

        Self {
            id,
            options,
            selected,
            label: None,
            _phantom: PhantomData,
        }
    }

    #[must_use]
    pub fn label(mut self, label: &'a str) -> Self {
        self.label = Some(label);
        self
    }

    #[must_use]
    pub fn into_element(self) -> Element<'a, Message<M>> {
        let id = self.id;
        let count = self.options.len();
        let options = self.options.clone();

        let pl = pick_list(self.options, self.selected, move |selected: String| {
            let idx = options.iter().position(|o| *o == selected).unwrap_or(0);
            let norm = truce_core::cast::discrete_norm(idx, count);
            Message::Param(ParamMessage::SetNormalized(id, norm))
        });

        let mut col = column![pl]
            .spacing(4)
            .align_x(crate::iced::Alignment::Center);

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
