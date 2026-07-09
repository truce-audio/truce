//! Regression tests: smoother snap discipline in the shells.
//! Bug: calling `snap_smoothers()` every block killed gradual smoothing,
//! causing zipper noise on param changes. Snapping belongs to `reset`
//! (and state loads), and it is the SHELL's job - user `reset` bodies
//! carry no params plumbing (`SmootherPlugin` below has no `reset` at
//! all; it relies on the trait's default no-op).

use std::sync::Arc;
use truce_core::AudioConfig;
use truce_core::buffer::AudioBuffer;
use truce_core::events::{Event, EventBody, EventList, TransportInfo};
use truce_core::plugin::PluginRuntime;
use truce_core::process::{ProcessContext, ProcessStatus};
use truce_derive::Params;
use truce_gui::PluginLogic;
use truce_params::{FloatParamReadF32, Params};

#[derive(Params)]
struct SmootherParams {
    #[param(id = 0, name = "Gain", range = "linear(0, 1)", smooth = "exp(50)")]
    gain: truce_params::FloatParam,
}

struct SmootherPlugin;

#[derive(Default)]
struct SmootherDspState {
    samples: Vec<f32>,
}

impl PluginLogic for SmootherPlugin {
    type Params = SmootherParams;
    type DspState = SmootherDspState;

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

/// `StaticShell::reset` owns the params plumbing: it must snap smoothers
/// so the first post-reset block starts at the target instead of ramping
/// from stale pre-reset state.
#[test]
fn static_shell_reset_snaps_smoothers() {
    let params = Arc::new(SmootherParams::new());
    let mut shell =
        truce_loader::static_shell::StaticShell::<SmootherParams, SmootherPlugin>::from_parts(
            Arc::clone(&params),
        );

    // Target away from both the default and the smoother's current
    // value, then reset.
    params.set_plain(0, 0.75);
    shell.reset(&AudioConfig::new(44100.0, 64));

    let g = params.gain.read();
    assert!(
        (g - 0.75).abs() < 1e-3,
        "reset should snap the smoother to its target (0.75); got {g}"
    );
}

/// HotShell coverage - needs the fixture dylib, so it lives behind the
/// same `shell` feature as the other dylib-loading tests. Both tests
/// observe the shell's own `Arc<FxParams>` directly: the fixture's
/// `process` never reads the gain param, so any movement of the smoother
/// is the shell's doing.
#[cfg(feature = "shell")]
mod hot_shell {
    use std::path::PathBuf;
    use std::sync::Arc;

    use reload_fixture_common::FxParams;
    use truce_core::AudioConfig;
    use truce_core::buffer::AudioBuffer;
    use truce_core::events::{Event, EventBody, EventList, TransportInfo};
    use truce_core::plugin::PluginRuntime;
    use truce_core::process::ProcessContext;
    use truce_loader::shell::HotShell;
    use truce_params::{FloatParamReadF32, Params};

    fn dylib_path() -> PathBuf {
        let mut path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        path.pop(); // crates/
        path.pop(); // workspace root
        path.push("target");
        path.push("debug");

        #[cfg(target_os = "macos")]
        path.push("libreload_fixture_keep_a.dylib");
        #[cfg(target_os = "linux")]
        path.push("libreload_fixture_keep_a.so");
        #[cfg(target_os = "windows")]
        path.push("reload_fixture_keep_a.dll");

        path
    }

    fn shell_or_skip() -> Option<HotShell<FxParams>> {
        let path = dylib_path();
        if !path.exists() {
            eprintln!("skipping: fixture dylib not found at {}", path.display());
            eprintln!("build it first: cargo build --workspace");
            return None;
        }
        Some(HotShell::new(FxParams::new(), path))
    }

    /// Regression for the hot-shell variant of the every-block snap bug:
    /// a `ParamChange` event through `process` must set the smoother's
    /// TARGET and leave the ramp to run, not snap current to target.
    #[test]
    fn hot_shell_process_does_not_snap_smoothers() {
        let Some(mut shell) = shell_or_skip() else {
            return;
        };
        let params = Arc::clone(&shell.params);
        shell.reset(&AudioConfig::new(44100.0, 64));

        // Pin the smoother at 0.0, then jump the target to 1.0 via a
        // process-delivered event.
        params.set_plain(0, 0.0);
        params.snap_smoothers();

        let mut events = EventList::default();
        events.push(Event::new(0, EventBody::ParamChange { id: 0, value: 1.0 }));

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

        shell.process(&mut buffer, &events, &mut ctx);

        // One post-block read: a ramp from 0.0 with exp(50ms) at 44.1kHz
        // has barely moved; a snap would return exactly 1.0.
        let g = params.gain.read();
        assert!(
            g < 0.9,
            "process snapped the smoother instead of ramping; got {g}"
        );
    }

    /// `HotShell::reset` owns the same params plumbing as the static
    /// shell: snap on reset, before the loader lock is even attempted.
    #[test]
    fn hot_shell_reset_snaps_smoothers() {
        let Some(mut shell) = shell_or_skip() else {
            return;
        };
        let params = Arc::clone(&shell.params);

        params.set_plain(0, 0.75);
        shell.reset(&AudioConfig::new(44100.0, 64));

        let g = params.gain.read();
        assert!(
            (g - 0.75).abs() < 1e-3,
            "reset should snap the smoother to its target (0.75); got {g}"
        );
    }
}
