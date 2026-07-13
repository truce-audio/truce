//! cargo-truce - build tool for truce audio plugins.
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

use cargo_truce::scaffold::{
    FeatureSet, PluginKind, PluginSpec, Scaffolder, Statefulness, VendorInfo,
};

fn main() -> ExitCode {
    // Tell objc2 to reuse an already-registered Obj-C class with the
    // same name instead of panicking. Required when multiple plugin
    // dylibs (each containing its own copy of raw-window-metal's
    // "RawWindowMetalLayer" subclass) load into the same host process
    // - e.g. Pro Tools loading two AAX plugins built with truce. See
    // raw-window-metal issue #29 and the `UNSAFE_OBJC2_ALLOW_CLASS_OVERRIDE`
    // check in objc2's src/__macro_helpers/define_class.rs. The env
    // var is read at compile time by objc2; setting it here means
    // every `cargo` cargo-truce spawns picks it up, so plugin authors
    // don't need to know it exists. Harmless on Linux / Windows (the
    // env var is only consumed by objc2, which only builds on Apple).
    //
    // SAFETY: We're at process entry on the main thread, before any
    // other thread can observe the environment.
    unsafe {
        std::env::set_var("UNSAFE_OBJC2_ALLOW_CLASS_OVERRIDE", "1");
    }

    let args: Vec<String> = std::env::args().skip(1).filter(|a| a != "truce").collect();

    let cmd = args.first().map_or("help", std::string::String::as_str);

    match cmd {
        // Scaffold commands - handled here
        "new" => match cmd_new(&args[1..]) {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("Error: {e}");
                ExitCode::FAILURE
            }
        },
        // Build/install commands - forwarded to the engine in lib.rs.
        "install" | "build" | "package" | "uninstall" | "run" | "screenshot" | "status"
        | "reset-au" | "reset-aax" | "validate" | "doctor" | "log-stream-au" | "preset" => {
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

#[allow(clippy::too_many_lines)]
fn print_help() {
    eprintln!(
        "\
cargo-truce - build tool for truce audio plugins

Usage: cargo truce <command> [options]

Scaffold:
  new <name> [--instrument] [--midi] [--pure|--stateful] [--no-standalone] [--vendor <n>] [--vendor-id <id>]
      Scaffold a new single-plugin project. Defaults include the
      `standalone` feature + `src/main.rs` host; pass --no-standalone
      to skip those (saves the bin entry, the dep, and the file).
      `--stateful` (default) implements `PluginLogic` where the plugin
      struct is its own DSP state (`type DspState = Self`), pre-wired;
      `--pure` implements the stateless `PurePluginLogic`.
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
        --pure | --stateful         PurePluginLogic vs PluginLogic (type DspState = Self) (default)
        --no-standalone             Skip the standalone feature + host bin in every plugin
        --type:<plugin>=<kind>      Per-plugin type override (effect, instrument, midi)

Build / Install / Package:
  install [--clap] [--vst3] [--vst2] [--lv2] [--au2] [--au3] [--aax] [--user|--system] [--shell] [--debug] [--no-build] [-p <crate>] [--target-cpu <value>]
      Build and install plugins into the host's plug-in directories.
      Defaults to release because installing usually means audio-
      testing in a DAW - release avoids surprise CPU spikes from
      debug-build DSP under load. This differs from `cargo build`'s
      debug default; pass `--debug` to opt back into the cargo dev
      profile (faster compile, slower DSP - fine for light plugins
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
      --vst2         VST2 only (legacy format)
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
      --target-cpu <value>
                     Override the x86_64 default of `-C target-cpu=x86-64-v3`.
                     Accepts baseline|v2|v3|v4|native or any literal
                     rustc target-cpu name (apple-m1, znver4, ...).
                     See `cargo truce build --help` for the full
                     description and per-value caveats.

  build [--clap] [--vst3] [--vst2] [--lv2] [--au2] [--au3] [--aax] [-p <crate>] [--shell] [--debug] [--target-cpu <value>]
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
                     binary inside is debug-quality - not for shipping.
      --target-cpu <value>
                     Override the x86_64 default of `-C target-cpu=x86-64-v3`.
                     baseline|v2|v3|v4|native or any literal rustc
                     target-cpu name. See `cargo truce build --help` for
                     the per-value description.

  package [-p <crate>] [--formats clap,vst3,...] [--user|--system|--ask] [--no-notarize] [--target-cpu <value>]
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

      --target-cpu <value>
                     Override the x86_64 default of `-C target-cpu=x86-64-v3`.
                     baseline|v2|v3|v4|native or any literal rustc
                     target-cpu name. See `cargo truce build --help`.

  run [-p <crate>] [--debug] [--target-cpu <value>] [-- <args>]
      Build and run a plugin standalone. Pass `--debug` for a
      faster-compile dev-profile build (fine when iterating outside
      a DAW); release otherwise. `--target-cpu` mirrors `build`'s flag
      (x86_64 defaults to x86-64-v3).

  uninstall [--clap] [--vst3] [--vst2] [--au2] [--au3] [--aax] [--user|--system] [-p <crate>] [-n <name>] [--stale] [--dry-run] [--yes]
      Uninstall plugin bundles for this project.
      Default: all formats, all plugins, both user + system scopes.
      Asks for confirmation. AAX and AU v3 are always system-scope -
      `--user` skips them with the same one-line note as install.
      -p <crate>   Filter by cargo crate name (e.g. -p truce-example-gain)
      -n <name>    Filter by display name (e.g. -n 'Truce Gain')
      --user       Only uninstall bundles in the per-user directories
      --system     Only uninstall bundles in the system directories
      --stale      Uninstall vendor bundles NOT in the current project
                   (renamed/deleted plugins still on the system)
      --dry-run    Show what would be uninstalled without deleting
      --yes        Skip confirmation prompt

Presets:
  preset <list|init|convert|import|export|pull> [...]
      Preset library management, format conversion, and the in-DAW
      authoring round-trip (`pull` harvests host-saved presets into
      the crate's presets/ library). `cargo truce preset --help`
      for the full surface.

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
      Scan installed plugin bundles (filesystem-only; for an AU
      registry check use `cargo truce validate --auval`).

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
      Swift wrapper, subsystem `com.truce.au3`). Forward-only - for
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

use cargo_truce::{CargoTruceError, Res};

// ---------------------------------------------------------------------------
// new - single standalone plugin
// ---------------------------------------------------------------------------

fn print_new_help() {
    eprintln!(
        "\
Usage:
  cargo truce new <name> [--instrument] [--midi] [--pure|--stateful]
                        [--no-standalone] [--vendor <name>] [--vendor-id <id>]
  cargo truce new <workspace-name> --workspace <plugin1> [plugin2 ...]
                                   [--instrument] [--midi] [--pure|--stateful]
                                   [--no-standalone] [--vendor <name>] [--vendor-id <id>]
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
  --pure                  Implement `PurePluginLogic` - params only, no
                          DSP state.
  --stateful              Implement `PluginLogic` where the plugin struct
                          is its own DSP state (`type DspState = Self`)
                          with a `state` argument on `process`, pre-wired.
                          This is the default.
  --no-standalone         Skip generating a standalone runner crate.
  --workspace             Multi-plugin workspace mode (positional args
                          after the name are plugin names).
  --vendor <name>         Vendor display name (default: placeholder).
  --vendor-id <id>        Vendor reverse-DNS id (default: placeholder).
  --type:<plugin>=<kind>  Per-plugin kind override (workspace only).
                          <kind> is `effect`, `instrument`, or `midi`.
  --github                Pin truce-* deps to the truce git repo at
                          tag vX.Y.Z instead of the default crates.io
                          version pin. Use this if you're scaffolding
                          against an unreleased truce checkout.
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

    let scaffolder = Scaffolder::new(parsed.use_registry);
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
    /// `PluginLogic` where the plugin struct is its own DSP state
    /// (`type DspState = Self`, default) vs the stateless
    /// `PurePluginLogic`. Set by `--stateful` / `--pure`; applies to
    /// every plugin in the scaffold.
    default_statefulness: Statefulness,
    vendor_name: Option<String>,
    vendor_id: Option<String>,
    type_overrides: Vec<(String, PluginKind)>,
    with_standalone: bool,
    workspace_mode: bool,
    /// Default `true` — scaffolds pin truce-* deps to crates.io
    /// (`version = "X.Y"`). Setting `--github` flips this to
    /// `false`, falling back to the pre-crates.io form
    /// (`git = "...", tag = "vX.Y.Z"`). Both branches are
    /// supported during the migration; the git branch and this
    /// flag can be removed once the registry path is the only
    /// one in use.
    use_registry: bool,
}

fn parse_new_args(args: &[String]) -> Result<NewArgs, CargoTruceError> {
    let mut name: Option<String> = None;
    let mut plugin_names: Vec<String> = Vec::new();
    let mut default_kind = PluginKind::Effect;
    let mut default_statefulness = Statefulness::Stateful;
    let mut vendor_name: Option<String> = None;
    let mut vendor_id: Option<String> = None;
    let mut type_overrides: Vec<(String, PluginKind)> = Vec::new();
    let mut with_standalone = true;
    let mut workspace_mode = false;
    let mut use_registry = true;

    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--workspace" => workspace_mode = true,
            "--instrument" => default_kind = PluginKind::Instrument,
            "--midi" => default_kind = PluginKind::Midi,
            "--pure" => default_statefulness = Statefulness::Pure,
            "--stateful" => default_statefulness = Statefulness::Stateful,
            "--no-standalone" => with_standalone = false,
            "--github" => use_registry = false,
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
                let kind: PluginKind = kind_str.parse()?;
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
         cargo truce new <name> [--instrument] [--midi] [--pure|--stateful] [--no-standalone]\n  \
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
        default_statefulness,
        vendor_name,
        vendor_id,
        type_overrides,
        with_standalone,
        workspace_mode,
        use_registry,
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
        statefulness: parsed.default_statefulness,
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
    // - typos here would silently apply the default kind to every
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
                .map_or(parsed.default_kind, |(_, k)| *k);
            PluginSpec {
                name: pn.clone(),
                kind,
                statefulness: parsed.default_statefulness,
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
