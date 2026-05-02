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

    // Renders the plugin's editor headlessly and compares the
    // pixels against a committed reference PNG at
    // `screenshots/{crate_name}.png` (relative to the workspace
    // root that owns this `Cargo.toml`). Catches visual
    // regressions in the layout, theme, widget rendering, and
    // GPU pipeline without needing a DAW.
    //
    // First run: no reference exists, so the test logs a
    // "promote" hint and PASSES (it doesn't fail — committing
    // the first reference is meant to be a deliberate step). The
    // current render lands in `target/screenshots/{crate_name}.png`;
    // the log line prints the exact `cp` command to copy it into
    // `screenshots/`. Inspect the rendered PNG, then run that
    // `cp` and commit the file.
    //
    // Subsequent runs:
    //   - pixel-equal → pass silently
    //   - diff > threshold on the reference platform → panic with
    //     both PNG paths so you can diff them
    //   - diff > threshold on a non-reference platform → log the
    //     diff count and pass (per-OS wgpu rasterization differs;
    //     cross-OS pixel comparison isn't meaningful)
    //
    // The reference platform defaults to macOS; override with
    // `TRUCE_SCREENSHOT_REFERENCE_OS=linux` (or `windows`) if
    // your CI runs on a different host.
    //
    // The third arg is the max allowed differing-pixel count. `0`
    // is strict equality; bump it (50–500 is typical) if anti-
    // aliasing on fonts or curves wobbles between machines.
    #[test]
    fn gui_screenshot() \{
        truce_test::assert_screenshot::<Plugin>("{crate_name}", "screenshots", 0);
    }
}
