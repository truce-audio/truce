use truce::prelude::*;
use truce_gui_types::layout::\{GridLayout, knob, widgets};

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
    type Params = {struct_name}Params;

{bus_layouts_method | unescaped}    fn reset(&mut self, config: &AudioConfig) \{
        self.params.set_sample_rate(config.sample_rate);
        self.params.snap_smoothers();
    }

{process_body | unescaped}

    fn editor(params: Arc<{struct_name}Params>) -> Box<dyn Editor> \{
        truce_gui::default_editor(
            params,
            GridLayout::build(vec![widgets(vec![{layout_knob | unescaped}])]),
        )
    }
}

{plugin_macro | unescaped}

// Installs the real-time allocation checker under `--features rt-paranoid`
// (a no-op otherwise). Wrap a driver run in `assert_no_audio_alloc` to
// fail a test if `process` ever allocates. See the audio-testing guide.
truce::enable_rt_paranoid!();
