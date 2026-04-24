//! Regression test: ParamChange events carry PLAIN values.
//! StaticShell must use set_plain(), not set_normalized().
//! Bug: double-denormalization caused gain to slam to extremes in VST3.

use truce_core::buffer::AudioBuffer;
use truce_core::events::{Event, EventBody, EventList, TransportInfo};
use truce_core::export::PluginExport;
use truce_core::plugin::Plugin;
use truce_core::process::{ProcessContext, ProcessStatus};
use truce_params::Params;
#[allow(unused_imports)]
use truce_params_derive::Params;

#[derive(Params)]
struct TestParams {
    #[param(id = 0, name = "Gain", range = "linear(-60, 6)", unit = "dB")]
    gain: truce_params::FloatParam,
}

struct TestPlugin {
    params: std::sync::Arc<TestParams>,
    last_gain_plain: f64,
}

impl TestPlugin {
    fn new(params: std::sync::Arc<TestParams>) -> Self {
        Self {
            params,
            last_gain_plain: 0.0,
        }
    }
}

impl truce_loader::PluginLogic for TestPlugin {
    fn reset(&mut self, sr: f64, _bs: usize) {
        self.params.set_sample_rate(sr);
    }

    fn process(
        &mut self,
        _buffer: &mut AudioBuffer,
        _events: &EventList,
        _ctx: &mut ProcessContext,
    ) -> ProcessStatus {
        // Record what the plugin sees as the gain value.
        self.last_gain_plain = self.params.gain.value() as f64;
        ProcessStatus::Normal
    }

    fn layout(&self) -> truce_gui::layout::GridLayout {
        truce_gui::layout::GridLayout::build("", "", 1, 80.0, vec![])
    }
}

#[test]
fn plain_param_not_double_denormalized() {
    // Simulate what format wrappers do: send a PLAIN value in ParamChange.
    // The shell must use set_plain, not set_normalized.
    // If it uses set_normalized, -27.0 dB would be treated as normalized
    // and denormalized to -60 + (-27 * 66) = way out of range.

    let params = std::sync::Arc::new(TestParams::new());
    let logic = TestPlugin::new(std::sync::Arc::clone(&params));
    let mut shell = truce_loader::static_shell::StaticShell::<TestParams, TestPlugin>::from_parts(
        params, logic,
    );
    shell.reset(44100.0, 512);

    let input = vec![0.5f32; 512];
    let mut output = vec![0.0f32; 512];
    let inputs: Vec<&[f32]> = vec![&input];
    let mut outputs: Vec<&mut [f32]> = vec![&mut output];
    let mut buffer = unsafe { AudioBuffer::from_slices(&inputs, &mut outputs, 512) };

    // ParamChange with PLAIN value -27.0 dB (this is what VST3/CLAP wrappers send).
    let mut events = EventList::new();
    events.push(Event {
        sample_offset: 0,
        body: EventBody::ParamChange {
            id: 0,
            value: -27.0,
        },
    });

    let transport = TransportInfo::default();
    let mut output_events = EventList::new();
    let param_fn = |_id: u32| -> f64 { 0.0 };
    let meter_fn = |_id: u32, _v: f32| {};
    let mut ctx = ProcessContext::new(&transport, 44100.0, 512, &mut output_events)
        .with_params(&param_fn)
        .with_meters(&meter_fn);

    shell.process(&mut buffer, &events, &mut ctx);

    // The plugin should see -27.0 dB (the plain value), NOT some
    // double-denormalized extreme.
    let gain = shell.logic_ref().last_gain_plain;
    assert!(
        (gain - (-27.0)).abs() < 0.1,
        "Expected gain ≈ -27.0 dB, got {gain}. Likely double-denormalization bug."
    );
}
