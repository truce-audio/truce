//! One-pole low-pass filter whose cutoff is driven by an incoming MIDI
//! CC (default CC74, the standard "brightness" controller).
//!
//! Exercises `midi_input = true` on an audio effect: it consumes MIDI
//! but emits none, and on AU it forces the component to register as an
//! `aumf` `MusicEffect` so the host actually routes MIDI to it (a plain
//! `aufx` never receives MIDI).

use truce::prelude::*;
use truce_gui::IntoLayoutEditor;
use truce_gui_types::layout::{GridLayout, knob, widgets};

use CcFilterParamsParamId as P;
use std::sync::Arc;

#[derive(Params)]
pub struct CcFilterParams {
    /// Cutoff used before any CC arrives, and the value the editor knob
    /// shows. Incoming CC overrides it.
    #[param(
        name = "Cutoff",
        range = "log(20, 20000)",
        default = 2000.0,
        unit = "Hz",
        smooth = "exp(10)",
        midi_cc = 74
    )]
    pub cutoff: FloatParam,

    #[param(
        name = "CC",
        short_name = "CC",
        range = "discrete(0, 127)",
        default = 74
    )]
    pub cc: IntParam,
}

/// Stateless descriptor - DSP state lives in [`CcFilterDspState`].
pub struct CcFilter;

/// Per-instance DSP state: the one-pole filter memory and the
/// CC-driven cutoff.
#[derive(DspState)]
pub struct CcFilterDspState {
    sample_rate: f64,
    /// One-pole state per channel.
    z1: [f64; 2],
    /// Cutoff in Hz, updated when a matching CC arrives.
    cutoff_hz: f64,
}

impl Default for CcFilterDspState {
    fn default() -> Self {
        Self {
            sample_rate: 44100.0,
            z1: [0.0; 2],
            cutoff_hz: 1000.0,
        }
    }
}

/// Map a 7-bit CC value to a cutoff in Hz on a log scale (20..20000),
/// matching the param's perceptual range.
fn cc_to_cutoff(value: u8) -> f64 {
    let t = f64::from(value) / 127.0;
    20.0 * (20_000.0_f64 / 20.0).powf(t)
}

/// One-pole low-pass coefficient for `cutoff` Hz at `sr`.
fn one_pole_a(cutoff: f64, sr: f64) -> f64 {
    let fc = cutoff.clamp(20.0, sr * 0.45);
    1.0 - (-std::f64::consts::TAU * fc / sr).exp()
}

impl PluginLogic for CcFilter {
    type Params = CcFilterParams;
    type DspState = CcFilterDspState;

    fn init(_params: &CcFilterParams) -> CcFilterDspState {
        CcFilterDspState::default()
    }

    fn bus_layouts() -> Vec<BusLayout> {
        vec![BusLayout::stereo()]
    }

    fn reset(state: &mut CcFilterDspState, params: &CcFilterParams, config: &AudioConfig) {
        let sample_rate = config.sample_rate;
        state.sample_rate = sample_rate;
        params.set_sample_rate(sample_rate);
        params.snap_smoothers();
        state.z1 = [0.0; 2];
        state.cutoff_hz = f64::from(params.cutoff.read());
    }

    fn process(
        state: &mut CcFilterDspState,
        params: &CcFilterParams,
        buffer: &mut AudioBuffer,
        events: &EventList,
        _context: &mut ProcessContext,
    ) -> ProcessStatus {
        // Listen for the configured CC and steer the cutoff from it.
        let target_cc = params.cc.value_u8();
        for event in events.iter() {
            if let EventBody::ControlChange { cc, value, .. } = &event.body
                && *cc == target_cc
            {
                state.cutoff_hz = cc_to_cutoff(*value);
            }
        }

        let a = one_pole_a(state.cutoff_hz, state.sample_rate);
        for ch in 0..buffer.channels().min(2) {
            let mut z = state.z1[ch];
            let (inp, out) = buffer.io(ch);
            for i in 0..inp.len() {
                z += a * (f64::from(inp[i]) - z);
                out[i] = filtered_f32(z);
            }
            state.z1[ch] = z;
        }

        ProcessStatus::Normal
    }

