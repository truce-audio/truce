//! cargo-truce — build tool for truce audio plugins.
//!
//! Install:
//!   cargo install cargo-truce
//!
//! Usage:
//!   cargo truce new my-plugin          # scaffold a new plugin project
//!   cargo truce new studio --workspace gain reverb synth
//!   cargo truce install                # build + bundle + sign + install
//!   cargo truce install --clap         # single format
//!   cargo truce validate               # run auval, auval3, pluginval, clap-validator
//!   cargo truce doctor                 # check environment

use std::path::Path;
use std::process::ExitCode;

use cargo_truce::scaffold::{FeatureSet, PluginKind, PluginSpec, Scaffolder, VendorInfo};

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).filter(|a| a != "truce").collect();

    let cmd = args.first().map(|s| s.as_str()).unwrap_or("help");

    match cmd {
        // Scaffold commands — handled here
        "new" => match cmd_new(&args[1..]) {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("Error: {e}");
                ExitCode::FAILURE
            }
        },
        "new-workspace" => {
            eprintln!(
                "Error: `cargo truce new-workspace` was removed.\n\
                 Use `cargo truce new <name> --workspace <plugin1> [plugin2 ...]` instead."
            );
            ExitCode::FAILURE
        }

        // Build/install commands — forwarded to the engine in lib.rs.
        "install" | "build" | "package" | "remove" | "run" | "screenshot" | "status"
        | "reset-au" | "reset-aax" | "validate" | "doctor" | "log-stream-au" => {
            cargo_truce::run(&args)
        }

        "help" | "--help" | "-h" => {
            print_help();
            ExitCode::SUCCESS
        }
        other => {
            eprintln!("Unknown command: {other}");
            print_help();
            ExitCode::FAILURE
        }
    }
}

