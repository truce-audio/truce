//! In-process standalone runner for integration tests.
//!
//! Runs the plugin's full lifecycle — instantiate → activate → feed
//! scripted MIDI → render audio blocks — entirely in memory. No
//! cpal, no window, no devices. Captures the output buffer + meter
//! readings for the test to assert on.
//!
//! This is what `truce-test` grows into: instead of hand-crafting
//! AudioBuffer / ProcessContext for every assertion, test authors
//! describe a scripted session and assert on the recorded audio.
//!
//! ```ignore
//! use truce_standalone::in_process::{InProcessOpts, MidiScript};
//! use std::time::Duration;
//!
//! let result = truce_standalone::in_process::run::<MyPlugin>(
//!     InProcessOpts::default()
//!         .sample_rate(48_000.0)
//!         .midi(|m: &mut MidiScript| {
//!             m.note_on(60, 0.8);
//!             m.wait_ms(100);
//!             m.note_off(60);
//!         })
//!         .duration(Duration::from_secs(1)),
//! );
//!
//! assert!(truce_test::is_nonzero(&result.output));
//! assert!(truce_test::is_silence_after(&result.output, Duration::from_millis(500)));
//! ```

use std::time::Duration;

use truce_core::buffer::AudioBuffer;
use truce_core::events::{Event, EventBody, EventList};
use truce_core::export::PluginExport;
use truce_core::info::PluginCategory;
use truce_core::process::ProcessContext;
use truce_params::Params;

use crate::transport::Transport;

/// Configuration for an in-process run.
pub struct InProcessOpts {
    sample_rate: f64,
    channels: usize,
    block_size: usize,
    duration: Duration,
    bpm: f64,
    playing: bool,
    script: MidiScript,
    input: Option<Vec<Vec<f32>>>,
}

impl Default for InProcessOpts {
    fn default() -> Self {
        Self {
            sample_rate: 44_100.0,
            channels: 2,
            block_size: 512,
            duration: Duration::from_secs(1),
            bpm: 120.0,
            playing: false,
            script: MidiScript::default(),
            input: None,
        }
    }
}

impl InProcessOpts {
    pub fn sample_rate(mut self, sr: f64) -> Self { self.sample_rate = sr; self }
    pub fn channels(mut self, ch: usize) -> Self { self.channels = ch; self }
    pub fn block_size(mut self, n: usize) -> Self { self.block_size = n; self }
    pub fn duration(mut self, d: Duration) -> Self { self.duration = d; self }
    pub fn bpm(mut self, bpm: f64) -> Self { self.bpm = bpm; self }
    pub fn playing(mut self, p: bool) -> Self { self.playing = p; self }

    /// Build a MIDI script using a closure. Script offsets are in
    /// the order events are declared; each `wait_ms` advances the
    /// cursor.
    pub fn midi(mut self, f: impl FnOnce(&mut MidiScript)) -> Self {
        f(&mut self.script);
        self
    }

    /// Feed an input audio buffer (effects only). Channel-major:
    /// `input[ch][frame]`. Panics at run time if the plugin is an
    /// instrument.
    pub fn input(mut self, input: Vec<Vec<f32>>) -> Self {
        self.input = Some(input);
        self
    }
}

/// A scripted sequence of MIDI events with sample-accurate timing.
#[derive(Default, Clone)]
pub struct MidiScript {
    /// `(sample_offset, body)` ordered by sample_offset.
    events: Vec<(usize, EventBody)>,
    cursor_samples: usize,
    sample_rate: f64,
}

impl MidiScript {
    pub fn note_on(&mut self, note: u8, velocity: f32) {
        self.events.push((
            self.cursor_samples,
            EventBody::NoteOn { channel: 0, note, velocity },
        ));
    }
    pub fn note_off(&mut self, note: u8) {
        self.events.push((
            self.cursor_samples,
            EventBody::NoteOff { channel: 0, note, velocity: 0.0 },
        ));
    }
    pub fn cc(&mut self, cc: u8, value: f32) {
        self.events.push((
            self.cursor_samples,
            EventBody::ControlChange { channel: 0, cc, value },
        ));
    }
    pub fn wait_ms(&mut self, ms: u64) {
        let sr = if self.sample_rate > 0.0 { self.sample_rate } else { 44_100.0 };
        self.cursor_samples += ((sr * ms as f64) / 1000.0) as usize;
    }
}

