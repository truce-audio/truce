//! `Editor::idle` demonstration: a custom editor whose idle tick sweeps a
//! knob back and forth.
//!
//! The knob moves only because `idle` fires ~60fps on the host UI thread.
//! `process` never touches the `Sweep` value, so if the wrapper doesn't
//! drive `idle` the knob sits frozen - which is exactly the symptom that
//! motivated this example. Rendering is delegated to the built-in editor;
//! `idle` just advances the value it displays, which keeps the example
//! about the hook rather than about rasterization.
//!
//! `idle` is driven by standalone, CLAP, VST3, AU v2, and LV2 (not yet the
//! AU v3 app-extension view); see [`truce_core::editor::Editor::idle`] for
//! per-format coverage. Run it with `cargo truce run -p truce-example-idle-anim`.

use std::f64::consts::TAU;
use std::sync::Arc;
use std::time::Duration;

use truce::prelude::*;
use truce_core::editor::RawWindowHandle;
use truce_gui::IntoLayoutEditor;
use truce_gui_types::layout::{GridLayout, knob, widgets};

use IdleAnimParamsParamId as P;

#[derive(Params)]
pub struct IdleAnimParams {
    #[param(
        name = "Gain",
        range = "linear(-60, 6)",
        unit = "dB",
        smooth = "exp(5)"
    )]
    pub gain: FloatParam,

    /// The animated value. Nothing in `process` touches it; the editor
    /// sweeps it from `idle`, so its knob moves only while `idle` fires.
    /// Written straight to the atomic (not via `set_param`), so the sweep
    /// doesn't flood the host with automation.
    #[param(name = "Sweep", range = "linear(0, 1)")]
    pub sweep: FloatParam,
}

/// Stateless descriptor - the DSP is a plain gain; the point is the editor.
pub struct IdleAnim;

impl PurePluginLogic for IdleAnim {
    type Params = IdleAnimParams;

    fn process(
        params: &IdleAnimParams,
        buffer: &mut AudioBuffer,
        _events: &EventList,
        _context: &mut ProcessContext,
    ) -> ProcessStatus {
        for i in 0..buffer.num_samples() {
            // `read` advances the smoother one step, so call it once per
            // sample - hoisting it out of the loop smooths at block rate
            // (gain changes crawl in ~block-size steps).
            let gain = db_to_linear(params.gain.read());
            for ch in 0..buffer.channels() {
                let (inp, out) = buffer.io(ch);
                out[i] = inp[i] * gain;
            }
        }
        ProcessStatus::Normal
    }

    fn editor(params: Arc<IdleAnimParams>) -> Box<dyn Editor> {
        let inner = GridLayout::build(vec![widgets(vec![
            knob(P::Gain, "Gain").at(0, 0),
            knob(P::Sweep, "idle()").at(1, 0),
        ])])
        .with_title("IDLE ANIM")
        .into_editor(&params);
        Box::new(IdleAnimEditor {
            inner,
            params,
            phase: 0.0,
        })
    }
}

/// Wraps the built-in editor, delegating every window / render concern to
/// it, and sweeps the `Sweep` value from [`Editor::idle`].
struct IdleAnimEditor {
    inner: Box<dyn Editor>,
    params: Arc<IdleAnimParams>,
    /// Sweep position in `[0, 1)`; advanced by the `dt` the host-UI tick
    /// hands us each `idle`.
    phase: f64,
}

impl Editor for IdleAnimEditor {
    fn size(&self) -> (u32, u32) {
        self.inner.size()
    }

    fn open(&mut self, parent: RawWindowHandle, context: PluginContext) {
        self.inner.open(parent, context);
    }

    fn close(&mut self) {
        self.inner.close();
    }

    fn idle(&mut self, dt: Duration) {
        // Advance by the framework-supplied `dt` (real elapsed time), so the
        // sweep runs at a fixed rate no matter how fast or unevenly the host
        // ticks `idle`. It still freezes if `idle` never fires - the point.
        const CYCLES_PER_SEC: f64 = 0.5;
        self.phase = (self.phase + dt.as_secs_f64() * CYCLES_PER_SEC).rem_euclid(1.0);
        let value = 0.5 - 0.5 * (self.phase * TAU).cos();
        self.params.sweep.set_value(value);
        self.inner.idle(dt);
    }

    fn set_size(&mut self, width: u32, height: u32) -> bool {
        self.inner.set_size(width, height)
    }

    fn can_resize(&self) -> bool {
        self.inner.can_resize()
    }

    fn min_size(&self) -> (u32, u32) {
        self.inner.min_size()
    }

    fn max_size(&self) -> (u32, u32) {
        self.inner.max_size()
    }

    fn aspect_ratio(&self) -> Option<(u32, u32)> {
        self.inner.aspect_ratio()
    }

    fn size_increment(&self) -> Option<(u32, u32)> {
        self.inner.size_increment()
    }

    fn prefers_pow2(&self) -> bool {
        self.inner.prefers_pow2()
    }

    fn can_maximize(&self) -> bool {
        self.inner.can_maximize()
    }

    fn set_scale_factor(&mut self, factor: f64) {
        self.inner.set_scale_factor(factor);
    }

    fn set_uses_system_scale(&mut self, yes: bool) {
        self.inner.set_uses_system_scale(yes);
    }

    fn state_changed(&mut self) {
        self.inner.state_changed();
    }

    fn screenshot(&mut self, params: Arc<dyn truce_params::Params>) -> Option<(Vec<u8>, u32, u32)> {
        self.inner.screenshot(params)
    }
}

truce::plugin! {
    logic: IdleAnim,
    params: IdleAnimParams,
}
