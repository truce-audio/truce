//! Two-thread model of the wrapper's audio/host contract, sized to run
//! under Miri's data-race detector (small blocks, few iterations;
//! `-Zmiri-many-seeds` varies the schedule).
//!
//! The audio thread solely owns the plugin while processing and, after
//! each block, publishes its custom state into the lock-free
//! [`SnapshotSlot`] - exactly as the shells do. The host / GUI thread
//! reads that slot and the `MeterStore`; it never touches the plugin.
//! So the two sides never access the plugin concurrently, and the only
//! shared mutable state (the snapshot bytes, the meter cells) is behind
//! primitives that synchronize themselves. Miri would flag a regression
//! that reintroduced a host-thread read of the shared plugin - the raw
//! pointer into the instance the mediation lock used to guard.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use truce_core::meters::MeterStore;
use truce_core::snapshot::SnapshotSlot;
use truce_params::METER_ID_BASE;

const BLOCK: u64 = 8;
#[cfg(miri)]
const BLOCKS: u64 = 40;
#[cfg(not(miri))]
const BLOCKS: u64 = 2_000;

#[test]
fn audio_owns_plugin_while_host_reads_snapshot() {
    let snapshot = SnapshotSlot::new();
    let meters = MeterStore::new();
    let stop = Arc::new(AtomicBool::new(false));

    let reader = {
        let snapshot = Arc::clone(&snapshot);
        let meters = Arc::clone(&meters);
        let stop = Arc::clone(&stop);
        std::thread::spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                let _ = meters.read(METER_ID_BASE);
                // The host-side state read: lock-free snapshot, never the
                // plugin. A torn read would surface a length the audio
                // thread never publishes.
                if let Some(blob) = snapshot.read() {
                    assert!(blob.is_empty() || blob.len() == 8, "torn snapshot");
                }
                std::thread::yield_now();
            }
        })
    };

    // Audio thread: owns its state exclusively (no lock needed - the host
    // never touches it), and publishes a snapshot after each block.
    let mut frames: u64 = 0;
    for block in 0..BLOCKS {
        frames += BLOCK;
        snapshot.publish(|buf| {
            buf.extend_from_slice(&frames.to_le_bytes());
            true
        });
        #[allow(clippy::cast_precision_loss)]
        meters.write(METER_ID_BASE, block as f32);
    }

    stop.store(true, Ordering::Relaxed);
    reader.join().expect("reader panicked");

    // The per-block publish is skipped on reader contention (it uses
    // `try_lock`), so the last one may not have landed. The reader has
    // now joined, so this final publish is uncontended.
    snapshot.publish(|buf| {
        buf.extend_from_slice(&frames.to_le_bytes());
        true
    });
    let final_frames = u64::from_le_bytes(
        snapshot
            .read()
            .expect("published")
            .try_into()
            .expect("8-byte blob"),
    );
    assert_eq!(final_frames, BLOCKS * BLOCK);
}
