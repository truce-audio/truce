mod commands;
mod config;
pub(crate) mod dirs;
pub(crate) mod presets;
mod templates;
mod util;

#[cfg(target_os = "windows")]
mod packaging_windows;

// Re-exports needed by `packaging_windows`. Cfg-gated so the imports
// don't show as dead on macOS / Linux builds.
#[cfg(target_os = "windows")]
pub(crate) use commands::install::aax::build_aax_template;
#[cfg(target_os = "windows")]
pub(crate) use commands::package::PkgFormat;
pub(crate) use config::*;
pub(crate) use util::*;

use std::env;
use std::process::ExitCode;

pub(crate) type Res = std::result::Result<(), Box<dyn std::error::Error>>;
pub(crate) type BoxErr = Box<dyn std::error::Error>;

pub fn main() -> ExitCode {
    let args: Vec<String> = env::args().skip(1).collect();
    run(&args)
}

/// Run a command with the given args (e.g. `["install", "--clap"]`).
///
/// Used by both `cargo xtask` (workspace binary) and `cargo truce`
/// (globally installed binary).
pub fn run(args: &[String]) -> ExitCode {
    let cmd = args.first().map(|s| s.as_str()).unwrap_or("help");

    let result = match cmd {
        "install" => commands::install::cmd_install(&args[1..]),
        "build" => commands::build::cmd_build(&args[1..]),
        "package" => commands::package::cmd_package(&args[1..]),
        "remove" => commands::remove::cmd_remove(&args[1..]),
        "run" => commands::run::cmd_run(&args[1..]),
        "new" => commands::new::cmd_new(&args[1..]),
        "test" => commands::test::cmd_test(),
        "status" => commands::status::cmd_status(),
        "clean" => commands::clean::cmd_clean(&args[1..]),
        "nuke" => commands::nuke::cmd_nuke(&args[1..]),
        "validate" => commands::validate::cmd_validate(&args[1..]),
        "doctor" => commands::doctor::cmd_doctor(),
        "log" => commands::log::cmd_log(),
        "help" | "--help" | "-h" => {
            print_help();
            Ok(())
        }
        other => {
            eprintln!("Unknown command: {other}");
            print_help();
            Err("unknown command".into())
        }
    };

    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("Error: {e}");
            ExitCode::FAILURE
        }
    }
}

fn print_help() {
    eprintln!(
        "\
Usage: cargo xtask <command> [options]

Commands:
  install [--clap] [--vst3] [--vst2] [--au2] [--au3] [--aax] [--dev] [--no-build] [-p <suffix>]
      Build and install plugins. Defaults to whichever formats are in the
      plugin's Cargo.toml default features (typically clap + vst3). VST2, AU,
      and AAX are opt-in and must be enabled explicitly via these flags or
      by adding them to the plugin's default features.
      --clap       CLAP only (no sudo)
      --vst3       VST3 only
      --vst2       VST2 only (legacy format — see truce/Cargo.toml note)
      --au2        AU v2 only (.component, macOS only)
      --au3        AU v3 only (.appex, requires Xcode, macOS only)
      --aax        AAX only (requires pre-built template)
      --dev        Build hot-reload shells (use with cargo watch for iteration)
      --no-build   Skip build, install existing artifacts
      -p <suffix>  Install only the plugin with this suffix (e.g. -p gain)

  test
      Run all plugin tests (render, state, params, metadata).

  status
      Show installed plugins and AU registration state.

  clean [--yes]
      Clear all AU/DAW caches and restart audio daemons. Asks for
      confirmation by default.
      --yes        Skip confirmation prompt

  remove [--clap] [--vst3] [--vst2] [--au2] [--au3] [--aax] [-p <suffix>] [-n <name>] [--stale] [--dry-run] [--yes]
      Remove installed plugin bundles for this project.
      Default: all formats, all plugins. Asks for confirmation.
      -p <suffix>  Filter by plugin suffix (e.g. -p gain)
      -n <name>    Filter by display name (e.g. -n 'Truce Gain')
      --stale      Remove vendor bundles NOT in the current project
                   (renamed/deleted plugins still on the system)
      --dry-run    Show what would be removed without deleting
      --yes        Skip confirmation prompt

  nuke [-p <suffix>]
      Nuclear reset: remove AU v3 apps, disable pluginkit registrations,
      kill daemons, clear all caches, cargo clean.
      Use when AU v3 appex is stuck serving stale binaries.
      -p <suffix>  Nuke only the specified plugin

  validate [--auval] [--auval3] [--pluginval] [--clap] [--all] [-p <suffix>]
      Run validation tools on installed plugins.
      --auval      AU v2 validation only (macOS)
      --auval3     AU v3 validation only (macOS)
      --pluginval  VST3 validation via pluginval
      --clap       CLAP validation via clap-validator
      --all        Run all available validators (default)
      -p <suffix>  Validate only the plugin with this suffix

  log
      Stream AU v3 appex logs (NSLog output from the extension process).
      Press Ctrl-C to stop.

  package [-p <suffix>] [--formats clap,vst3,...] [--no-notarize]
      Build, sign, and package plugins into macOS .pkg installers.
      Output goes to dist/ directory.

  build [--clap] [--vst3] [--vst2] [--lv2] [--au2] [-p <suffix>] [--dev]
      Build per-format bundles into target/bundles/ without installing.
      Defaults match `install`: when no format flags are passed, every
      format in the project's default Cargo features is built.
      AU v3 and AAX are install-only (xcodebuild / cmake template +
      system locations); use `cargo truce install --au3` / `--aax`.
      --clap       CLAP only
      --vst3       VST3 only
      --vst2       VST2 only
      --lv2        LV2 only
      --au2        AU v2 only (.component, macOS only)
      -p <suffix>  Build only the plugin with this suffix
      --dev        Add the `dev` feature and also build debug dylibs
                   (the logic libs the hot-reload shells watch)

  run [-p <suffix>] [-- <args>]
      Build and run a plugin standalone.

  new <name> [--hot] [--instrument] [--midi]
      Scaffold a new plugin.
      --hot          Shell/logic split for hot-reload
      --instrument   Instrument template (no audio input)
      --midi         MIDI effect template

  doctor
      Check development environment and installed plugins.

  help
      Show this message.

Configuration is read from truce.toml in the project root.
Run 'cargo truce new <name>' to scaffold a new project."
    );
}
