//! `cargo-truce` library — engine for `cargo truce <subcommand>`.
//!
//! `main.rs` owns argument parsing, the `cargo truce` arg strip,
//! the user-facing help block, and dispatch for `new` (single +
//! `--workspace` modes, which live in the [`scaffold`] module).
//! Every other subcommand goes through [`run`].

mod commands;
mod config;
pub(crate) mod dirs;
mod install_scope;
pub mod scaffold;
mod templates;
mod util;

#[cfg(target_os = "windows")]
mod packaging_windows;

#[cfg(target_os = "windows")]
mod windows_manifest;

// Re-exports needed by `packaging_windows`. Cfg-gated so the imports
// don't show as dead on macOS / Linux builds.
#[cfg(target_os = "windows")]
#[cfg(any(target_os = "macos", target_os = "windows"))]
pub(crate) use commands::install::aax::build_aax_template;
#[cfg(target_os = "windows")]
#[cfg(any(target_os = "macos", target_os = "windows"))]
pub(crate) use commands::package::PkgFormat;
pub(crate) use config::*;
pub(crate) use util::*;

use std::process::ExitCode;

pub(crate) type Res = std::result::Result<(), Box<dyn std::error::Error>>;
pub(crate) type BoxErr = Box<dyn std::error::Error>;

/// Run a command with the given args (e.g. `["install", "--clap"]`).
///
/// Help, scaffold (`new`), and the `cargo truce`
/// arg-stripping live in `main.rs`. Unknown commands here surface
/// back to the caller as an error so `main` can render its own help
/// block.
pub fn run(args: &[String]) -> ExitCode {
    // Strip global `-v` / `--verbose` from anywhere in the arg list.
    // Setting the static once here means every subcommand picks it up
    // without each having to parse the flag.
    let mut filtered: Vec<String> = Vec::with_capacity(args.len());
    for a in args {
        if a == "-v" || a == "--verbose" {
            util::set_verbose(true);
        } else {
            filtered.push(a.clone());
        }
    }
    let args = &filtered[..];

    let cmd = args.first().map(|s| s.as_str()).unwrap_or("");

    let result = match cmd {
        "install" => commands::install::cmd_install(&args[1..]),
        "build" => commands::build::cmd_build(&args[1..]),
        "package" => commands::package::cmd_package(&args[1..]),
        "remove" => commands::remove::cmd_remove(&args[1..]),
        "run" => commands::run::cmd_run(&args[1..]),
        "screenshot" => commands::screenshot::cmd_screenshot(&args[1..]),
        "test" => commands::test::cmd_test(),
        "status" => commands::status::cmd_status(),
        "clean" => commands::clean::cmd_clean(&args[1..]),
        "reset-au" => commands::reset_au::cmd_reset_au(&args[1..]),
        "reset-aax" => commands::reset_aax::cmd_reset_aax(&args[1..]),
        "validate" => commands::validate::cmd_validate(&args[1..]),
        "doctor" => commands::doctor::cmd_doctor(),
        "log-stream-au" => commands::log_stream_au::cmd_log_stream_au(),
        other => Err(format!("unknown command: {other:?}").into()),
    };

    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("Error: {e}");
            ExitCode::FAILURE
        }
    }
}
