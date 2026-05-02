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
        GridLayout::build("{upper_name}", "V0.1", 2, 50.0, vec![widgets(vec![
            {layout_knob | unescaped},
        ])])
    }
}

{plugin_macro | unescaped}

#[cfg(test)]
mod tests \{
    use super::*;

    #[test]
    fn builds_and_runs() \{
        let result = {test_body | unescaped};
        truce_test::assert_no_nans(&result.output);
    }
{{- if is_effect }}

    #[test]
    fn renders_nonzero_output() \{
        let result = {test_body | unescaped};
        truce_test::assert_nonzero(&result.output);
    }

    #[test]
    fn bus_config_effect() \{
        truce_test::assert_bus_config_effect::<Plugin>();
    }
{{- endif }}

    #[test]
    fn info_is_valid() \{
        truce_test::assert_valid_info::<Plugin>();
    }

    #[test]
    fn has_editor() \{
        truce_test::assert_has_editor::<Plugin>();
    }

    #[test]
    fn state_round_trips() \{
        truce_test::assert_state_round_trip::<Plugin>();
    }

    #[test]
    fn param_defaults_match() \{
        truce_test::assert_param_defaults_match::<Plugin>();
    }

    #[test]
    fn no_duplicate_param_ids() \{
        truce_test::assert_no_duplicate_param_ids::<Plugin>();
    }

    #[test]
    fn corrupt_state_no_crash() \{
        truce_test::assert_corrupt_state_no_crash::<Plugin>();
    }

    #[test]
    fn param_normalized_clamped() \{
        truce_test::assert_param_normalized_clamped::<Plugin>();
    }
}
