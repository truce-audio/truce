use truce::prelude::*;
use truce_gui_types::layout::\{GridLayout, knob, widgets};

{params_struct | unescaped}

use {struct_name}ParamsParamId as P;

{descriptor_block | unescaped}pub struct {struct_name};

impl {impl_trait} for {struct_name} \{
    type Params = {struct_name}Params;
{dsp_state_type | unescaped}
{bus_layouts_method | unescaped}{process_body | unescaped}

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
