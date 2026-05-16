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
pub mod keyboard;
pub mod midi;
#[cfg(feature = "playback")]
pub mod offline;
#[cfg(feature = "playback")]
pub mod playback;
pub mod state;
pub mod transport;

#[cfg(feature = "gui")]
pub mod windowed;

#[cfg(all(target_os = "macos", feature = "gui"))]
pub mod menu_macos;

#[cfg(all(target_os = "windows", feature = "gui"))]
pub mod menu_windows;

pub mod headless;

pub use truce_core::export::PluginExport;

// ---------------------------------------------------------------------------
// Verbose state — set once from CLI / env, read everywhere via `vlog!`.
// ---------------------------------------------------------------------------

use std::sync::atomic::{AtomicBool, Ordering};

static VERBOSE: AtomicBool = AtomicBool::new(false);

/// True when `--verbose` / `-v` (or `TRUCE_STANDALONE_VERBOSE=1`) was
/// passed at launch. Errors and `--list-*` output ignore this flag.
pub fn is_verbose() -> bool {
    VERBOSE.load(Ordering::Relaxed)
}

pub(crate) fn set_verbose(on: bool) {
    VERBOSE.store(on, Ordering::Relaxed);
}

/// `eprintln!`, but only fires when [`is_verbose`] is true. Used for
/// status chatter (device picks, toggles, transport state, save /
/// load notices) — anything the user might want a trace of but that
/// shouldn't clutter the default output.
macro_rules! vlog {
    ($($arg:tt)*) => {
        if $crate::is_verbose() {
            eprintln!($($arg)*);
        }
    };
}
pub(crate) use vlog;

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

impl Defaults {
    /// Layer these author-supplied defaults beneath whatever argv /
    /// env already resolved. Only fields where `opts` is `None` adopt
    /// the default; CLI / env take precedence.
    ///
    /// Adding a new field to [`Defaults`] **must also add a line
    /// here** — keeping the apply logic next to the struct is the
    /// only thing stopping a new field from silently never being
    /// applied. The `match` below is exhaustive over [`Defaults`]'s
    /// fields by destructuring; adding a field there forces a
    /// compile error here if this method isn't updated.
    fn apply(self, opts: &mut cli::Options) {
        let Defaults {
            input_enabled,
            output_enabled,
        } = self;
        opts.input_enabled = opts.input_enabled.or(input_enabled);
        opts.output_enabled = opts.output_enabled.or(output_enabled);
    }
}

/// Run the plugin standalone with no plugin-author defaults. Argv,
/// env, and the compiled runtime defaults are the only inputs.
/// Dispatches to the windowed or headless runner; returns when the
/// user closes the window or sends SIGINT.
pub fn run<P: PluginExport>()
where
    P::Params: 'static,
{
    run_with::<P>(Defaults::default());
}

/// `cargo truce package` on Windows links the standalone `.exe` with
/// `/SUBSYSTEM:WINDOWS` so the packaged installer doesn't pop a stray
/// console next to the plugin window when the user launches from the
/// Start Menu / Explorer. The downside is that the same `.exe`
/// invoked from `cmd.exe` / PowerShell starts with no console, so
/// `eprintln!` lands on a null handle. `AttachConsole(ATTACH_PARENT_PROCESS)`
/// rebinds the standard handles to whatever terminal launched us, so
/// `--help`, `--list-devices`, and error diagnostics print where the
/// user expects. Failure means there was no parent console (Start
/// Menu / Explorer launch) — silently move on; that's the case the
/// subsystem flag exists to handle. No-op on non-Windows or in
/// console-subsystem builds (`AttachConsole` returns failure when a
/// console is already attached, which we ignore).
#[cfg(target_os = "windows")]
fn attach_parent_console() {
    use windows_sys::Win32::System::Console::{ATTACH_PARENT_PROCESS, AttachConsole};
    // SAFETY: trivial FFI — no aliasing or lifetime concerns.
    unsafe {
        let _ = AttachConsole(ATTACH_PARENT_PROCESS);
    }
}

#[cfg(not(target_os = "windows"))]
fn attach_parent_console() {}

/// Run the plugin standalone with the supplied launch defaults.
/// Argv and env still take precedence — `defaults` only fills in
/// values neither layer set. Same dispatch as [`run`].
pub fn run_with<P: PluginExport>(defaults: Defaults)
where
    P::Params: 'static,
{
    // Must run before any stdout/stderr output: Rust caches the
    // standard handles on first use, so attaching after the first
    // `eprintln!` would leave the cached null handles in place and
    // any later prints would still vanish.
    attach_parent_console();

    let mut opts = match cli::parse() {
        Ok(o) => o,
        Err(e) => {
            eprintln!("Error: {e}");
            eprintln!("Run with --help for usage.");
            std::process::exit(2);
        }
    };

    // `--help` was a process-wide `exit(0)` inside `cli::parse`. Now
    // it returns `Options { help: true, .. }` so libraries calling
    // into `parse` can be tested without short-circuiting the
    // process; we honor it here at the binary boundary.
    if opts.help {
        return;
    }

    // Latch the verbose flag before anything else logs — every
    // `vlog!` checks this static.
    set_verbose(opts.verbose);

    // Layer the plugin-author defaults beneath whatever argv / env
    // already resolved. Other Options fields stay CLI/env-only —
    // device, sample rate, buffer, MIDI, BPM, state are per-machine
    // concerns the developer shouldn't pin in code.
    defaults.apply(&mut opts);

    // Lowest tier above the runtime default: the TOML-baked
    // `mute_preview_output`. Lets analyzer-style plug-ins ship a
    // standalone that drives `process()` from mic input without
    // closing a feedback loop to the speakers. CLI / env / `Defaults`
    // already set `output_enabled` to something explicit if any of
    // those tiers cared, so this `or` only fires when nothing above
    // it spoke.
    if P::info().mute_preview_output {
        opts.output_enabled = opts.output_enabled.or(Some(false));
    }

    if opts.list_devices {
        audio::list_devices();
        return;
    }
    if opts.list_midi {
        midi::list_midi();
        return;
    }

    // `--output-file` always forces headless: opening a window
    // during a render burns GPU/CPU on a UI nobody is watching,
    // and offline mode doesn't drive an event loop at all. Notice
    // only fires when the user didn't explicitly ask for headless,
    // so we don't double-message the deliberate case.
    #[cfg(feature = "playback")]
    if opts.output_file.is_some() && !opts.headless {
        eprintln!(
            "--output-file implies --headless; \
             running without a window."
        );
        opts.headless = true;
    }

    // `--no-playback` only applies in the canonical CI render
    // shape (--input-file + --output-file). In any other combo
    // there's either no driver or no destination — soft-warn and
    // fall through to real-time so the runner stays useful.
    #[cfg(feature = "playback")]
    if opts.no_playback && !(opts.input_file.is_some() && opts.output_file.is_some()) {
        eprintln!(
            "--no-playback ignored: \
             requires both --input-file and --output-file"
        );
        opts.no_playback = false;
    }

    #[cfg(feature = "playback")]
    if opts.no_playback {
        match offline::render::<P>(&opts) {
            Ok(()) => return,
            Err(e) => {
                eprintln!("Error: {e}");
                std::process::exit(1);
            }
        }
    }

    #[cfg(feature = "gui")]
    if opts.headless {
        headless::run::<P>(&opts);
    } else {
        windowed::run::<P>(&opts);
    }
    #[cfg(not(feature = "gui"))]
    headless::run::<P>(&opts);
}
