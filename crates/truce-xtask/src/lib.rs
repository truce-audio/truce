mod commands;
mod config;
pub(crate) mod dirs;
mod templates;
mod util;

#[cfg(target_os = "windows")]
mod packaging_windows;

pub(crate) use commands::install::aax::build_aax_template;
pub(crate) use commands::package::PkgFormat;
pub(crate) use config::*;
pub(crate) use util::*;

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

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
        "doctor" => cmd_doctor(),
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

  build [-p <suffix>] [--dev]
      Build plugin bundles to target/bundles/ without installing.

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




// ---------------------------------------------------------------------------
// doctor — environment diagnostics
// ---------------------------------------------------------------------------

fn cmd_doctor() -> Res {
    eprintln!("truce doctor");
    eprintln!("─────────────────────────────────────────");
    eprintln!();

    // Toolchain
    eprintln!("  Toolchain");
    check_cmd("rustc", &["--version"], "rustc");
    check_cmd("cargo", &["--version"], "cargo");

    // Platform tools
    #[cfg(target_os = "macos")]
    {
        eprintln!();
        eprintln!("  macOS");
        check_cmd("xcode-select", &["-p"], "Xcode CLI tools");
        check_cmd("xcodebuild", &["-version"], "xcodebuild (AU v3)");
        check_cmd("codesign", &["--help"], "codesign");
        match locate_wraptool_macos() {
            Some(p) => eprintln!("    ✅ wraptool (PACE) at {}", p.display()),
            None => eprintln!(
                "    ℹ️  wraptool not found — only needed for signed AAX builds. \
                 Install Eden via the iLok License Manager, then optionally \
                 `sudo ln -s /Applications/PACEAntiPiracy/Eden/Fusion/Current/bin/wraptool /usr/local/bin/wraptool`"
            ),
        }

        // Universal packaging (default for `cargo truce package`) needs both
        // Apple Rust targets. Missing targets are a warning, not an error —
        // `--host-only` still works without them.
        let has_x64 = rustup_has_target("x86_64-apple-darwin");
        let has_arm = rustup_has_target("aarch64-apple-darwin");
        match (has_x64, has_arm) {
            (true, true) => eprintln!(
                "    ✅ Rust targets: x86_64-apple-darwin + aarch64-apple-darwin — `cargo truce package` will produce universal Mach-O binaries"
            ),
            (false, true) => eprintln!(
                "    ⚠️  Rust target x86_64-apple-darwin missing — run: rustup target add x86_64-apple-darwin (or pass `--host-only` to skip)"
            ),
            (true, false) => eprintln!(
                "    ⚠️  Rust target aarch64-apple-darwin missing — run: rustup target add aarch64-apple-darwin (or pass `--host-only` to skip)"
            ),
            (false, false) => eprintln!(
                "    ⚠️  No Apple Rust targets installed — run: rustup target add x86_64-apple-darwin aarch64-apple-darwin (or pass `--host-only` to skip)"
            ),
        }
    }
    #[cfg(target_os = "windows")]
    {
        eprintln!();
        eprintln!("  Windows");
        match locate_cmake() {
            Some(p) => eprintln!("    ✅ cmake (AAX template build): {}", p.display()),
            None => eprintln!(
                "    ❌ cmake.exe not found — install cmake or VS \"C++ CMake tools\""
            ),
        }
        match locate_ninja() {
            Some(p) => eprintln!("    ✅ ninja (AAX template build): {}", p.display()),
            None => eprintln!(
                "    ❌ ninja.exe not found — install ninja or VS \"C++ CMake tools\""
            ),
        }

        eprintln!();
        eprintln!("  Packaging (Windows)");
        packaging_windows::doctor();
    }

    // Compilers
    eprintln!();
    eprintln!("  Compilers");
    #[cfg(not(target_os = "windows"))]
    {
        check_cmd("cc", &["--version"], "C compiler");
        check_cmd("c++", &["--version"], "C++ compiler (VST3 shim)");
    }
    #[cfg(target_os = "windows")]
    {
        check_cmd("cl", &["/?"], "MSVC compiler (run from Developer Command Prompt)");
    }

    // Validation tools
    eprintln!();
    eprintln!("  Validation Tools");
    check_cmd("auval", &["-h"], "auval");
    check_which_with_env("pluginval", Some("PLUGINVAL"));
    check_which_with_env("clap-validator", Some("CLAP_VALIDATOR"));

    // Configuration
    eprintln!();
    eprintln!("  Configuration");
    let root = project_root();
    let config = if root.join("truce.toml").exists() {
        match load_config() {
            Ok(c) => {
                eprintln!("    ✅ truce.toml: {} plugins configured", c.plugin.len());
                Some(c)
            }
            Err(e) => {
                eprintln!("    ❌ truce.toml parse error: {e}");
                None
            }
        }
    } else {
        eprintln!("    ❌ truce.toml not found");
        None
    };

    // AAX SDK
    eprintln!();
    eprintln!("  SDKs");
    let aax_sdk = config.as_ref().and_then(resolve_aax_sdk_path);
    match aax_sdk {
        Some(p) => eprintln!("    ✅ AAX SDK at {}", p.display()),
        None => {
            let hint = if cfg!(target_os = "windows") { "[windows].aax_sdk_path" } else { "[macos].aax_sdk_path" };
            eprintln!("    ⚠️  AAX SDK not configured (set {hint} in truce.toml or AAX_SDK_PATH env var)");
        }
    }

    // Installed plugins
    eprintln!();
    eprintln!("  Installed Plugins");
    #[cfg(target_os = "macos")]
    {
        let home = dirs::home_dir().unwrap_or_default();
        count_plugins(&home.join("Library/Audio/Plug-Ins/CLAP"), "CLAP");
        count_plugins(&Path::new("/Library/Audio/Plug-Ins/VST3").to_path_buf(), "VST3");
        count_plugins(&Path::new("/Library/Audio/Plug-Ins/Components").to_path_buf(), "AU v2");
        count_plugins(&home.join("Library/Audio/Plug-Ins/VST"), "VST2");
    }
    #[cfg(target_os = "windows")]
    {
        let cpf = common_program_files();
        let pf = program_files();
        count_plugins(&cpf.join("CLAP"), "CLAP");
        count_plugins(&cpf.join("VST3"), "VST3");
        count_plugins(&pf.join("Steinberg").join("VstPlugins"), "VST2");
        count_plugins(&cpf.join("Avid").join("Audio").join("Plug-Ins"), "AAX");
    }
    if root.join("rust-toolchain.toml").exists() {
        eprintln!("    ✅ rust-toolchain.toml present");
    }

    eprintln!();
    eprintln!("─────────────────────────────────────────");
    Ok(())
}


