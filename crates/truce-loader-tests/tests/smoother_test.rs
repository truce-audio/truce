//! Regression test: param sync must NOT snap smoothers.
//! Bug: calling `snap_smoothers()` every block killed gradual smoothing,
//! causing zipper noise on param changes.

use std::sync::Arc;
use truce_core::AudioConfig;
use truce_core::buffer::AudioBuffer;
use truce_core::events::{Event, EventBody, EventList, TransportInfo};
use truce_core::plugin::PluginRuntime;
use truce_core::process::{ProcessContext, ProcessStatus};
use truce_derive::{DspState, Params};
use truce_gui::PluginLogic;
use truce_params::{FloatParamReadF32, Params};

#[derive(Params)]
struct SmootherParams {
    #[param(id = 0, name = "Gain", range = "linear(0, 1)", smooth = "exp(50)")]
    gain: truce_params::FloatParam,
}

struct SmootherPlugin;

#[derive(DspState)]
struct SmootherDspState {
    samples: Vec<f32>,
}

impl PluginLogic for SmootherPlugin {
    type Params = SmootherParams;
    type DspState = SmootherDspState;

    fn init(_params: &SmootherParams) -> SmootherDspState {
        SmootherDspState {
            samples: Vec::new(),
        }
    }

    fn reset(_state: &mut SmootherDspState, params: &SmootherParams, config: &AudioConfig) {
        params.set_sample_rate(config.sample_rate);
    }

    fn process(
        state: &mut SmootherDspState,
        params: &SmootherParams,
        buffer: &mut AudioBuffer,
        _events: &EventList,
        _ctx: &mut ProcessContext,
    ) -> ProcessStatus {
        for i in 0..buffer.num_samples() {
            let g = params.gain.read();
            state.samples.push(g);
            let (inp, out) = buffer.io_pair(0, 0);
            out[i] = inp[i] * g;
        }
        ProcessStatus::Normal
    }

    fn editor(_params: Arc<SmootherParams>) -> Box<dyn truce::prelude::Editor> {
        // DSP-only test; the editor slot is never exercised. Return
        // a stub so the trait requirement is satisfied.
        Box::new(NoEditor)
    }
}

struct NoEditor;
impl truce::prelude::Editor for NoEditor {
    fn size(&self) -> (u32, u32) {
        (0, 0)
    }
    fn open(&mut self, _: truce_core::editor::RawWindowHandle, _: truce::prelude::PluginContext) {}
    fn close(&mut self) {}
    fn idle(&mut self) {}
}

#[test]
fn smoother_ramps_gradually() {
    let params = Arc::new(SmootherParams::new());
    let mut shell =
        truce_loader::static_shell::StaticShell::<SmootherParams, SmootherPlugin>::from_parts(
            params,
        );
    shell.reset(&AudioConfig::new(44100.0, 64));

    // Set gain to 0.0 initially.
    let mut events = EventList::default();
    events.push(Event::new(0, EventBody::ParamChange { id: 0, value: 0.0 }));

    let input = vec![1.0f32; 64];
    let mut output = vec![0.0f32; 64];
    let inputs: Vec<&[f32]> = vec![&input];
    let mut outputs: Vec<&mut [f32]> = vec![&mut output];
    let mut buffer = unsafe { AudioBuffer::from_slices(&inputs, &mut outputs, 64) };

    let transport = TransportInfo::default();
    let mut output_events = EventList::default();
    let param_fn = |_: u32| 0.0;
    let meter_fn = |_: u32, _: f32| {};
    let mut ctx = ProcessContext::new(&transport, 44100.0, 64, &mut output_events)
        .with_params(&param_fn)
        .with_meters(&meter_fn);

    // Process first block to settle at gain=0.0.
    shell.process(&mut buffer, &events, &mut ctx);

    // Now jump to gain=1.0.
    events.clear();
    events.push(Event::new(0, EventBody::ParamChange { id: 0, value: 1.0 }));

    shell.state_ref_mut().samples.clear();

    let mut second_output = vec![0.0f32; 64];
    let mut outputs2: Vec<&mut [f32]> = vec![&mut second_output];
    let mut buffer2 = unsafe { AudioBuffer::from_slices(&inputs, &mut outputs2, 64) };
    let mut output_events2 = EventList::default();
    let mut ctx2 = ProcessContext::new(&transport, 44100.0, 64, &mut output_events2)
        .with_params(&param_fn)
        .with_meters(&meter_fn);

    shell.process(&mut buffer2, &events, &mut ctx2);

    let samples = &shell.state_ref().samples;
    assert!(!samples.is_empty(), "should have recorded samples");

    // The first sample should NOT be 1.0 - it should be somewhere
    // between 0 and 1 (smoother hasn't reached target yet).
    let first = samples[0];
    let last = samples[samples.len() - 1];

    assert!(
        first < 0.9,
        "First sample {first} is too close to 1.0 - smoother was snapped instead of ramping"
    );
    assert!(
        last > first,
        "Smoother should be ramping up: first={first}, last={last}"
    );
}
