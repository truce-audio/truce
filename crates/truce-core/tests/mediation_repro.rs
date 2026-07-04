//! Distilled two-thread repro of the wrapper mediation contract,
//! sized to run under Miri's data-race detector (small blocks, few
//! iterations; `-Zmiri-many-seeds` varies the schedule).
//!
//! One thread plays the audio side: lock the `SharedPlugin` per
//! block, mutate plain (non-atomic) plugin fields from `process()`,
//! publish a meter. The other plays the GUI/host side: read the
//! `MeterStore` and `try_lock` + `save_state` - the exact accesses
//! that used to go through a raw pointer to the instance while the
//! audio thread held `&mut`, which is what Miri would flag if anyone
//! reintroduces that shortcut.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use truce_core::buffer::AudioBuffer;
use truce_core::events::{EventList, TransportInfo};
use truce_core::info::{AutomationConfig, MidiDialect, PluginCategory, PluginInfo};
use truce_core::meters::MeterStore;
use truce_core::plugin::PluginRuntime;
use truce_core::process::{ProcessContext, ProcessStatus};
use truce_core::wrapper::{lock_plugin, shared_plugin, try_lock_plugin};
use truce_params::METER_ID_BASE;

const BLOCK: usize = 8;
#[cfg(miri)]
const BLOCKS: usize = 40;
#[cfg(not(miri))]
const BLOCKS: usize = 2_000;

/// Deliberately worst-case plugin: plain fields written every block
/// and read back by `save_state`. Sound only because the lock makes
/// the two sides mutually exclusive.
struct RacyByDesign {
    frames: u64,
    blob: Vec<u8>,
}

impl PluginRuntime for RacyByDesign {
    type Sample = f32;

    fn info() -> PluginInfo {
        PluginInfo {
            name: "Mediation Repro",
            vendor: "Truce",
            url: "",
            version: "0.0.0",
            category: PluginCategory::Effect,
            accepts_midi_in: false,
            emits_midi: false,
            midi_input_dialect: MidiDialect::Midi1,
            midi_output_dialect: MidiDialect::Midi1,
            midi_input_ports: 0,
            midi_output_ports: 0,
            bundle_id: "mediation-repro",
            vst3_id: "",
            clap_id: "",
            fourcc: *b"Test",
            au_type: *b"aufx",
            au_manufacturer: *b"Trce",
            aax_id: None,
            aax_category: None,
            vst3_subcategory: None,
            preset_user_dir: None,
            vst3_name: None,
            clap_name: None,
            vst2_name: None,
            au_name: None,
            au3_name: None,
            aax_name: None,
            lv2_name: None,
            mute_preview_output: false,
            automation: AutomationConfig::DEFAULT,
            legacy_au_keys: &[],
            legacy_lv2_uris: &[],
            legacy_aax_chunk_ids: &[],
        }
    }

    fn reset(&mut self, _sample_rate: f64, _max_block_size: usize) {}

    fn process(
        &mut self,
        buffer: &mut AudioBuffer<f32>,
        _events: &EventList,
        _context: &mut ProcessContext,
    ) -> ProcessStatus {
        self.frames += u64::try_from(buffer.num_samples()).unwrap_or(0);
        // Non-atomic write that `save_state` reads: the author trap
        // the mediation lock exists to make safe.
        self.blob.clear();
        self.blob.extend_from_slice(&self.frames.to_le_bytes());
        ProcessStatus::Normal
    }

    fn save_state(&self) -> Vec<u8> {
        self.blob.clone()
    }
}

#[test]
fn locked_process_races_mediated_readers_cleanly() {
    let plugin = shared_plugin(RacyByDesign {
        frames: 0,
        blob: Vec::new(),
    });
    let meters = MeterStore::new();
    let stop = Arc::new(AtomicBool::new(false));
    let saves = Arc::new(AtomicU32::new(0));

    let reader = {
        let plugin = Arc::clone(&plugin);
        let meters = Arc::clone(&meters);
        let stop = Arc::clone(&stop);
        let saves = Arc::clone(&saves);
        std::thread::spawn(move || {
            let mut last_meter = 0.0f32;
            while !stop.load(Ordering::Relaxed) {
                last_meter = meters.read(METER_ID_BASE);
                if let Some(guard) = try_lock_plugin(&plugin) {
                    let blob = guard.save_state();
                    // A torn read would show a length the writer
                    // never produces.
                    assert!(blob.is_empty() || blob.len() == 8);
                    saves.fetch_add(1, Ordering::Relaxed);
                }
                std::thread::yield_now();
            }
            last_meter
        })
    };

    let input = [[0.5f32; BLOCK]];
    let mut output = [[0.0f32; BLOCK]];
    for block in 0..BLOCKS {
        let mut guard = lock_plugin(&plugin);
        let inputs: [&[f32]; 1] = [&input[0]];
        let mut out0: &mut [f32] = &mut output[0];
        let outputs = std::slice::from_mut(&mut out0);
        let mut buffer = AudioBuffer::from_slices_checked(&inputs, outputs, BLOCK);
        let events = EventList::with_capacity(2);
        let mut output_events = EventList::with_capacity(2);
        let transport = TransportInfo::default();
        let mut ctx = ProcessContext::new(&transport, 48_000.0, BLOCK, &mut output_events);
        let _ = guard.process(&mut buffer, &events, &mut ctx);
        // Publish a meter from "inside the block", as the shells'
        // meter callback does.
        #[allow(clippy::cast_precision_loss)]
        meters.write(METER_ID_BASE, block as f32);
    }

    // Keep the reader running until it has won the lock at least
    // once - the tiny native block loop can finish before the OS
    // even schedules the reader thread, and the point here is racing
    // schedules, not racing thread startup.
    while saves.load(Ordering::Relaxed) == 0 {
        std::thread::yield_now();
    }
    stop.store(true, Ordering::Relaxed);
    let _last_meter = reader.join().expect("reader panicked");

    let final_frames = u64::from_le_bytes(
        lock_plugin(&plugin)
            .save_state()
            .try_into()
            .expect("8-byte blob"),
    );
    assert_eq!(final_frames, (BLOCKS * BLOCK) as u64);
}
