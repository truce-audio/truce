//! Spreads incoming notes round-robin across N MIDI channels - a
//! minimal MPE-style voice allocator.
//!
//! Exercises multi-channel MIDI output (up to all 16 channels), the
//! `status | channel` nibble packing in every wrapper, and higher
//! event counts against `EVENT_LIST_PREALLOC`.

use truce::prelude::*;
use truce_gui::IntoLayoutEditor;
use truce_gui_types::layout::{GridLayout, knob, widgets};

use SpreaderParamsParamId as P;
use std::sync::Arc;

#[derive(Params)]
pub struct SpreaderParams {
    #[param(
        name = "Channels",
        short_name = "Chans",
        range = "discrete(1, 16)",
        default = 16
    )]
    pub channels: IntParam,
}

pub struct Spreader {
    params: Arc<SpreaderParams>,
    /// Channel each held input note was assigned, so its `NoteOff` lands
    /// on the same channel even if the spread width changes mid-hold.
    note_channel: [Option<u8>; 128],
    /// Next channel to hand out.
    next: u8,
}

impl Spreader {
    pub fn new(params: Arc<SpreaderParams>) -> Self {
        Self {
            params,
            note_channel: [None; 128],
            next: 0,
        }
    }
}

impl PluginLogic for Spreader {
    fn bus_layouts() -> Vec<BusLayout> {
        // MIDI effect: no audio I/O.
        vec![BusLayout::new()]
    }

    fn reset(&mut self, sample_rate: f64, _max_block_size: usize) {
        self.params.set_sample_rate(sample_rate);
        self.params.snap_smoothers();
        self.note_channel = [None; 128];
        self.next = 0;
    }

    fn process(
        &mut self,
        _buffer: &mut AudioBuffer,
        events: &EventList,
        context: &mut ProcessContext,
    ) -> ProcessStatus {
        let width = self.params.channels.value_u8().clamp(1, 16);

        for event in events.iter() {
            match &event.body {
                EventBody::NoteOn {
                    group,
                    note,
                    velocity,
                    ..
                } => {
                    let channel = self.next % width;
                    self.next = (self.next + 1) % width;
                    self.note_channel[*note as usize] = Some(channel);
                    context.output_events.push(Event::new(
                        event.sample_offset,
                        EventBody::NoteOn {
                            group: *group,
                            channel,
                            note: *note,
                            velocity: *velocity,
                        },
                    ));
                }
                EventBody::NoteOff {
                    group,
                    note,
                    velocity,
                    ..
                } => {
                    let channel = self.note_channel[*note as usize].take().unwrap_or(0);
                    context.output_events.push(Event::new(
                        event.sample_offset,
                        EventBody::NoteOff {
                            group: *group,
                            channel,
                            note: *note,
                            velocity: *velocity,
                        },
                    ));
                }
                _ => {}
            }
        }

        ProcessStatus::Normal
    }

    fn editor(&self) -> Box<dyn Editor> {
        GridLayout::build(vec![widgets(vec![knob(P::Channels, "Channels")])])
            .with_title("SPREAD")
            .into_editor(&self.params)
    }
}

truce::plugin! {
    logic: Spreader,
    params: SpreaderParams,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn info_is_valid() {
        truce_test::assert_valid_info::<Plugin>();
    }

    #[test]
    fn spreads_notes_across_channels() {
        let params = Arc::new(SpreaderParams::new());
        let mut plugin = Spreader::new(Arc::clone(&params));
        plugin.params.channels.set_value(4);
        plugin.reset(44100.0, 64);

        let input: Vec<Vec<f32>> = Vec::new();
        let input_refs: Vec<&[f32]> = input.iter().map(std::vec::Vec::as_slice).collect();
        let mut output: Vec<Vec<f32>> = Vec::new();
        let mut output_refs: Vec<&mut [f32]> =
            output.iter_mut().map(std::vec::Vec::as_mut_slice).collect();
        let mut buffer = unsafe { AudioBuffer::from_slices(&input_refs, &mut output_refs, 64) };

        let mut events = EventList::default();
        for note in 60..64u8 {
            events.push(Event::new(
                0,
                EventBody::NoteOn {
                    group: 0,
                    channel: 0,
                    note,
                    velocity: 100,
                },
            ));
        }

        let transport = TransportInfo::default();
        let mut output_events = EventList::default();
        let mut context = ProcessContext::new(&transport, 44100.0, 64, &mut output_events);
        plugin.process(&mut buffer, &events, &mut context);

        let channels: Vec<u8> = output_events
            .iter()
            .filter_map(|e| match e.body {
                EventBody::NoteOn { channel, .. } => Some(channel),
                _ => None,
            })
            .collect();
        // Four notes over a width of 4 → channels 0,1,2,3.
        assert_eq!(channels, vec![0, 1, 2, 3]);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn gui_screenshot_macos() {
        truce_test::screenshot!(Plugin, "screenshots/mpe_spreader_default_macos.png").run();
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn gui_screenshot_linux() {
        truce_test::screenshot!(Plugin, "screenshots/mpe_spreader_default_linux.png")
            .pixel_threshold(2)
            .run();
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn gui_screenshot_windows() {
        truce_test::screenshot!(Plugin, "screenshots/mpe_spreader_default_windows.png")
            .pixel_threshold(2)
            .run();
    }
}
