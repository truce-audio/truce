//! Regression test: param sync must NOT snap smoothers.
//! Bug: calling snap_smoothers() every block killed gradual smoothing,
//! causing zipper noise on param changes.

use truce_core::buffer::AudioBuffer;
use truce_core::events::{Event, EventBody, EventList, TransportInfo};
use truce_core::process::{ProcessContext, ProcessStatus};
use truce_core::plugin::Plugin;
use truce_params::Params;
#[allow(unused_imports)]
use truce_params_derive::Params;

#[derive(Params)]
struct SmootherParams {
    #[param(id = 0, name = "Gain", range = "linear(0, 1)", smooth = "exp(50)")]
    gain: truce_params::FloatParam,
}

struct SmootherPlugin {
    params: SmootherParams,
    samples: Vec<f32>,
}

impl truce_loader::PluginLogic for SmootherPlugin {
    fn new() -> Self {
        Self {
            params: SmootherParams::new(),
            samples: Vec::new(),
        }
    }

    fn params_mut(&mut self) -> Option<&mut dyn Params> {
        Some(&mut self.params)
    }

    fn reset(&mut self, sr: f64, _bs: usize) {
        self.params.set_sample_rate(sr);
    }

    fn process(
        &mut self,
        buffer: &mut AudioBuffer,
        _events: &EventList,
        _ctx: &mut ProcessContext,
    ) -> ProcessStatus {
        for i in 0..buffer.num_samples() {
            let g = self.params.gain.smoothed_next();
            self.samples.push(g);
            let (inp, out) = buffer.io_pair(0, 0);
            out[i] = inp[i] * g;
        }
        ProcessStatus::Normal
    }

    fn layout(&self) -> truce_gui::layout::GridLayout {
        truce_gui::layout::GridLayout::build("", "", 1, 80.0, vec![], vec![])
    }
}

#[test]
fn smoother_ramps_gradually() {
    let mut shell = truce_loader::static_shell::StaticShell::<SmootherParams, SmootherPlugin>::new(
        SmootherParams::new(),
    );
    shell.reset(44100.0, 64);

    // Set gain to 0.0 initially.
    let mut events = EventList::new();
    events.push(Event {
        sample_offset: 0,
        body: EventBody::ParamChange { id: 0, value: 0.0 },
    });

    let input = vec![1.0f32; 64];
    let mut output = vec![0.0f32; 64];
    let inputs: Vec<&[f32]> = vec![&input];
    let mut outputs: Vec<&mut [f32]> = vec![&mut output];
    let mut buffer = unsafe { AudioBuffer::from_slices(&inputs, &mut outputs, 64) };

    let transport = TransportInfo::default();
    let mut output_events = EventList::new();
    let param_fn = |_: u32| 0.0;
    let meter_fn = |_: u32, _: f32| {};
    let mut ctx = ProcessContext::new(&transport, 44100.0, 64, &mut output_events)
        .with_params(&param_fn)
        .with_meters(&meter_fn);

    // Process first block to settle at gain=0.0.
    shell.process(&mut buffer, &events, &mut ctx);

    // Now jump to gain=1.0.
    events.clear();
    events.push(Event {
        sample_offset: 0,
        body: EventBody::ParamChange { id: 0, value: 1.0 },
    });

    shell.logic_ref_mut().samples.clear();

    let mut output2 = vec![0.0f32; 64];
    let mut outputs2: Vec<&mut [f32]> = vec![&mut output2];
    let mut buffer2 = unsafe { AudioBuffer::from_slices(&inputs, &mut outputs2, 64) };
    let mut output_events2 = EventList::new();
    let mut ctx2 = ProcessContext::new(&transport, 44100.0, 64, &mut output_events2)
        .with_params(&param_fn)
        .with_meters(&meter_fn);

    shell.process(&mut buffer2, &events, &mut ctx2);

    let samples = &shell.logic_ref().samples;
    assert!(!samples.is_empty(), "should have recorded samples");

    // The first sample should NOT be 1.0 — it should be somewhere
    // between 0 and 1 (smoother hasn't reached target yet).
    let first = samples[0];
    let last = samples[samples.len() - 1];

    assert!(
        first < 0.9,
        "First sample {first} is too close to 1.0 — smoother was snapped instead of ramping"
    );
    assert!(
        last > first,
        "Smoother should be ramping up: first={first}, last={last}"
    );
}