fn check_which(name: &str) {
    check_which_with_env(name, None);
}

/// Like `check_which`, but consults `env_var` (process env, then
/// `.cargo/config.toml` `[env]`) before falling back to `$PATH`. Lets users
/// point doctor at tools installed outside `$PATH` — useful for `.app`-bundled
/// binaries (pluginval) or sibling source checkouts (clap-validator).
fn check_which_with_env(name: &str, env_var: Option<&str>) {
    if let Some(var) = env_var {
        if let Some(path) = std::env::var(var)
            .ok()
            .or_else(|| read_cargo_config_env(var))
        {
            let p = PathBuf::from(&path);
            if p.is_file() {
                eprintln!("    ✅ {name}: {path} (via ${var})");
                return;
            }
            eprintln!(
                "    ⚠️  {name}: ${var}={path} but file not found — falling back to $PATH"
            );
        }
    }
    match Command::new("which").arg(name).output() {
        Ok(o) if o.status.success() => {
            let path = String::from_utf8_lossy(&o.stdout).trim().to_string();
            eprintln!("    ✅ {name}: {path}");
        }
        _ => {
            let hint = env_var
                .map(|v| format!(" (or set ${v} in shell or .cargo/config.toml [env])"))
                .unwrap_or_default();
            eprintln!("    ⚠️  {name}: not found{hint}");
        }
    }
}

fn count_plugins(dir: &PathBuf, label: &str) {
    if dir.exists() {
        let count = fs::read_dir(dir)
            .map(|entries| entries.filter(|e| e.is_ok()).count())
            .unwrap_or(0);
        eprintln!("    {label}: {count} items in {}", dir.display());
    } else {
        eprintln!("    {label}: directory not found");
    }
}