fn print_help() {
    eprintln!(
        "\
cargo-truce — build tool for truce audio plugins

Usage: cargo truce <command> [options]

Scaffold:
  new <name> [--instrument] [--midi] [--no-standalone] [--vendor <n>] [--vendor-id <id>]
      Scaffold a new single-plugin project. Defaults include the
      `standalone` feature + `src/main.rs` host; pass --no-standalone
      to skip those (saves the bin entry, the dep, and the file).
      `--vendor` / `--vendor-id` populate `truce.toml` directly;
      omit them to get a `My Company` / `com.mycompany` placeholder
      to edit by hand.

  new <name> --workspace <plugin1> [plugin2 ...] [options]
      Scaffold a workspace with multiple plugins. The first positional
      is the workspace directory; positionals after `--workspace` are
      plugin names (a single plugin produces a workspace-shaped layout
      with one crate).
      Options:
        --vendor <name>             Vendor display name (defaults to PascalCase of <name>)
        --vendor-id <id>            Reverse-domain vendor ID (defaults to com.<name>)
        --instrument                Default all plugins to instrument type
        --midi                      Default all plugins to midi type
        --no-standalone             Skip the standalone feature + host bin in every plugin
        --type:<plugin>=<kind>      Per-plugin type override (effect, instrument, midi)

Build / Install / Package:
  install [--clap] [--vst3] [--vst2] [--lv2] [--au2] [--au3] [--aax] [--user|--system] [--shell] [--debug] [--no-build] [-p <crate>]
      Build and install plugins into the host's plug-in directories.
      Defaults to release because installing usually means audio-
      testing in a DAW — release avoids surprise CPU spikes from
      debug-build DSP under load. This differs from `cargo build`'s
      debug default; pass `--debug` to opt back into the cargo dev
      profile (faster compile, slower DSP — fine for light plugins
      and wiring checks).

      Defaults to whichever formats are in the plugin's Cargo.toml
      default features (typically clap + vst3). VST2, LV2, AU, and AAX
      are opt-in and must be enabled explicitly via these flags or by
      adding them to the plugin's default features.

      Per-format scope is per-user by default on every platform; pass
      `--system` to install into the shared system directories (sudo
      / admin required). AAX and AU v3 are always system-scope, and
      `--user` for these formats falls back silently with a one-line
      note.
      --clap         CLAP only (no sudo)
      --vst3         VST3 only
      --vst2         VST2 only (legacy format — see truce/Cargo.toml note)
      --lv2          LV2 only
      --au2          AU v2 only (.component, macOS only)
      --au3          AU v3 only (.appex, requires Xcode, macOS only)
      --aax          AAX only (requires pre-built template)
      --user         Install into the per-user directories (default).
                     No sudo / admin needed for CLAP, VST3, VST2 (macOS),
                     LV2, and AU v2.
      --system       Install into the system-wide directories. Requires
                     sudo on macOS, admin on Windows.
      --shell        Build dynamic shells (loaded by the DAW) + per-
                     plugin logic dylibs the shells dlopen at runtime.
                     The shell uses the custom `[profile.shell]`
                     (target/shell/); the logic uses release by default,
                     debug if `--debug` is also passed (use `--debug`
                     for fast iteration with `cargo watch -x build`).
      --debug        Compile with the cargo dev profile (faster compile,
                     slower DSP). Don't ship plugins built this way.
                     With `--shell`: selects the *logic* dylib's profile
                     (debug instead of the default release).
      --no-build     Skip build, install existing artifacts
      -p <crate>     Install only the plugin with this cargo crate name
                     (e.g. -p truce-example-gain)

  build [--clap] [--vst3] [--vst2] [--lv2] [--au2] [--au3] [--aax] [-p <crate>] [--shell] [--debug]
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
      --shell        Build dynamic shells (custom `[profile.shell]`,
                     `target/shell/`) plus the per-plugin logic dylibs
                     they dlopen at runtime. Logic profile is release
                     by default, debug if `--debug` is also passed.
      --debug        Cargo dev profile (faster compile, slower DSP).
                     Bundles still stage and sign correctly, but the
                     binary inside is debug-quality — not for shipping.

  package [-p <crate>] [--formats clap,vst3,...] [--user|--system|--ask] [--no-notarize]
      Build, sign, and package plugins into macOS .pkg / Windows .exe
      installers. Output goes to `target/dist/`.

      Scope flags pick how the resulting installer behaves at the
      end user's machine:
      --ask        End user picks at install time via the macOS
                   Installer.app destination page or the Inno Setup
                   \"Choose installation mode\" page (default).
      --user       Hard-lock to user-scope. CLAP/VST3 land in user
                   paths with no admin prompt. AAX, AU v3, and
                   Windows VST2 are kept and installed to the system
                   path (one admin prompt at install time on Windows;
                   on macOS the whole pkg widens to system-domain
                   when AAX/AU v3 are present).
      --system     Hard-lock to system paths (today's behavior).

      Set `[packaging] preferred_scope = \"user\" | \"system\" | \"ask\"`
      in `truce.toml` to override the default for a project.

  run [-p <crate>] [--debug] [-- <args>]
      Build and run a plugin standalone. Pass `--debug` for a
      faster-compile dev-profile build (fine when iterating outside
      a DAW); release otherwise.

  remove [--clap] [--vst3] [--vst2] [--au2] [--au3] [--aax] [--user|--system] [-p <crate>] [-n <name>] [--stale] [--dry-run] [--yes]
      Remove installed plugin bundles for this project.
      Default: all formats, all plugins, both user + system scopes.
      Asks for confirmation. AAX and AU v3 are always system-scope —
      `--user` skips them with the same one-line note as install.
      -p <crate>   Filter by cargo crate name (e.g. -p truce-example-gain)
      -n <name>    Filter by display name (e.g. -n 'Truce Gain')
      --user       Only remove bundles in the per-user directories
      --system     Only remove bundles in the system directories
      --stale      Remove vendor bundles NOT in the current project
                   (renamed/deleted plugins still on the system)
      --dry-run    Show what would be removed without deleting
      --yes        Skip confirmation prompt

Validation / Inspection:
  validate [--auval] [--auval3] [--pluginval] [--clap] [--vst2] [--all] [-p <crate>]
      Run validation tools on installed plugins.
      --auval      AU v2 validation only (macOS)
      --auval3     AU v3 validation only (macOS)
      --pluginval  VST3 validation via pluginval
      --clap       CLAP validation via clap-validator
      --vst2       VST2 dlopen + AEffect probe (macOS-only smoke binary)
      --all        Run all available validators (default)
      -p <crate>   Validate only the plugin with this cargo crate name

  screenshot [-p <crate>] [--name <name>]
      Render a plugin's editor headlessly and save the PNG to
      target/screenshots/<name>.png. With no -p, screenshots every
      plugin in truce.toml. Default name is <bundle_id>_screenshot.

  status
      Show installed plugins and AU registration state.

  doctor
      Check development environment and installed plugins.

Maintenance:
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

  log-stream-au
      macOS-only. Tail AU v3 appex logs live (`os_log` output from the
      Swift wrapper, subsystem `com.truce.au3`). Forward-only — for
      historical entries use `log show --last <duration>` directly.
      Press Ctrl-C to stop.

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

type Res = Result<(), Box<dyn std::error::Error>>;

// ---------------------------------------------------------------------------
// new — single standalone plugin
// ---------------------------------------------------------------------------

fn print_new_help() {
    eprintln!(
        "\
Usage:
  cargo truce new <name> [--instrument] [--midi] [--no-standalone]
                        [--vendor <name>] [--vendor-id <id>]
  cargo truce new <workspace-name> --workspace <plugin1> [plugin2 ...]
                                   [--instrument] [--midi] [--no-standalone]
                                   [--vendor <name>] [--vendor-id <id>]
                                   [--type:<plugin>=<kind> ...]

Scaffold a new truce plugin project.

Single mode (no --workspace):
  Creates a single-crate project at ./<name>/ with one plugin.

Workspace mode (--workspace):
  Creates a workspace at ./<workspace-name>/ with one crate per plugin
  under plugins/<plugin>/. The default plugin kind is `effect`; override
  per-plugin with --type:<plugin>=<effect|instrument|midi>.

Options:
  --instrument            Default plugin kind is `instrument` (synth).
  --midi                  Default plugin kind is `midi`.
  --no-standalone         Skip generating a standalone runner crate.
  --workspace             Multi-plugin workspace mode (positional args
                          after the name are plugin names).
  --vendor <name>         Vendor display name (default: placeholder).
  --vendor-id <id>        Vendor reverse-DNS id (default: placeholder).
  --type:<plugin>=<kind>  Per-plugin kind override (workspace only).
                          <kind> is `effect`, `instrument`, or `midi`.
  -h, --help              Show this message."
    );
}

fn cmd_new(args: &[String]) -> Res {
    if args.iter().any(|a| a == "--help" || a == "-h") {
        print_new_help();
        return Ok(());
    }
    let parsed = parse_new_args(args)?;
    if Path::new(&parsed.name).exists() {
        return Err(format!("Directory '{}' already exists", parsed.name).into());
    }

    let scaffolder = Scaffolder::new();
    let features = FeatureSet {
        standalone: parsed.with_standalone,
    };

    if parsed.workspace_mode {
        scaffold_workspace(&scaffolder, parsed, features)
    } else {
        scaffold_single(&scaffolder, parsed, features)
    }
}

/// Parsed `cargo truce new` flags. Single + workspace modes share
/// one parser; the validity of mode-specific combinations is
/// asserted later by the per-mode entrypoints.
struct NewArgs {
    name: String,
    plugin_names: Vec<String>,
    default_kind: PluginKind,
    vendor_name: Option<String>,
    vendor_id: Option<String>,
    type_overrides: Vec<(String, PluginKind)>,
    with_standalone: bool,
    workspace_mode: bool,
}

fn parse_new_args(args: &[String]) -> Result<NewArgs, Box<dyn std::error::Error>> {
    let mut name: Option<String> = None;
    let mut plugin_names: Vec<String> = Vec::new();
    let mut default_kind = PluginKind::Effect;
    let mut vendor_name: Option<String> = None;
    let mut vendor_id: Option<String> = None;
    let mut type_overrides: Vec<(String, PluginKind)> = Vec::new();
    let mut with_standalone = true;
    let mut workspace_mode = false;

    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--workspace" => workspace_mode = true,
            "--instrument" => default_kind = PluginKind::Instrument,
            "--midi" => default_kind = PluginKind::Midi,
            "--no-standalone" => with_standalone = false,
            "--vendor" => {
                vendor_name = Some(iter.next().ok_or("--vendor requires a value")?.clone());
            }
            "--vendor-id" => {
                vendor_id = Some(iter.next().ok_or("--vendor-id requires a value")?.clone());
            }
            s if s.starts_with("--type:") => {
                let rest = &s["--type:".len()..];
                let (pname, kind_str) = rest.split_once('=').ok_or_else(|| {
                    format!("Invalid --type flag: {s} (expected --type:<plugin>=<kind>)")
                })?;
                let kind = PluginKind::parse(kind_str)?;
                type_overrides.push((pname.to_string(), kind));
            }
            s if s.starts_with('-') => {
                return Err(format!("Unknown option: {s}").into());
            }
            s if name.is_none() => name = Some(s.to_string()),
            s => plugin_names.push(s.to_string()),
        }
    }

    let name = name.ok_or(
        "Usage:\n  \
         cargo truce new <name> [--instrument] [--midi] [--no-standalone]\n  \
         cargo truce new <workspace-name> --workspace <plugin1> [plugin2 ...] [options]",
    )?;

    if !workspace_mode && !plugin_names.is_empty() {
        return Err(format!(
            "extra positional arguments: {}\n\
             To scaffold a multi-plugin workspace, pass --workspace.",
            plugin_names.join(", ")
        )
        .into());
    }

    Ok(NewArgs {
        name,
        plugin_names,
        default_kind,
        vendor_name,
        vendor_id,
        type_overrides,
        with_standalone,
        workspace_mode,
    })
}

fn scaffold_single(scaffolder: &Scaffolder, parsed: NewArgs, features: FeatureSet) -> Res {
    // `--type:` overrides only make sense across multiple plugins;
    // reject in single mode so a typo (e.g., the user forgot
    // `--workspace`) surfaces as an error instead of silently
    // dropping the override.
    if !parsed.type_overrides.is_empty() {
        return Err(
            "--type:<plugin>=<kind> only applies to --workspace scaffolds (multiple plugins)."
                .into(),
        );
    }

    let vendor = match (parsed.vendor_name, parsed.vendor_id) {
        (Some(name), Some(id)) => VendorInfo { name, id },
        (Some(name), None) => VendorInfo {
            name,
            id: VendorInfo::placeholder().id,
        },
        (None, Some(id)) => VendorInfo {
            name: VendorInfo::placeholder().name,
            id,
        },
        (None, None) => VendorInfo::placeholder(),
    };

    let plugin = PluginSpec {
        name: parsed.name.clone(),
        kind: parsed.default_kind,
    };
    scaffolder.single(Path::new(&parsed.name), &plugin, features, &vendor)?;

    eprintln!("Created {}/", parsed.name);
    eprintln!();
    eprintln!("  cd {}", parsed.name);
    eprintln!("  cargo truce install --clap      # build + install CLAP");
    eprintln!("  cargo truce install              # all formats in default features");
    eprintln!("  cargo truce package              # signed .pkg / .exe installer in target/dist/");
    eprintln!("  cargo truce doctor               # check environment");
    eprintln!();
    eprintln!("Edit src/lib.rs to add your DSP.");
    eprintln!("Edit truce.toml to configure vendor info and AU metadata.");
    eprintln!("Edit .cargo/config.toml to set signing identities and SDK paths.");
    eprintln!();
    if cfg!(target_os = "windows") {
        eprintln!("Windows: `cargo truce install` writes to system directories and needs");
        eprintln!("an Administrator command prompt.");
        eprintln!();
    }
    Ok(())
}

fn scaffold_workspace(scaffolder: &Scaffolder, parsed: NewArgs, features: FeatureSet) -> Res {
    if parsed.plugin_names.is_empty() {
        return Err("--workspace requires at least one plugin name.\n\
            Usage: cargo truce new <workspace-name> --workspace <plugin1> [plugin2 ...]"
            .into());
    }

    // Duplicate plugin names → a workspace where two crates would
    // try to live at the same `plugins/<name>/` path. Caught here
    // for a clearer error than "directory already exists" mid-run.
    let mut seen = std::collections::HashSet::new();
    for pn in &parsed.plugin_names {
        if !seen.insert(pn.as_str()) {
            return Err(format!("Duplicate plugin name: '{pn}'").into());
        }
    }

    // `--type:foo=instrument` must reference an actual plugin name
    // — typos here would silently apply the default kind to every
    // plugin, which is exactly the trap the override is supposed
    // to avoid.
    for (override_name, _) in &parsed.type_overrides {
        if !parsed.plugin_names.contains(override_name) {
            return Err(format!(
                "--type:{override_name}=... does not match any plugin name. \
                 Available plugins: {}",
                parsed.plugin_names.join(", "),
            )
            .into());
        }
    }

    let plugins: Vec<PluginSpec> = parsed
        .plugin_names
        .iter()
        .map(|pn| {
            let kind = parsed
                .type_overrides
                .iter()
                .find(|(n, _)| n == pn)
                .map(|(_, k)| *k)
                .unwrap_or(parsed.default_kind);
            PluginSpec {
                name: pn.clone(),
                kind,
            }
        })
        .collect();

    let derived = VendorInfo::derive_from_workspace_name(&parsed.name);
    let vendor = VendorInfo {
        name: parsed.vendor_name.unwrap_or(derived.name),
        id: parsed.vendor_id.unwrap_or(derived.id),
    };

    scaffolder.workspace(
        Path::new(&parsed.name),
        &parsed.name,
        &plugins,
        features,
        &vendor,
    )?;

    eprintln!("Created {}/ with {} plugins:", parsed.name, plugins.len());
    for p in &plugins {
        let kind_label = match p.kind {
            PluginKind::Effect => "effect",
            PluginKind::Instrument => "instrument",
            PluginKind::Midi => "midi",
        };
        eprintln!("  plugins/{:<20} ({})", p.name, kind_label);
    }
    eprintln!();
    eprintln!("  cd {}", parsed.name);
    eprintln!("  cargo truce install --clap      # build + install all as CLAP");
    eprintln!("  cargo truce install              # all formats in default features");
    eprintln!("  cargo truce package              # signed .pkg / .exe installer in target/dist/");
    eprintln!("  cargo truce doctor               # check environment");
    eprintln!();
    eprintln!("Edit plugins/*/src/lib.rs to add your DSP.");
    eprintln!("Edit truce.toml to configure vendor info and AU metadata.");
    eprintln!("Edit .cargo/config.toml to set signing identities and SDK paths.");
    eprintln!();
    if cfg!(target_os = "windows") {
        eprintln!("Windows: `cargo truce install` writes to system directories and needs");
        eprintln!("an Administrator command prompt.");
        eprintln!();
    }
    Ok(())
}
