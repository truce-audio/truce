//! Echoes every received `SysEx` message back to the host unchanged.
//!
//! Exercises the plugin-to-host `SysEx` output path and the per-format
//! `0xF0..0xF7` framing - the variable-length payload travels through
//! the `EventList` byte pool, separate from the fixed channel-voice
//! events.

use truce::prelude::*;
use truce_gui::IntoLayoutEditor;
use truce_gui_types::layout::{GridLayout, toggle, widgets};

use SysexEchoParamsParamId as P;
use std::sync::Arc;

#[derive(Params)]
pub struct SysexEchoParams {
    #[param(name = "Enabled", default = 1)]
    pub enabled: BoolParam,
}

/// Stateless descriptor - the echo carries no DSP state, only params.
pub struct SysexEcho;

impl PluginLogic for SysexEcho {
    type Params = SysexEchoParams;
    type DspState = ();

    fn init(_params: &SysexEchoParams) {}

    fn bus_layouts() -> Vec<BusLayout> {
        // MIDI effect: no audio I/O.
        vec![BusLayout::new()]
    }

    fn reset(_state: &mut (), params: &SysexEchoParams, config: &AudioConfig) {
        let sample_rate = config.sample_rate;
        params.set_sample_rate(sample_rate);
        params.snap_smoothers();
    }

    fn process(
        _state: &mut (),
        params: &SysexEchoParams,
        _buffer: &mut AudioBuffer,
        events: &EventList,
        context: &mut ProcessContext,
    ) -> ProcessStatus {
        if !params.enabled.value() {
            return ProcessStatus::Normal;
        }

        for event in events.iter() {
            if let EventBody::SysEx { .. } = &event.body {
                let bytes = events.sysex_bytes(&event.body);
                // Drop on a full pool rather than truncate - a partial
                // SysEx is invalid.
                let _ = context.output_events.push_sysex(event.sample_offset, bytes);
            }
        }

        ProcessStatus::Normal
    }

    fn editor(params: Arc<SysexEchoParams>) -> Box<dyn Editor> {
        GridLayout::build(vec![widgets(vec![toggle(P::Enabled, "Enabled")])])
            .with_title("SYSEX")
            .into_editor(&params)
    }
}

truce::plugin! {
    logic: SysexEcho,
    params: SysexEchoParams,
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
                    s.sysex(&[0xF0, 0x7D, 0x01, 0xF7]);
                    s.note_on(60, 0.8);
                    s.set_param(P::Enabled, 0.9);
                    s.wait_ms(15);
                    s.set_param(P::Enabled, 0.1);
                    s.wait_ms(15);
                    s.note_off(60);
                })
                .run()
        });
    }

    #[test]
    fn info_is_valid() {
        truce_test::assert_valid_info::<Plugin>();
    }

    #[test]
    fn echoes_sysex_payload() {
        let params = SysexEchoParams::new();
        SysexEcho::init(&params);
        SysexEcho::reset(&mut (), &params, &AudioConfig::new(44100.0, 64));

        let input: Vec<Vec<f32>> = Vec::new();
        let input_refs: Vec<&[f32]> = input.iter().map(std::vec::Vec::as_slice).collect();
        let mut output: Vec<Vec<f32>> = Vec::new();
        let mut output_refs: Vec<&mut [f32]> =
            output.iter_mut().map(std::vec::Vec::as_mut_slice).collect();
        let mut buffer = unsafe { AudioBuffer::from_slices(&input_refs, &mut output_refs, 64) };

        let payload = [0x7e, 0x00, 0x06, 0x01];
        // `with_capacity` reserves the SysEx byte pool; `default()`
        // reserves none, so `push_sysex` would report a full pool.
        let mut events = EventList::with_capacity(8);
        events.push_sysex(0, &payload).expect("pool has room");

        let transport = TransportInfo::default();
        let mut output_events = EventList::with_capacity(8);
        let mut context = ProcessContext::new(&transport, 44100.0, 64, &mut output_events);
        SysexEcho::process(&mut (), &params, &mut buffer, &events, &mut context);

        let echoed: Vec<&[u8]> = output_events
            .iter()
            .filter(|e| matches!(e.body, EventBody::SysEx { .. }))
            .map(|e| output_events.sysex_bytes(&e.body))
            .collect();
        assert_eq!(echoed, vec![&payload[..]]);
    }

    // Integration test through the real sample-accurate chunking path:
    // a Transport event mid-block forces a sub-block split, so the input
    // SysEx is delivered via the chunker's rebased scratch list - the
    // path that previously dropped (or, with an unsized pool, panicked
    // on) the payload. Direct `process()` tests bypass this entirely.
    #[test]
    fn sysex_survives_chunk_split() {
        use std::time::Duration;
        use truce_test::driver;

        let payload = [0x7e, 0x00, 0x06, 0x01];
        let result = driver!(Plugin)
            .duration(Duration::from_millis(5))
            .capture_output_events(true)
            .script(|s| {
                s.sysex(&payload);
                s.wait_samples(64);
                s.raw(EventBody::Transport(TransportInfo::default()));
            })
            .run();

        assert_eq!(result.output_sysex, vec![payload.to_vec()]);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn gui_screenshot_macos() {
        truce_test::screenshot!(Plugin, "screenshots/sysex_echo_default_macos.png").run();
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn gui_screenshot_linux() {
        truce_test::screenshot!(Plugin, "screenshots/sysex_echo_default_linux.png")
            .pixel_threshold(2)
            .run();
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn gui_screenshot_windows() {
        truce_test::screenshot!(Plugin, "screenshots/sysex_echo_default_windows.png")
            .pixel_threshold(2)
            .run();
    }
}
