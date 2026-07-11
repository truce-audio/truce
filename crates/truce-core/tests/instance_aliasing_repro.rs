//! Two-thread model of the wrapper instance-access contract, sized to run
//! under Miri. Run the aliasing check with
//! `cargo +nightly miri test -p truce-core --test instance_aliasing_repro
//! -- -Zmiri-tree-borrows` (the default data-race detector also runs, and
//! `-Zmiri-many-seeds` varies the schedule).
//!
//! Every format wrapper reaches its instance through a shared `&Instance`
//! cast from the host's `*ctx` - never a whole-struct `&mut *ctx`. Two live
//! references to one allocation where at least one is `&mut`, formed on the
//! audio and host threads at once, is undefined behavior under the aliasing
//! model. The fix routes per-block scratch through an `audio` [`PluginCell`]
//! and editor state through a `gui` one: interior mutability via
//! `UnsafeCell`, so the audio thread's `&mut` to the scratch and a
//! concurrent host-thread `&` to a sibling field (or the gui cell) never
//! alias.
//!
//! This harness drives exactly that shape - audio thread owns the `audio`
//! cell while the host thread owns the `gui` cell and reads sibling atomics,
//! both through their own `&Instance`. A regression to `&mut *ctx` (which
//! would reborrow the whole struct mutably on the audio thread) trips Tree
//! Borrows against the host thread's live `&Instance` here.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use truce_core::wrapper::PluginCell;

/// Per-block scratch, audio-thread-owned (mirrors `Vst3Scratch` etc.).
struct Scratch {
    counter: u64,
    buf: Vec<u32>,
}

/// Editor state, host-thread-owned (mirrors `Vst3Gui` etc.).
struct Gui {
    opened: u64,
}

/// Shape of a wrapper instance: two ownership cells plus a sibling atomic
/// the host thread reads while the audio thread holds `&mut` into `audio`.
struct Instance {
    audio: PluginCell<Scratch>,
    gui: PluginCell<Gui>,
    latency: AtomicU64,
}

#[cfg(miri)]
const BLOCKS: u64 = 60;
#[cfg(not(miri))]
const BLOCKS: u64 = 20_000;

#[test]
fn audio_cell_and_host_access_do_not_alias() {
    let inst = Arc::new(Instance {
        audio: PluginCell::new(Scratch {
            counter: 0,
            buf: vec![0; 8],
        }),
        gui: PluginCell::new(Gui { opened: 0 }),
        latency: AtomicU64::new(0),
    });
    let stop = Arc::new(AtomicBool::new(false));

    // Host thread: enters the `gui` cell (a different allocation region than
    // `audio`) and reads the sibling `latency` atomic, both through its own
    // shared `&Instance` - exactly what a GUI / param callback does while
    // `process` runs on the audio thread.
    let host = {
        let inst = Arc::clone(&inst);
        let stop = Arc::clone(&stop);
        std::thread::spawn(move || {
            let mut last = 0u64;
            while !stop.load(Ordering::Relaxed) {
                inst.gui.enter().opened += 1;
                last = last.max(inst.latency.load(Ordering::Relaxed));
                std::thread::yield_now();
            }
            last
        })
    };

    // Audio thread: enters the `audio` cell (forming `&mut Scratch` through
    // the `UnsafeCell` only, never a whole-struct `&mut`), mutates scratch,
    // and publishes latency for the host to observe.
    for block in 0..BLOCKS {
        {
            let mut scr = inst.audio.enter();
            scr.counter += 1;
            let len = scr.buf.len() as u64;
            let n = usize::try_from(scr.counter % len).unwrap_or(0);
            scr.buf[n] = scr.buf[n].wrapping_add(1);
        }
        inst.latency.store(block, Ordering::Relaxed);
        std::thread::yield_now();
    }

    stop.store(true, Ordering::Relaxed);
    host.join().expect("host thread panicked");

    // The threads have joined, so these final reads are uncontended.
    assert_eq!(inst.audio.enter().counter, BLOCKS);
    assert!(inst.gui.enter().opened >= 1);
}
