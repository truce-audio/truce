//! Standalone host for truce plugins.
//!
//! Runs a plugin cdylib with direct cpal audio I/O and an optional
//! GUI window (via baseview + the plugin's own `Editor`). Zero
//! plugin-library code is required — the runner obtains the editor
//! via `PluginExport::editor()`, the same API every format wrapper
//! uses.
//!
//! # Entry point
//!
//! Plugins supply a `[[bin]] <suffix>-standalone` target with a
//! `src/main.rs` that calls:
//!
//! ```ignore
//! fn main() {
//!     truce_standalone::run::<my_plugin::Plugin>();
//! }
//! ```
//!
//! # Modes
//!
//! - **Windowed** (default, requires `gui` feature): opens a
//!   baseview window hosting the plugin's editor, drives a cpal
//!   stream on the audio thread.
//! - **Headless** (`--headless` flag or the `gui` feature disabled):
//!   audio only. For effects this means audio passes through; for
//!   instruments the plugin emits silence unless a MIDI device is
//!   connected (`--midi-input`; see [`midi`]).
//!
//! See [`cli`] for the full flag surface and
//! [`../../../../truce-docs/docs/internal/standalone-bring-up-to-speed.md`](../../../../truce-docs/docs/internal/standalone-bring-up-to-speed.md)
//! for the phased design plan.

pub mod audio;
pub mod cli;
pub mod in_process;
pub mod keyboard;
pub mod midi;
pub mod transport;

#[cfg(feature = "gui")]
pub mod windowed;

pub mod headless;

pub use truce_core::export::PluginExport;

/// Re-export for backward compatibility.
pub use truce_core::export::PluginExport as StandaloneExport;

/// Run the plugin standalone.
///
/// Parses CLI flags, loads config + environment overrides, then
/// dispatches to the windowed or headless runner. Returns when the
/// user closes the window or sends SIGINT.
pub fn run<P: PluginExport>()
where
    P::Params: 'static,
{
    let opts = match cli::parse() {
        Ok(o) => o,
        Err(e) => {
            eprintln!("Error: {e}");
            eprintln!("Run with --help for usage.");
            std::process::exit(2);
        }
    };

    if opts.list_devices {
        audio::list_devices();
        return;
    }
    if opts.list_midi {
        midi::list_midi();
        return;
    }

    #[cfg(feature = "gui")]
    {
        if opts.headless {
            headless::run::<P>(&opts);
        } else {
            windowed::run::<P>(&opts);
        }
        return;
    }
    #[cfg(not(feature = "gui"))]
    headless::run::<P>(&opts);
}
