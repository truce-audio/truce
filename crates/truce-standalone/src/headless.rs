//! Headless standalone: audio + MIDI device input, no window.

use std::sync::Arc;

use truce_core::export::PluginExport;
use truce_core::info::PluginCategory;

use crate::audio;
use crate::cli::Options;
use crate::midi;
use crate::vlog;

/// Run audio-only and block until SIGINT (or, when capturing a
/// finite `--input-file` to `--output-file`, until the input file
/// runs dry - at which point the capture sink is finalized and
/// the runner returns cleanly).
pub fn run<P: PluginExport>(opts: &Options) {
    #[cfg_attr(not(feature = "playback"), allow(unused_mut))]
    let mut handles = match audio::start_audio::<P>(opts) {
        Ok(h) => h,
        Err(e) => {
            eprintln!("Error: {e}");
            std::process::exit(1);
        }
    };

    // MIDI device input (if requested and available). On success
    // this spawns a background thread that pushes events into
    // `handles.pending`.
    let (_midi_guard, _midi_ctrl) = midi::MidiInputThread::start(
        opts,
        usize::from(P::info().midi_input_ports),
        Arc::clone(&handles.pending),
    );

    let is_instrument = P::info().category != PluginCategory::Effect;
    vlog!("Plugin: {}", P::info().name);
    if is_instrument && opts.midi_inputs.is_empty() {
        // Soft warning - actionable, so always print.
        eprintln!(
            "(instrument; no --midi-input specified - plugin will \
             emit silence. Use --list-midi to see available devices.)"
        );
    }

    // CI / test shape: real-time render of a finite input WAV to
    // an output WAV. Exit naturally on input EOF so the harness
    // doesn't have to send SIGINT (which would skip the WAV
    // header rewrite and leave a truncated file).
    #[cfg(feature = "playback")]
    let auto_exit_on_eof = opts.input_file.is_some() && opts.output_file.is_some();
    #[cfg(not(feature = "playback"))]
    let auto_exit_on_eof = false;

    if auto_exit_on_eof {
        // The render-to-file flow blocks until the input drains,
        // so a "starting" / "abort with Ctrl-C" hint is useful
        // enough to print unconditionally.
        eprintln!("Rendering input file to output file in real-time. Ctrl-C to abort.");
    } else {
        vlog!("Ctrl-C to quit.");
    }

    if auto_exit_on_eof {
        // Poll the playback cursor on a coarse cadence - a few
        // hundred ms of post-EOF padding is fine and keeps this
        // loop off the hot path. The audio thread keeps draining
        // the file until it saturates; we wait a little after EOF
        // so the final block makes it through cpal + the capture
        // channel.
        #[cfg(feature = "playback")]
        if let Some(src) = handles.playback.clone() {
            while !src.is_eof() {
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            std::thread::sleep(std::time::Duration::from_millis(200));
        }
    } else {
        // Block the main thread. cpal drives audio from its own
        // thread, so parking main is enough - SIGINT takes down
        // the process.
        loop {
            std::thread::park();
        }
    }

    // Finalize capture first (sets the shutdown flag, joins the
    // writer thread). This has to happen before `drop(handles)`
    // so the writer thread can finish writing whatever the audio
    // callback already submitted; setting the flag also stops
    // any further audio-thread submits, so subsequent cpal
    // callbacks (which may keep firing for a few hundred ms on
    // macOS) become no-ops on the capture path.
    #[cfg(feature = "playback")]
    if let Some(sink) = handles.capture.take() {
        sink.finalize();
    }
    drop(handles);
}
