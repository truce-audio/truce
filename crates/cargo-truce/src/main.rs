//! cargo-truce — build tool for truce audio plugins.
//!
//! Install:
//!   cargo install --git https://github.com/truce-audio/truce cargo-truce
//!
//! Usage:
//!   cargo truce new my-plugin          # scaffold a new plugin project
//!   cargo truce new-workspace studio gain reverb synth
//!   cargo truce install                # build + bundle + sign + install
//!   cargo truce install --clap         # single format
//!   cargo truce validate               # run auval, auval3, pluginval, clap-validator
//!   cargo truce doctor                 # check environment

mod scaffold;

use std::fs;
use std::path::Path;
use std::process::ExitCode;

use scaffold::{PluginKind, PluginSpec};

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
        "new-workspace" => match cmd_new_workspace(&args[1..]) {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("Error: {e}");
                ExitCode::FAILURE
            }
        },

        // Build/install commands — forwarded to truce-xtask
        "install" | "build" | "package" | "remove" | "run" | "screenshot" | "test" | "status"
        | "clean" | "reset-au" | "reset-aax" | "validate" | "doctor" | "log-stream-au" => {
            truce_xtask::run(&args)
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

USAGE:
  cargo truce new <name> [--instrument] [--midi]
      Scaffold a new single-plugin project.

  cargo truce new-workspace <name> <plugin1> [plugin2 ...] [options]
      Scaffold a workspace with multiple plugins.
      Options:
        --vendor <name>             Vendor display name
        --vendor-id <id>            Reverse-domain vendor ID
        --instrument                Default all plugins to instrument type
        --midi                      Default all plugins to midi type
        --type:<plugin>=<kind>      Per-plugin type override (effect, instrument, midi)

  cargo truce install [--clap] [--vst3] [--vst2] [--au2] [--au3] [--aax] [-p <crate>]
      Build, bundle, sign, and install plugins.

  cargo truce build [--clap] [--vst3] [--vst2] [--lv2] [--au2] [--au3]
                    [--aax] [-p <crate>] [--hot-reload]
      Build signed per-format bundles into target/bundles/ without
      installing. No format flags → every format in the project's
      default features.

  cargo truce validate [--auval] [--auval3] [--pluginval] [--clap]
                       [--vst2] [--all] [-p <crate>]
      Run plugin validators against installed bundles. No flags → run
      all available (auval, auval3, pluginval, clap-validator, vst2);
      missing tools are skipped with a hint. `--vst2` runs an in-tree
      `dlopen` + AEffect probe (macOS-only smoke binary; VST2 has no
      industry-standard validator).

  cargo truce test
      Run in-process regression tests.

  cargo truce screenshot [-p <crate>] [--name <name>]
      Render a plugin's editor headlessly and save the PNG to
      target/screenshots/. With no -p, renders every plugin in
      truce.toml.

  cargo truce clean [--all]
      Run `cargo clean` while preserving `target/dist/` (signed /
      notarized installers — expensive to rebuild). Pass `--all` to
      wipe everything, equivalent to a bare `cargo clean`. Does not
      touch installed plugin bundles or AU / AAX host caches — see
      `cargo truce remove`, `reset-au`, `reset-aax` for those.

  cargo truce reset-au
      macOS-only. Flush Audio Unit caches (AudioUnitCache, GarageBand
      / Logic / Reaper plists), reset pluginkit registrations, and
      restart `pkd` + `AudioComponentRegistrar`. Use when AU bundles
      are stuck serving stale binaries.

  cargo truce reset-aax
      macOS-only. Wipe this vendor's entries from the Pro Tools AAX
      cache. Pro Tools re-scans AAX plugins on next launch.

  cargo truce status
      Show installed plugins.

  cargo truce doctor
      Check development environment.

  cargo truce log-stream-au
      Tail AU v3 appex logs live (macOS-only, forward-only).

GLOBAL FLAGS (accepted by every subcommand):
  -v, --verbose
      Echo per-format build banners, per-bundle paths, and the full
      `codesign` chatter. Default output is the `Built:` /
      `Installed:` / `Skipped:` summary plus one `✓ signed <bundle>`
      line per codesign call.
"
    );
}

type Res = Result<(), Box<dyn std::error::Error>>;

// ---------------------------------------------------------------------------
// new — single standalone plugin
// ---------------------------------------------------------------------------

