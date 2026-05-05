//! `cargo-truce` library — engine for `cargo truce <subcommand>`.
//!
//! `main.rs` owns argument parsing, the `cargo truce` arg strip,
//! the user-facing help block, and dispatch for `new` (single +
//! `--workspace` modes, which live in the [`scaffold`] module).
//! Every other subcommand goes through [`run`].

mod commands;
mod config;
pub(crate) mod dirs;
mod format;
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
pub(crate) use commands::install::aax::build_aax_template;
#[cfg(target_os = "windows")]
pub(crate) use commands::package::PkgFormat;
pub(crate) use config::{Config, PluginDef, deployment_target, load_config, resolve_aax_sdk_path};
pub(crate) use util::{
    cargo_build, cargo_build_debug, check_cmd, codesign_bundle, confirm_prompt,
    detect_default_features, find_on_path, is_debug_profile, log_output, log_skip, project_root,
    read_standalone_bin_name, release_lib, run_sudo, set_build_profile, set_debug_profile,
    tag_fail, tag_ok, tag_warn, take_outputs, take_skipped, tmp_dir, verify_shell_profile_declared,
    vprintln,
};

// Re-exports used only by the macOS / Windows installer pipelines.
// Linux ships plugins via distro tooling rather than the bundled
// `package` flow, so these symbols are absent there.
#[cfg(any(target_os = "macos", target_os = "windows"))]
pub(crate) use util::{
    read_workspace_version, release_lib_for_target, rustup_has_target, tag_info,
};

// macOS-only: codesign / lipo / notary / AAX PACE-sign pipeline. The
// `PackagingConfig` / `copy_dir_recursive` re-exports land here too —
// only `commands::package::{macos, stage}` consume them, and both are
// macOS-gated.
#[cfg(target_os = "macos")]
pub(crate) use config::PackagingConfig;
#[cfg(target_os = "macos")]
pub(crate) use util::{
    MacArch, cargo_build_for_arch, copy_dir_recursive, extract_team_id, is_production_identity,
    lipo_into, locate_wraptool_macos, pace_sign_aax_macos, run_codesign, run_quiet, run_silent,
};

// Windows-only: VS / MSVC / cmake / ninja discovery + Program Files
// path helpers, used by `commands::{doctor, install, install::aax}`
// and `packaging_windows`.
#[cfg(target_os = "windows")]
pub(crate) use config::WindowsSigningConfig;
#[cfg(target_os = "windows")]
pub(crate) use util::{
    common_program_files, locate_cmake, locate_msvc_cl, locate_ninja, locate_vcvars64,
    program_files, vs_install_paths, which_exe,
};

use std::process::ExitCode;

pub(crate) type Res = std::result::Result<(), Box<dyn std::error::Error>>;
pub(crate) type BoxErr = Box<dyn std::error::Error>;

/// Run a command with the given args (e.g. `["install", "--clap"]`).
///
/// Help, scaffold (`new`), and the `cargo truce`
/// arg-stripping live in `main.rs`. Unknown commands here surface
/// back to the caller as an error so `main` can render its own help
/// block.
#[must_use]
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

    let cmd = args.first().map_or("", std::string::String::as_str);

    let result = match cmd {
        "install" => commands::install::cmd_install(&args[1..]),
        "build" => commands::build::cmd_build(&args[1..]),
        "package" => commands::package::cmd_package(&args[1..]),
        "uninstall" => commands::uninstall::cmd_uninstall(&args[1..]),
        "run" => commands::run::cmd_run(&args[1..]),
        "screenshot" => commands::screenshot::cmd_screenshot(&args[1..]),
        "status" => commands::status::cmd_status(),
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
