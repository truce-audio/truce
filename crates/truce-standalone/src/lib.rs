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
//! See [`cli`] for the full flag surface.

pub mod audio;
pub mod cli;
pub mod in_process;
pub mod keyboard;
pub mod midi;
pub mod transport;

#[cfg(feature = "gui")]
pub mod windowed;

#[cfg(all(target_os = "macos", feature = "gui"))]
pub mod menu_macos;

#[cfg(all(target_os = "windows", feature = "gui"))]
pub mod menu_windows;

pub mod headless;

pub use truce_core::export::PluginExport;

/// Re-export for backward compatibility.
pub use truce_core::export::PluginExport as StandaloneExport;

/// Plugin-author launch defaults — used as the lowest tier of the
/// CLI parser, beneath argv and `TRUCE_STANDALONE_*` env vars.
/// Empty `Defaults::default()` lets every value fall through to the
/// compiled runtime default (input off, output on, cpal-picked
/// devices). Pass to [`run_with`] when you want to override.
#[derive(Default, Clone, Copy, Debug)]
pub struct Defaults {
    /// Whether the mic is enabled at launch. `None` = use the
    /// privacy default (off).
    pub input_enabled: Option<bool>,
    /// Whether the speakers are enabled at launch. `None` = use the
    /// runtime default (on).
    pub output_enabled: Option<bool>,
}

/// Run the plugin standalone with no plugin-author defaults. Argv,
/// env, and the compiled runtime defaults are the only inputs.
/// Dispatches to the windowed or headless runner; returns when the
/// user closes the window or sends SIGINT.
pub fn run<P: PluginExport>()
where
    P::Params: 'static,
{
    run_with::<P>(Defaults::default())
}

/// Run the plugin standalone with the supplied launch defaults.
/// Argv and env still take precedence — `defaults` only fills in
/// values neither layer set. Same dispatch as [`run`].
pub fn run_with<P: PluginExport>(defaults: Defaults)
where
    P::Params: 'static,
{
    let mut opts = match cli::parse() {
        Ok(o) => o,
        Err(e) => {
            eprintln!("Error: {e}");
            eprintln!("Run with --help for usage.");
            std::process::exit(2);
        }
    };

    // Layer the plugin-author defaults beneath whatever argv / env
    // already resolved. Other Options fields stay CLI/env-only —
    // device, sample rate, buffer, MIDI, BPM, state are
    // per-machine concerns the developer shouldn't pin in code.
    opts.input_enabled = opts.input_enabled.or(defaults.input_enabled);
    opts.output_enabled = opts.output_enabled.or(defaults.output_enabled);

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
