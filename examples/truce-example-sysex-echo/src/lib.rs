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

impl PurePluginLogic for SysexEcho {
    type Params = SysexEchoParams;

    fn bus_layouts() -> Vec<BusLayout> {
        // MIDI effect: no audio I/O.
        vec![BusLayout::new()]
    }

    fn process(
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
        use truce_test::BlockRunner;

        let params = SysexEchoParams::new();
        let payload = [0x7e, 0x00, 0x06, 0x01];
        // `with_capacity` reserves the SysEx byte pool; `default()`
        // reserves none, so `push_sysex` would report a full pool.
        let mut events = EventList::with_capacity(8);
        events.push_sysex(0, &payload).expect("pool has room");

        // MIDI effect: no audio buses, so pin an empty output shape.
        let out = BlockRunner::<SysexEcho>::new(&params)
            .outputs(0, 64)
            .run(&params, &[], &events);

        assert_eq!(out.sysex, vec![payload.to_vec()]);
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
