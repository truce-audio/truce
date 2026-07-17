//! Regression guard: sub-blocks created by a `CHUNKED` param split see a
//! playhead advanced to their start sample, not the block-start position.
//!
//! A tempo-synced plugin that re-derives phase from `context.transport`
//! each `process()` call would otherwise get up-to-a-block of timing
//! jitter exactly when automation lands (the split point).

use truce_core::buffer::AudioBuffer;
use truce_core::chunked_process::{ChunkedProcess, process_chunked};
use truce_core::config::{AudioConfig, ProcessMode};
use truce_core::events::{EVENT_LIST_PREALLOC, Event, EventBody, EventList, TransportInfo};
use truce_core::info::PluginInfo;
use truce_core::plugin::PluginRuntime;
use truce_core::process::{ProcessContext, ProcessStatus};
use truce_derive::Params;
use truce_params::{FloatParam, ParamFlags, Params};

#[derive(Params)]
struct GainParams {
    // Automatable params are CHUNKED by default, so a change to this one
    // splits the block.
    #[param(id = 0, name = "Gain", range = "linear(0, 1)")]
    gain: FloatParam,
}

/// Records the transport position and sub-block length each `process()`
/// call sees.
struct RecordPlugin {
    seen: Vec<(i64, usize)>,
}

impl PluginRuntime for RecordPlugin {
    type Sample = f32;

    fn info() -> PluginInfo {
        unimplemented!("process_chunked never queries info()")
    }

    fn reset(&mut self, _config: &AudioConfig) {}

    fn process(
        &mut self,
        buffer: &mut AudioBuffer,
        _events: &EventList,
        context: &mut ProcessContext,
    ) -> ProcessStatus {
        self.seen
            .push((context.transport.position_samples, buffer.num_samples()));
        ProcessStatus::Normal
    }
}

#[test]
fn chunked_split_advances_sub_block_playhead() {
    let params = GainParams::new();
    let param_infos = params.param_infos();
    assert!(
        param_infos[0].flags.contains(ParamFlags::CHUNKED),
        "the split relies on the param being chunked by default",
    );

    let mut plugin = RecordPlugin { seen: Vec::new() };

    let input = vec![0.0f32; 512];
    let mut output = vec![0.0f32; 512];
    let inputs: [&[f32]; 1] = [&input];
    let mut outputs: [&mut [f32]; 1] = [&mut output];
    let mut buffer = AudioBuffer::<f32>::from_slices_checked(&inputs, &mut outputs, 512);

    // One chunked param change mid-block splits [0, 512) at 400.
    let mut events = EventList::with_capacity(EVENT_LIST_PREALLOC);
    events.push(Event::new(
        400,
        EventBody::ParamChange { id: 0, value: 0.5 },
    ));

    let mut scratch = EventList::with_capacity(EVENT_LIST_PREALLOC);
    let mut out_events = EventList::with_capacity(EVENT_LIST_PREALLOC);
    // Playing, block starts at sample 1000, 120 BPM at 48 kHz.
    let mut transport = TransportInfo {
        playing: true,
        tempo: 120.0,
        position_samples: 1_000,
        ..TransportInfo::default()
    };

    let args = ChunkedProcess {
        events: &events,
        sub_event_scratch: &mut scratch,
        transport: &mut transport,
        sample_rate: 48_000.0,
        process_mode: ProcessMode::Realtime,
        output_events: &mut out_events,
        params_fn: None,
        meters_fn: None,
        param_infos: &param_infos,
        min_subblock_samples: 1,
    };
    process_chunked(&mut plugin, &params as &dyn Params, &mut buffer, args);

    // [0, 400): still at 1000. [400, 512): advanced to 1400.
    assert_eq!(
        plugin.seen,
        vec![(1_000, 400), (1_400, 112)],
        "second sub-block must see the playhead advanced by 400 samples",
    );
}