fn cmd_new(args: &[String]) -> Res {
    let mut name: Option<String> = None;
    let mut kind = PluginKind::Effect;

    for arg in args {
        match arg.as_str() {
            "--instrument" => kind = PluginKind::Instrument,
            "--midi" => kind = PluginKind::Midi,
            s if !s.starts_with('-') && name.is_none() => name = Some(s.to_string()),
            other => return Err(format!("Unknown argument: {other}").into()),
        }
    }

    let name = name.ok_or("Usage: cargo truce new <name> [--instrument] [--midi]")?;

    if Path::new(&name).exists() {
        return Err(format!("Directory '{name}' already exists").into());
    }

    let struct_name = scaffold::to_pascal_case(&name);

    fs::create_dir_all(format!("{name}/src"))?;
    fs::create_dir_all(format!("{name}/.cargo"))?;
    fs::write(
        format!("{name}/Cargo.toml"),
        scaffold::plugin_cargo_toml_standalone(&name),
    )?;
    fs::write(
        format!("{name}/build.rs"),
        "fn main() { truce_build::emit_plugin_env(); }\n",
    )?;
    fs::write(
        format!("{name}/src/lib.rs"),
        scaffold::plugin_lib_rs(&struct_name, kind),
    )?;
    fs::write(format!("{name}/.gitignore"), scaffold::gitignore())?;
    fs::write(
        format!("{name}/.cargo/config.toml"),
        scaffold::cargo_config_toml(),
    )?;

    // truce.toml — single plugin
    let plugin = PluginSpec {
        name: name.clone(),
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
            &name,
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

// ---------------------------------------------------------------------------
// new-workspace — multi-plugin workspace
// ---------------------------------------------------------------------------

fn cmd_new_workspace(args: &[String]) -> Res {
    let mut workspace_name: Option<String> = None;
    let mut plugin_names: Vec<String> = Vec::new();
    let mut default_kind = PluginKind::Effect;
    let mut vendor_name: Option<String> = None;
    let mut vendor_id: Option<String> = None;
    let mut type_overrides: Vec<(String, PluginKind)> = Vec::new();

    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--instrument" => default_kind = PluginKind::Instrument,
            "--midi" => default_kind = PluginKind::Midi,
            "--vendor" => {
                vendor_name = Some(iter.next().ok_or("--vendor requires a value")?.clone());
            }
            "--vendor-id" => {
                vendor_id = Some(iter.next().ok_or("--vendor-id requires a value")?.clone());
            }
            s if s.starts_with("--type:") => {
                // --type:gain=instrument
                let rest = &s["--type:".len()..];
                let (pname, kind_str) = rest.split_once('=').ok_or_else(|| {
                    format!("Invalid --type flag: {s} (expected --type:<plugin>=<kind>)")
                })?;
                let kind = PluginKind::from_str(kind_str)?;
                type_overrides.push((pname.to_string(), kind));
            }
            s if s.starts_with('-') => {
                return Err(format!("Unknown option: {s}").into());
            }
            s if workspace_name.is_none() => {
                workspace_name = Some(s.to_string());
            }
            s => {
                plugin_names.push(s.to_string());
            }
        }
    }

    let workspace_name = workspace_name
        .ok_or("Usage: cargo truce new-workspace <name> <plugin1> [plugin2 ...] [options]")?;

    if plugin_names.is_empty() {
        return Err("At least one plugin name is required.\n\
            Usage: cargo truce new-workspace <name> <plugin1> [plugin2 ...]"
            .into());
    }

    if Path::new(&workspace_name).exists() {
        return Err(format!("Directory '{workspace_name}' already exists").into());
    }

    // Check for duplicate plugin names
    let mut seen = std::collections::HashSet::new();
    for pn in &plugin_names {
        if !seen.insert(pn.as_str()) {
            return Err(format!("Duplicate plugin name: '{pn}'").into());
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

    // Resolve fourcc codes (handles collisions automatically)
    let fourcc_map = scaffold::resolve_fourccs(&plugins);

    // Check that all --type: overrides reference actual plugin names
    for (override_name, _) in &type_overrides {
        if !plugin_names.contains(override_name) {
            return Err(format!(
                "--type:{override_name}=... does not match any plugin name. \
                 Available plugins: {}",
                plugin_names.join(", "),
            )
            .into());
        }
    }

    let vendor = vendor_name.unwrap_or_else(|| scaffold::to_pascal_case(&workspace_name));
    let vid = vendor_id.unwrap_or_else(|| format!("com.{}", workspace_name.replace('-', "")));

    // Create directory structure
    for p in &plugins {
        fs::create_dir_all(format!("{workspace_name}/plugins/{}/src", p.name))?;
    }
    fs::create_dir_all(format!("{workspace_name}/.cargo"))?;

    // Root Cargo.toml
    fs::write(
        format!("{workspace_name}/Cargo.toml"),
        scaffold::workspace_cargo_toml(&workspace_name, &plugins),
    )?;

    // truce.toml
    fs::write(
        format!("{workspace_name}/truce.toml"),
        scaffold::truce_toml(&vendor, &vid, &plugins, &workspace_name, &fourcc_map, true),
    )?;

    // .gitignore
    fs::write(
        format!("{workspace_name}/.gitignore"),
        scaffold::gitignore(),
    )?;

    // .cargo/config.toml — per-developer signing + SDK paths (gitignored)
    fs::write(
        format!("{workspace_name}/.cargo/config.toml"),
        scaffold::cargo_config_toml(),
    )?;

    // Per-plugin files
    for p in &plugins {
        let crate_name = format!("{workspace_name}-{}", p.name);
        let struct_name = scaffold::to_pascal_case(&p.name);

        fs::write(
            format!("{workspace_name}/plugins/{}/Cargo.toml", p.name),
            scaffold::plugin_cargo_toml_workspace(&crate_name),
        )?;

        fs::write(
            format!("{workspace_name}/plugins/{}/build.rs", p.name),
            "fn main() { truce_build::emit_plugin_env(); }\n",
        )?;

        fs::write(
            format!("{workspace_name}/plugins/{}/src/lib.rs", p.name),
            scaffold::plugin_lib_rs(&struct_name, p.kind),
        )?;
    }

    // Print summary
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
