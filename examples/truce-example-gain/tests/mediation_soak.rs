//! Soak the audio/host contract: one thread runs audio blocks (the
//! write side, as every format wrapper's process callback does) while
//! another hammers a host session save (the read side). The save reads
//! the lock-free snapshot the audio thread publishes each block - it
//! never touches the plugin - so the audio thread owns the plugin
//! without contention. Asserts every block completes well under a
//! generous deadline and the saves land; a regression that put the save
//! back on the plugin lock would show as a blown deadline or the debug
//! overlap detector tripping.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::{Duration, Instant};

use truce_core::AudioConfig;
use truce_core::events::{EventList, TransportInfo};
use truce_core::export::PluginExport;
use truce_core::plugin::PluginRuntime;
use truce_core::process::ProcessContext;
use truce_core::wrapper::{enter_plugin, shared_plugin};
use truce_example_gain::Plugin;

const BLOCKS: usize = 2_000;
const BLOCK_SIZE: usize = 256;
// Far above any real contention stall; only a deadlock or a
// pathologically slow lock trips it.
const BLOCK_DEADLINE: Duration = Duration::from_secs(1);

#[test]
fn concurrent_save_never_wedges_the_audio_thread() {
    let mut plugin = Plugin::create();
    plugin.init();
    plugin.reset(&AudioConfig::new(48_000.0, BLOCK_SIZE));
    // The lock-free slot the audio thread publishes into and a host save
    // reads from - the same handle the format wrappers hold.
    let snapshot = plugin.snapshot_slot();
    let plugin = shared_plugin(plugin);

    let stop = Arc::new(AtomicBool::new(false));
    let saves = Arc::new(AtomicU32::new(0));

    let save_thread = {
        let snapshot = Arc::clone(&snapshot);
        let stop = Arc::clone(&stop);
        let saves = Arc::clone(&saves);
        std::thread::spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                // A host session save reads the snapshot, never the
                // plugin, so it can't contend the audio thread.
                let _ = snapshot.read();
                saves.fetch_add(1, Ordering::Relaxed);
            }
        })
    };

    let input = vec![vec![0.25_f32; BLOCK_SIZE]; 2];
    let mut output = vec![vec![0.0_f32; BLOCK_SIZE]; 2];
    let mut worst = Duration::ZERO;
    for _ in 0..BLOCKS {
        let started = Instant::now();
        {
            let mut guard = enter_plugin(&plugin);
            let inputs: Vec<&[f32]> = input.iter().map(Vec::as_slice).collect();
            let mut outputs: Vec<&mut [f32]> = output.iter_mut().map(Vec::as_mut_slice).collect();
            let mut buffer = truce_core::buffer::AudioBuffer::from_slices_checked(
                &inputs,
                &mut outputs,
                BLOCK_SIZE,
            );
            let events = EventList::with_capacity(4);
            let mut output_events = EventList::with_capacity(4);
            let transport = TransportInfo::default();
            let mut ctx = ProcessContext::new(&transport, 48_000.0, BLOCK_SIZE, &mut output_events);
            let _ = guard.process(&mut buffer, &events, &mut ctx);
        }
        let elapsed = started.elapsed();
        worst = worst.max(elapsed);
        assert!(
            elapsed < BLOCK_DEADLINE,
            "block took {elapsed:?} under concurrent save - audio thread stalled?"
        );
    }

    stop.store(true, Ordering::Relaxed);
    save_thread.join().expect("saver thread panicked");
    assert!(saves.load(Ordering::Relaxed) > 0, "saver thread never ran");
    // Not asserted (CI timing noise), but useful when run locally.
    eprintln!("worst block under save contention: {worst:?}");
}
