//! Headless standalone: audio + MIDI device input, no window.

use std::sync::Arc;

use truce_core::export::PluginExport;
use truce_core::info::PluginCategory;

use crate::audio;
use crate::cli::Options;
use crate::midi;

/// Run audio-only and block until SIGINT.
pub fn run<P: PluginExport>(opts: &Options) {
    let handles = audio::start_audio::<P>(opts);

    // MIDI device input (if requested and available). On success
    // this spawns a background thread that pushes events into
    // `handles.pending`.
    let _midi_guard = midi::MidiInputThread::start(opts, Arc::clone(&handles.pending));

    let is_instrument = P::info().category != PluginCategory::Effect;
    println!("=== truce standalone (headless) ===");
    println!("Plugin: {}", P::info().name);
    if is_instrument && opts.midi_input.is_none() {
        println!(
            "(instrument; no --midi-input specified — plugin will \
             emit silence. Use --list-midi to see available devices.)"
        );
    }
    println!("Ctrl-C to quit.");

    // Block the main thread. cpal drives audio from its own thread,
    // so parking main is enough — SIGINT takes down the process.
    loop {
        std::thread::park();
    }

    #[allow(unreachable_code)]
    drop(handles);
}
