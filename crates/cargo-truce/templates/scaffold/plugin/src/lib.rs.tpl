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

    /// Renders the plugin's editor headlessly and compares the
    /// resulting PNG to a committed reference at
    /// `screenshots/default.png`. Catches visual regressions in
    /// the layout, theme, widget rendering, and GPU pipeline
    /// without needing a DAW.
    ///
    /// # First run / regenerating the reference
    ///
    /// On first invocation (no reference PNG yet) the test will
    /// fail and write the rendered image to
    /// `screenshots/default.png.actual`. Inspect that file; if it
    /// looks correct, commit it as the new reference:
    ///
    ///     cargo truce screenshot -p {crate_name}
    ///
    /// That command renders the current state and writes
    /// `target/screenshots/{crate_name}_screenshot.png` — copy it
    /// (or re-render with `--name default`) to
    /// `screenshots/default.png` to make it the test's baseline.
    ///
    /// # Tuning the threshold
    ///
    /// The third arg (`0`) is the maximum allowed pixel
    /// difference. Set to `0` for strict equality. Bump it to
    /// tolerate small rasterizer drift across OS / GPU drivers
    /// (typical: 50–500 pixels for fonts and antialiasing).
    #[test]
    fn gui_screenshot() \{
        truce_test::assert_screenshot::<Plugin>("default", "screenshots", 0);
    }
}
