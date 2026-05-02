use truce::prelude::*;
use truce_gui::layout::\{GridLayout, knob, widgets};

{params_struct | unescaped}

use {struct_name}ParamsParamId as P;

pub struct {struct_name} \{
    params: Arc<{struct_name}Params>,
}

impl {struct_name} \{
    pub fn new(params: Arc<{struct_name}Params>) -> Self \{
        Self \{ params }
    }
}

impl PluginLogic for {struct_name} \{
    fn reset(&mut self, sr: f64, _bs: usize) \{
        self.params.set_sample_rate(sr);
        self.params.snap_smoothers();
    }

{process_body | unescaped}

    fn layout(&self) -> truce_gui::layout::GridLayout \{
        GridLayout::build(vec![widgets(vec![
            {layout_knob | unescaped},
        ])])
    }
}

{plugin_macro | unescaped}
