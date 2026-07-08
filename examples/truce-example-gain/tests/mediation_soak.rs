//! Soak the wrapper-standard plugin lock: one thread runs audio
//! blocks (the write side, as every format wrapper's process
//! callback does) while another hammers `save_state` (the read side,
//! as a host session save does). Asserts the bounded-stall contract:
//! every block completes, saves land, and no lock acquisition
//! wedges - a deadlock here would hang the test past its timeout.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::{Duration, Instant};

use truce_core::AudioConfig;
use truce_core::events::{EventList, TransportInfo};
use truce_core::export::PluginExport;
use truce_core::plugin::PluginRuntime;
use truce_core::process::ProcessContext;
use truce_core::wrapper::{lock_plugin, shared_plugin};
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
    let plugin = shared_plugin(plugin);

    let stop = Arc::new(AtomicBool::new(false));
    let saves = Arc::new(AtomicU32::new(0));

    let save_thread = {
        let plugin = Arc::clone(&plugin);
        let stop = Arc::clone(&stop);
        let saves = Arc::clone(&saves);
        std::thread::spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                let blob = lock_plugin(&plugin).save_state();
                let _ = blob;
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
            let mut guard = lock_plugin(&plugin);
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
            "block took {elapsed:?} under concurrent save - lock wedged?"
        );
    }

    stop.store(true, Ordering::Relaxed);
    save_thread.join().expect("saver thread panicked");
    assert!(
        saves.load(Ordering::Relaxed) > 0,
        "saver thread never acquired the lock"
    );
    // Not asserted (CI timing noise), but useful when run locally.
    eprintln!("worst block under save contention: {worst:?}");
}