/// Captured audio + metadata from an in-process run.
pub struct RunResult {
    /// Channel-major output: `output[ch][frame]`.
    pub output: Vec<Vec<f32>>,
    pub sample_rate: f64,
    pub block_size: usize,
    pub total_frames: usize,
    /// Meter readings per slot at end-of-run (keyed by meter id).
    pub meters: Vec<(u32, f32)>,
}

/// Run the plugin headlessly for `opts.duration` and return the
/// captured output. No cpal, no window.
pub fn run<P: PluginExport>(mut opts: InProcessOpts) -> RunResult {
    // Wire the script's sample-rate before it's queried (so wait_ms
    // uses the run's SR, not the default).
    opts.script.sample_rate = opts.sample_rate;

    let mut plugin = P::create();
    plugin.init();
    plugin.reset(opts.sample_rate, opts.block_size);
    plugin.params().set_sample_rate(opts.sample_rate);
    plugin.params().snap_smoothers();

    let is_effect = P::info().category == PluginCategory::Effect;
    let total_frames =
        (opts.duration.as_secs_f64() * opts.sample_rate) as usize;

    let mut output: Vec<Vec<f32>> = (0..opts.channels)
        .map(|_| Vec::with_capacity(total_frames))
        .collect();

    // Pad input channels to the channel count we run at.
    let input_channels: Vec<Vec<f32>> = if is_effect {
        match opts.input.as_ref() {
            Some(bufs) => {
                assert_eq!(
                    bufs.len(),
                    opts.channels,
                    "InProcessOpts::input channel count doesn't match opts.channels"
                );
                bufs.clone()
            }
            None => vec![vec![0.0; total_frames]; opts.channels],
        }
    } else {
        Vec::new()
    };

    let transport = Transport::new(opts.bpm, opts.sample_rate);
    if opts.playing {
        transport.toggle_playing();
    }

    // Sort script by sample offset so the per-block filter is O(n).
    opts.script.events.sort_by_key(|(off, _)| *off);

    let mut cursor = 0usize;
    while cursor < total_frames {
        let block_len = opts.block_size.min(total_frames - cursor);

        // Pull MIDI events that fall inside [cursor, cursor+block_len).
        let mut event_list = EventList::new();
        for (off, body) in &opts.script.events {
            if *off >= cursor && *off < cursor + block_len {
                event_list.push(Event {
                    sample_offset: (*off - cursor) as u32,
                    body: body.clone(),
                });
            }
        }

        // Allocate per-block channel buffers.
        let mut out_bufs: Vec<Vec<f32>> =
            (0..opts.channels).map(|_| vec![0.0f32; block_len]).collect();

        let in_bufs_slice: Vec<Vec<f32>> = if is_effect {
            input_channels
                .iter()
                .map(|ch| ch[cursor..cursor + block_len].to_vec())
                .collect()
        } else {
            Vec::new()
        };
        let in_slices: Vec<&[f32]> = in_bufs_slice.iter().map(|b| b.as_slice()).collect();
        let mut out_slices: Vec<&mut [f32]> =
            out_bufs.iter_mut().map(|b| b.as_mut_slice()).collect();
        let mut audio =
            unsafe { AudioBuffer::from_slices(&in_slices, &mut out_slices, block_len) };

        let transport_info = transport.tick_audio(block_len);
        let mut output_events = EventList::new();
        let mut ctx = ProcessContext::new(
            &transport_info,
            opts.sample_rate,
            block_len,
            &mut output_events,
        );
        plugin.process(&mut audio, &event_list, &mut ctx);

        for (ch, buf) in out_bufs.into_iter().enumerate() {
            output[ch].extend_from_slice(&buf);
        }

        cursor += block_len;
    }

    // Capture final meter readings.
    let meter_ids: Vec<u32> = plugin
        .params()
        .meter_ids()
        .into_iter()
        .collect();
    let meters: Vec<(u32, f32)> = meter_ids
        .iter()
        .map(|id| (*id, plugin.get_meter(*id)))
        .collect();

    RunResult {
        output,
        sample_rate: opts.sample_rate,
        block_size: opts.block_size,
        total_frames,
        meters,
    }
}
