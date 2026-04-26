mod commands;
mod config;
pub(crate) mod dirs;
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
/// Driven by the `cargo-truce` binary; this crate is the engine, not
/// the user-facing entry point.
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

    let cmd = args.first().map(|s| s.as_str()).unwrap_or("help");

    let result = match cmd {
        "install" => commands::install::cmd_install(&args[1..]),
        "build" => commands::build::cmd_build(&args[1..]),
        "package" => commands::package::cmd_package(&args[1..]),
        "remove" => commands::remove::cmd_remove(&args[1..]),
        "run" => commands::run::cmd_run(&args[1..]),
        "screenshot" => commands::screenshot::cmd_screenshot(&args[1..]),
        "new" => commands::new::cmd_new(&args[1..]),
        "test" => commands::test::cmd_test(),
        "status" => commands::status::cmd_status(),
        "clean" => commands::clean::cmd_clean(&args[1..]),
        "reset-au" => commands::reset_au::cmd_reset_au(&args[1..]),
        "reset-aax" => commands::reset_aax::cmd_reset_aax(&args[1..]),
        "validate" => commands::validate::cmd_validate(&args[1..]),
        "doctor" => commands::doctor::cmd_doctor(),
        "log-stream-au" => commands::log_stream_au::cmd_log_stream_au(),
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
Usage: cargo truce <command> [options]

Commands:
  install [--clap] [--vst3] [--vst2] [--au2] [--au3] [--aax] [--hot-reload] [--debug] [--no-build] [-p <crate>]
      Build and install plugins into the host's plug-in directories.
      Defaults to release because installing usually means audio-
      testing in a DAW — release avoids surprise CPU spikes from
      debug-build DSP under load. This differs from `cargo build`'s
      debug default; pass `--debug` to opt back into the cargo dev
      profile (faster compile, slower DSP — fine for light plugins
      and wiring checks).

      Defaults to whichever formats are in the plugin's Cargo.toml
      default features (typically clap + vst3). VST2, AU, and AAX are
      opt-in and must be enabled explicitly via these flags or by
      adding them to the plugin's default features.
      --clap         CLAP only (no sudo)
      --vst3         VST3 only
      --vst2         VST2 only (legacy format — see truce/Cargo.toml note)
      --au2          AU v2 only (.component, macOS only)
      --au3          AU v3 only (.appex, requires Xcode, macOS only)
      --aax          AAX only (requires pre-built template)
      --hot-reload   Build hot-reload shells (use with cargo watch for iteration)
      --debug        Compile with the cargo dev profile (faster compile,
                     slower DSP). Don't ship plugins built this way.
      --no-build     Skip build, install existing artifacts
      -p <crate>     Install only the plugin with this cargo crate name
                     (e.g. -p truce-example-gain)

  test
      Run all plugin tests (render, state, params, metadata).

  status
      Show installed plugins and AU registration state.

  clean [--all]
      Run `cargo clean` while preserving `target/dist/` (signed /
      notarized installers — expensive to rebuild). Pass `--all` to
      wipe everything, equivalent to a bare `cargo clean`. Does not
      touch installed plugin bundles or AU / AAX host caches — see
      `remove`, `reset-au`, and `reset-aax` for those.
      --all        Also remove `target/dist/`

  reset-au [--yes]
      macOS-only. Flush Audio Unit caches and restart `pkd` /
      `AudioComponentRegistrar`. Use when AU bundles are stuck
      serving stale binaries. CLAP / VST3 / VST2 / LV2 unaffected.
      --yes        Skip confirmation prompt

  reset-aax [--yes]
      macOS-only. Wipe this vendor's entries from the Pro Tools AAX
      cache (`/Users/Shared/Pro Tools/AAXPlugInCache`). Pro Tools
      re-scans AAX plugins on next launch.
      --yes        Skip confirmation prompt

  remove [--clap] [--vst3] [--vst2] [--au2] [--au3] [--aax] [-p <crate>] [-n <name>] [--stale] [--dry-run] [--yes]
      Remove installed plugin bundles for this project.
      Default: all formats, all plugins. Asks for confirmation.
      -p <crate>   Filter by cargo crate name (e.g. -p truce-example-gain)
      -n <name>    Filter by display name (e.g. -n 'Truce Gain')
      --stale      Remove vendor bundles NOT in the current project
                   (renamed/deleted plugins still on the system)
      --dry-run    Show what would be removed without deleting
      --yes        Skip confirmation prompt

  validate [--auval] [--auval3] [--pluginval] [--clap] [--vst2] [--all] [-p <crate>]
      Run validation tools on installed plugins.
      --auval      AU v2 validation only (macOS)
      --auval3     AU v3 validation only (macOS)
      --pluginval  VST3 validation via pluginval
      --clap       CLAP validation via clap-validator
      --vst2       VST2 dlopen + AEffect probe (macOS-only smoke binary)
      --all        Run all available validators (default)
      -p <crate>   Validate only the plugin with this cargo crate name

  log-stream-au
      macOS-only. Tail AU v3 appex logs live (`os_log` output from the
      Swift wrapper, subsystem `com.truce.au3`). Forward-only — for
      historical entries use `log show --last <duration>` directly.
      Press Ctrl-C to stop.

  package [-p <crate>] [--formats clap,vst3,...] [--no-notarize]
      Build, sign, and package plugins into macOS .pkg installers.
      Output goes to `target/dist/`.

  build [--clap] [--vst3] [--vst2] [--lv2] [--au2] [--au3] [--aax] [-p <crate>] [--hot-reload] [--debug]
      Build per-format bundles into target/bundles/ without installing.
      Defaults to release; pass `--debug` for the cargo dev profile
      when iterating on layout, packaging, or format-wrapper wiring.

      Defaults match `install`: when no format flags are passed, every
      format in the project's default Cargo features is built.
      --clap         CLAP only
      --vst3         VST3 only
      --vst2         VST2 only
      --lv2          LV2 only
      --au2          AU v2 only (.component, macOS only)
      --au3          AU v3 only (.appex inside .app, macOS only)
      --aax          AAX only (requires pre-built SDK + template)
      -p <crate>     Build only the plugin with this cargo crate name
      --hot-reload   Add the `hot-reload` feature and also build debug
                     dylibs (the logic libs the hot-reload shells watch)
      --debug        Cargo dev profile (faster compile, slower DSP).
                     Bundles still stage and sign correctly, but the
                     binary inside is debug-quality — not for shipping.

  run [-p <crate>] [--debug] [-- <args>]
      Build and run a plugin standalone. Pass `--debug` for a
      faster-compile dev-profile build (fine when iterating outside
      a DAW); release otherwise.

  screenshot [-p <crate>] [--name <name>]
      Render a plugin's editor headlessly and save the PNG to
      target/screenshots/<name>.png. With no -p, screenshots every
      plugin in truce.toml. Default name is <bundle_id>_screenshot.

  new <name>
      Scaffold a new plugin under ./plugins/<name>/.

  doctor
      Check development environment and installed plugins.

  help
      Show this message.

GLOBAL FLAGS (accepted by every subcommand):
  -v, --verbose
      Echo per-format build banners, per-bundle paths, and the full
      `codesign` chatter. Default output is the Built / Installed /
      Skipped summary plus one `✓ signed <bundle>` line per codesign.

Configuration is read from truce.toml in the project root.
Run 'cargo truce new <name>' to scaffold a new project."
    );
}