    fn editor(params: Arc<CcFilterParams>) -> Box<dyn Editor> {
        GridLayout::build(vec![widgets(vec![
            knob(P::Cutoff, "Cutoff"),
            knob(P::Cc, "CC"),
        ])])
        .with_title("CC FILTER")
        .into_editor(&params)
    }
}

/// Filter state as the f32 audio sample - the DSP output boundary.
#[allow(clippy::cast_possible_truncation)]
fn filtered_f32(z: f64) -> f32 {
    z as f32
}

truce::plugin! {
    logic: CcFilter,
    params: CcFilterParams,
}

truce::enable_rt_paranoid!();

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn process_is_allocation_free() {
        use std::time::Duration;
        use truce_test::{InputSource, assert_no_audio_alloc, driver};
        assert_no_audio_alloc(|| {
            driver!(Plugin)
                .duration(Duration::from_millis(40))
                .input(InputSource::Constant(0.25))
                .script(|s| {
                    s.set_param(P::Cutoff, 0.9);
                    s.cc(74, 0.5);
                    s.wait_ms(15);
                    s.set_param(P::Cutoff, 0.1);
                    s.wait_ms(15);
                })
                .run()
        });
    }

    #[test]
    fn info_is_valid() {
        truce_test::assert_valid_info::<Plugin>();
    }

    #[test]
    fn cutoff_declares_cc74_binding() {
        use truce::params::{MidiSource, Params, map_source_to_param};
        let infos = CcFilterParams::param_infos_static();
        // CC 74 (any channel) is declared as the cutoff's default
        // host-mapping; the resolver the wrappers use finds it.
        assert_eq!(
            map_source_to_param(&infos, 0, MidiSource::Cc(74)),
            Some(u32::from(P::Cutoff))
        );
        // An unbound source resolves to nothing.
        assert_eq!(map_source_to_param(&infos, 0, MidiSource::PitchBend), None);
    }

    #[test]
    fn effect_accepts_midi_input() {
        use truce_core::plugin::PluginRuntime;
        let info = <Plugin as PluginRuntime>::info();
        assert!(
            info.accepts_midi_in,
            "midi_input = true should set accepts_midi_in on an effect"
        );
        assert!(!info.emits_midi, "the filter emits no MIDI");
    }

    #[test]
    fn cc_steers_cutoff() {
        let params = CcFilterParams::new();
        let mut state = CcFilter::init(&params);
        CcFilter::reset(&mut state, &params, &AudioConfig::new(44100.0, 64));

        let before = state.cutoff_hz;
        let input = vec![vec![0.0f32; 64]; 2];
        let input_refs: Vec<&[f32]> = input.iter().map(std::vec::Vec::as_slice).collect();
        let mut output = vec![vec![0.0f32; 64]; 2];
        let mut output_refs: Vec<&mut [f32]> =
            output.iter_mut().map(std::vec::Vec::as_mut_slice).collect();
        let mut buffer = unsafe { AudioBuffer::from_slices(&input_refs, &mut output_refs, 64) };

        let mut events = EventList::default();
        events.push(Event::new(
            0,
            EventBody::ControlChange {
                group: 0,
                channel: 0,
                cc: 74,
                value: 0, // fully closed
            },
        ));

        let transport = TransportInfo::default();
        let mut output_events = EventList::default();
        let mut context = ProcessContext::new(&transport, 44100.0, 64, &mut output_events);
        CcFilter::process(&mut state, &params, &mut buffer, &events, &mut context);

        assert!(
            state.cutoff_hz < before,
            "CC74 = 0 should lower the cutoff (was {before}, now {})",
            state.cutoff_hz
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn gui_screenshot_macos() {
        truce_test::screenshot!(Plugin, "screenshots/cc_filter_default_macos.png").run();
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn gui_screenshot_linux() {
        truce_test::screenshot!(Plugin, "screenshots/cc_filter_default_linux.png")
            .pixel_threshold(2)
            .run();
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn gui_screenshot_windows() {
        truce_test::screenshot!(Plugin, "screenshots/cc_filter_default_windows.png")
            .pixel_threshold(2)
            .run();
    }
}
