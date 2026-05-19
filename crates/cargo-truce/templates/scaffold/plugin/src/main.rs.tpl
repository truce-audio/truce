//! Entry point for standalone mode - run the plugin as a regular
//! desktop app via `cargo truce run`, no DAW needed. Only compiled
//! when the `standalone` feature is enabled (see `[[bin]]` in
//! Cargo.toml).
//!
//! Safe to delete this file (and the `standalone` feature + bin
//! entry in Cargo.toml) if you don't want a standalone build.

use {crate_lib}::Plugin;

fn main() \{
    // `run::<Plugin>()` parses argv + `TRUCE_STANDALONE_*` env vars
    // and dispatches. To pin launch defaults (e.g. mic on at start
    // for an effect demo) call `run_with::<Plugin>(Defaults \{ … })`
    // - argv / env still take precedence over the values you pass.
    truce_standalone::run::<Plugin>();
}
