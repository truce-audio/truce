//! cargo-truce — build tool for truce audio plugins.
//!
//! Install:
//!   cargo install --git https://github.com/truce-audio/truce cargo-truce
//!
//! Usage:
//!   cargo truce new my-plugin          # scaffold a new plugin project
//!   cargo truce new studio --workspace gain reverb synth
//!   cargo truce install                # build + bundle + sign + install
//!   cargo truce install --clap         # single format
//!   cargo truce validate               # run auval, auval3, pluginval, clap-validator
//!   cargo truce doctor                 # check environment

use std::fs;
use std::path::Path;
use std::process::ExitCode;

use cargo_truce::scaffold::{self, PluginKind, PluginSpec};

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
        "install" | "build" | "package" | "remove" | "run" | "screenshot" | "test" | "status"
        | "clean" | "reset-au" | "reset-aax" | "validate" | "doctor" | "log-stream-au" => {
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
  new <name> [--instrument] [--midi] [--no-standalone]
      Scaffold a new single-plugin project. Defaults include the
      `standalone` feature + `src/main.rs` host; pass --no-standalone
      to skip those (saves the bin entry, the dep, and the file).

  new <name> --workspace <plugin1> [plugin2 ...] [options]
      Scaffold a workspace with multiple plugins. The first positional
      is the workspace directory; positionals after `--workspace` are
      plugin names (a single plugin produces a workspace-shaped layout
      with one crate).
      Options:
        --vendor <name>             Vendor display name
        --vendor-id <id>            Reverse-domain vendor ID
        --instrument                Default all plugins to instrument type
        --midi                      Default all plugins to midi type
        --no-standalone             Skip the standalone feature + host bin in every plugin
        --type:<plugin>=<kind>      Per-plugin type override (effect, instrument, midi)

Build / Install / Package:
  install [--clap] [--vst3] [--vst2] [--au2] [--au3] [--aax] [--user|--system] [--shell] [--debug] [--no-build] [-p <crate>]
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

      Per-format scope is per-user by default on every platform; pass
      `--system` to install into the shared system directories (sudo
      / admin required). AAX and AU v3 are always system-scope, and
      `--user` for these formats falls back silently with a one-line
      note.
      --clap         CLAP only (no sudo)
      --vst3         VST3 only
      --vst2         VST2 only (legacy format — see truce/Cargo.toml note)
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

  test
      Run all plugin tests (render, state, params, metadata).

  status
      Show installed plugins and AU registration state.

  doctor
      Check development environment and installed plugins.

Maintenance:
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

fn cmd_new(args: &[String]) -> Res {
    // Single-plugin AND multi-plugin workspace scaffolding share one
    // command: `cargo truce new`. Without `--workspace`, the first
    // positional is the project name, additional positionals are an
    // error. With `--workspace`, the first positional is the
    // workspace directory name and any additional positionals are
    // plugin names; a single positional + `--workspace` produces a
    // workspace-shaped layout with one plugin.
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

    // Single-plugin path: extra positionals are an error (probably
    // the user forgot --workspace).
    if !workspace_mode && !plugin_names.is_empty() {
        return Err(format!(
            "extra positional arguments: {}\n\
             To scaffold a multi-plugin workspace, pass --workspace.",
            plugin_names.join(", ")
        )
        .into());
    }

    if Path::new(&name).exists() {
        return Err(format!("Directory '{name}' already exists").into());
    }

    if workspace_mode {
        scaffold_workspace(
            &name,
            &plugin_names,
            default_kind,
            &type_overrides,
            vendor_name,
            vendor_id,
            with_standalone,
        )
    } else {
        // The plugin-type override flags only make sense in workspace
        // mode where multiple plugins exist; reject early in single
        // mode so users notice typos.
        if !type_overrides.is_empty() {
            return Err(
                "--type:<plugin>=<kind> only applies to --workspace scaffolds (multiple plugins)."
                    .into(),
            );
        }
        if vendor_name.is_some() || vendor_id.is_some() {
            return Err(
                "--vendor / --vendor-id only apply to --workspace scaffolds; \
                 single-plugin scaffolds use placeholder vendor info you edit \
                 in truce.toml."
                    .into(),
            );
        }
        scaffold_single(&name, default_kind, with_standalone)
    }
}

fn scaffold_single(name: &str, kind: PluginKind, with_standalone: bool) -> Res {
    let struct_name = scaffold::to_pascal_case(name);

    fs::create_dir_all(format!("{name}/src"))?;
    fs::create_dir_all(format!("{name}/.cargo"))?;
    fs::write(
        format!("{name}/Cargo.toml"),
        scaffold::plugin_cargo_toml_standalone(name, with_standalone),
    )?;
    fs::write(
        format!("{name}/build.rs"),
        "fn main() { truce_build::emit_plugin_env(); }\n",
    )?;
    fs::write(
        format!("{name}/src/lib.rs"),
        scaffold::plugin_lib_rs(&struct_name, kind),
    )?;
    if with_standalone {
        fs::write(
            format!("{name}/src/main.rs"),
            scaffold::plugin_main_rs(name),
        )?;
    }
    fs::write(format!("{name}/.gitignore"), scaffold::gitignore())?;
    fs::write(
        format!("{name}/.cargo/config.toml"),
        scaffold::cargo_config_toml(),
    )?;

    let plugin = PluginSpec {
        name: name.to_string(),
        kind,
    };
    let plugins = [plugin];
    let fourcc_map = scaffold::resolve_fourccs(&plugins);
    fs::write(
        format!("{name}/truce.toml"),
        scaffold::truce_toml(
            "My Company",
            "com.mycompany",
            &plugins,
            name,
            &fourcc_map,
            false,
        ),
    )?;

    eprintln!("Created {name}/");
    eprintln!();
    eprintln!("  cd {name}");
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

fn scaffold_workspace(
    workspace_name: &str,
    plugin_names: &[String],
    default_kind: PluginKind,
    type_overrides: &[(String, PluginKind)],
    vendor_name: Option<String>,
    vendor_id: Option<String>,
    with_standalone: bool,
) -> Res {
    if plugin_names.is_empty() {
        return Err("--workspace requires at least one plugin name.\n\
            Usage: cargo truce new <workspace-name> --workspace <plugin1> [plugin2 ...]"
            .into());
    }

    // Check for duplicate plugin names
    let mut seen = std::collections::HashSet::new();
    for pn in plugin_names {
        if !seen.insert(pn.as_str()) {
            return Err(format!("Duplicate plugin name: '{pn}'").into());
        }
    }

    // Check that all --type: overrides reference actual plugin names
    for (override_name, _) in type_overrides {
        if !plugin_names.contains(override_name) {
            return Err(format!(
                "--type:{override_name}=... does not match any plugin name. \
                 Available plugins: {}",
                plugin_names.join(", "),
            )
            .into());
        }
    }

    // Build plugin specs
    let plugins: Vec<PluginSpec> = plugin_names
        .iter()
        .map(|pn| {
            let kind = type_overrides
                .iter()
                .find(|(n, _)| n == pn)
                .map(|(_, k)| *k)
                .unwrap_or(default_kind);
            PluginSpec {
                name: pn.clone(),
                kind,
            }
        })
        .collect();

    let fourcc_map = scaffold::resolve_fourccs(&plugins);
    let vendor = vendor_name.unwrap_or_else(|| scaffold::to_pascal_case(workspace_name));
    let vid = vendor_id.unwrap_or_else(|| format!("com.{}", workspace_name.replace('-', "")));

    for p in &plugins {
        fs::create_dir_all(format!("{workspace_name}/plugins/{}/src", p.name))?;
    }
    fs::create_dir_all(format!("{workspace_name}/.cargo"))?;

    fs::write(
        format!("{workspace_name}/Cargo.toml"),
        scaffold::workspace_cargo_toml(workspace_name, &plugins, with_standalone),
    )?;

    fs::write(
        format!("{workspace_name}/truce.toml"),
        scaffold::truce_toml(&vendor, &vid, &plugins, workspace_name, &fourcc_map, true),
    )?;

    fs::write(
        format!("{workspace_name}/.gitignore"),
        scaffold::gitignore(),
    )?;

    fs::write(
        format!("{workspace_name}/.cargo/config.toml"),
        scaffold::cargo_config_toml(),
    )?;

    for p in &plugins {
        let crate_name = format!("{workspace_name}-{}", p.name);
        let struct_name = scaffold::to_pascal_case(&p.name);

        fs::write(
            format!("{workspace_name}/plugins/{}/Cargo.toml", p.name),
            scaffold::plugin_cargo_toml_workspace(&crate_name, with_standalone),
        )?;

        fs::write(
            format!("{workspace_name}/plugins/{}/build.rs", p.name),
            "fn main() { truce_build::emit_plugin_env(); }\n",
        )?;

        fs::write(
            format!("{workspace_name}/plugins/{}/src/lib.rs", p.name),
            scaffold::plugin_lib_rs(&struct_name, p.kind),
        )?;

        if with_standalone {
            fs::write(
                format!("{workspace_name}/plugins/{}/src/main.rs", p.name),
                scaffold::plugin_main_rs(&crate_name),
            )?;
        }
    }

    eprintln!("Created {workspace_name}/ with {} plugins:", plugins.len());
    for p in &plugins {
        let kind_label = match p.kind {
            PluginKind::Effect => "effect",
            PluginKind::Instrument => "instrument",
            PluginKind::Midi => "midi",
        };
        eprintln!("  plugins/{:<20} ({})", p.name, kind_label);
    }
    eprintln!();
    eprintln!("  cd {workspace_name}");
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
