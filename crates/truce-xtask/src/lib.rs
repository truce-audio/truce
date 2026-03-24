mod templates;

use serde::Deserialize;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

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
        "install" => cmd_install(&args[1..]),
        "build" => cmd_build(&args[1..]),
        "run" => cmd_run(&args[1..]),
        "new" => cmd_new(&args[1..]),
        "test" => cmd_test(),
        "status" => cmd_status(),
        "clean" => cmd_clean(),
        "nuke" => cmd_nuke(&args[1..]),
        "validate" => cmd_validate(&args[1..]),
        "doctor" => cmd_doctor(),
        "log" => cmd_log(),
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
  install [--clap] [--vst3] [--vst2] [--au2] [--au3] [--aax] [--dev] [--gpu] [--no-build] [-p <suffix>]
      Build and install plugins. Default: all formats, all plugins.
      --clap       CLAP only (no sudo)
      --vst3       VST3 only
      --vst2       VST2 only (no sudo)
      --au2        AU v2 only (.component)
      --au3        AU v3 only (.appex, requires Xcode)
      --aax        AAX only (requires pre-built template)
      --dev        Build hot-reload shells (use with cargo watch for iteration)
      --gpu        Enable GPU rendering (wgpu/Metal backend)
      --no-build   Skip build, install existing artifacts
      -p <suffix>  Install only the plugin with this suffix (e.g. -p gain)

  test
      Run all plugin tests (render, state, params, metadata).

  status
      Show installed plugins and AU registration state.

  clean
      Clear all AU/DAW caches and restart audio daemons.

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
Copy truce.toml.example to get started."
    );
}

fn cmd_new(args: &[String]) -> Res {
    let mut name: Option<String> = None;
    let mut hot = false;

    for arg in args {
        match arg.as_str() {
            "--hot" => hot = true,
            s if !s.starts_with('-') && name.is_none() => name = Some(s.to_string()),
            other => return Err(format!("unknown argument: {other}").into()),
        }
    }

    let name = name.ok_or("usage: cargo xtask new <name> [--hot]")?;
    let root = project_root();

    if hot {
        scaffold_hot_plugin(&root, &name)
    } else {
        scaffold_static_plugin(&root, &name)
    }
}

fn scaffold_static_plugin(root: &Path, name: &str) -> Res {
    let dir = root.join("examples").join(name);
    if dir.exists() {
        return Err(format!("examples/{name} already exists").into());
    }

    let crate_name = format!("truce-example-{name}");
    let struct_name = to_pascal_case(name);
    let _au_sub = to_fourcc(name);

    fs::create_dir_all(dir.join("src"))?;

    // Cargo.toml
    fs::write(dir.join("Cargo.toml"), format!(r#"[package]
name = "{crate_name}"
version.workspace = true
edition.workspace = true
license.workspace = true

[lib]
crate-type = ["cdylib", "rlib"]

[features]
default = ["clap", "vst3"]
clap = ["dep:truce-clap", "dep:clap-sys"]
vst3 = ["dep:truce-vst3"]
vst2 = ["dep:truce-vst2"]
au = ["dep:truce-au"]
dev = ["truce/dev"]

[dependencies]
truce = {{ workspace = true }}
truce-core = {{ workspace = true }}
truce-params = {{ workspace = true }}
truce-params-derive = {{ workspace = true }}
truce-loader = {{ workspace = true }}
truce-gui = {{ workspace = true }}
truce-clap = {{ workspace = true, optional = true }}
truce-vst3 = {{ workspace = true, optional = true }}
truce-vst2 = {{ workspace = true, optional = true }}
truce-au = {{ workspace = true, optional = true }}
clap-sys = {{ version = "0.5", optional = true }}

[dev-dependencies]
truce-test = {{ workspace = true }}

[build-dependencies]
truce-build = {{ workspace = true }}
"#))?;

    // build.rs
    fs::write(dir.join("build.rs"), "fn main() { truce_build::emit_plugin_env(); }\n")?;

    // src/lib.rs
    fs::write(dir.join("src/lib.rs"), format!(r#"use truce::prelude::*;

#[derive(Params)]
pub struct {struct_name}Params {{
    #[param(id = 0, name = "Gain", range = "linear(-60, 6)", unit = "dB", smooth = "exp(5)")]
    pub gain: FloatParam,
}}

pub struct {struct_name} {{
    params: {struct_name}Params,
}}

impl PluginLogic for {struct_name} {{
    fn new() -> Self {{
        Self {{ params: {struct_name}Params::new() }}
    }}

    fn params_mut(&mut self) -> Option<&mut dyn Params> {{
        Some(&mut self.params)
    }}

    fn reset(&mut self, sample_rate: f64, _max_block_size: usize) {{
        self.params.set_sample_rate(sample_rate);
    }}

    fn process(&mut self, buffer: &mut AudioBuffer, _events: &EventList, _context: &mut ProcessContext) -> ProcessStatus {{
        let gain = db_to_linear(self.params.gain.smoothed_next() as f64) as f32;
        for ch in 0..buffer.channels() {{
            let (inp, out) = buffer.io(ch);
            for i in 0..buffer.num_samples() {{
                out[i] = inp[i] * gain;
            }}
        }}
        ProcessStatus::Normal
    }}

    fn layout(&self) -> truce_gui::layout::GridLayout {{
        use truce_gui::layout::{{GridLayout, GridWidget}};
        GridLayout::build("{struct_name}", "V0.1", 2, 80.0, vec![
            GridWidget::knob(0, "Gain"),
        ], vec![])
    }}
}}

truce::plugin! {{
    logic: {struct_name},
    params: {struct_name}Params,
}}
"#))?;

    eprintln!("Created examples/{name}/");
    eprintln!();
    eprintln!("Next steps:");
    eprintln!("  1. Add \"{crate_name}\" to [workspace.members] in Cargo.toml");
    eprintln!("  2. Add a [[plugin]] entry to truce.toml with suffix = \"{name}\"");
    eprintln!("  3. cargo build -p {crate_name}");
    Ok(())
}

fn scaffold_hot_plugin(root: &Path, name: &str) -> Res {
    let dir = root.join("examples").join(name);
    if dir.exists() {
        return Err(format!("examples/{name} already exists").into());
    }

    let struct_name = to_pascal_case(name);
    let logic_crate = format!("{name}-logic");
    let shell_crate = format!("{name}-shell");
    let logic_lib = logic_crate.replace('-', "_");

    fs::create_dir_all(dir.join("logic/src"))?;
    fs::create_dir_all(dir.join("shell/src"))?;

    // --- Logic crate ---

    fs::write(dir.join("logic/Cargo.toml"), format!(r#"[package]
name = "{logic_crate}"
version.workspace = true
edition.workspace = true
license.workspace = true

[lib]
crate-type = ["cdylib", "rlib"]

[dependencies]
truce = {{ workspace = true }}
truce-loader = {{ workspace = true }}
"#))?;

    fs::write(dir.join("logic/src/lib.rs"), format!(r#"use truce::prelude::*;

pub struct {struct_name} {{
    gain: f64,
}}

impl PluginLogic for {struct_name} {{
    fn new() -> Self {{
        Self {{ gain: 1.0 }}
    }}

    fn reset(&mut self, _sample_rate: f64, _max_block_size: usize) {{
        self.gain = 1.0;
    }}

    fn process(&mut self, buffer: &mut AudioBuffer, _events: &EventList, context: &mut ProcessContext) -> ProcessStatus {{
        let target = db_to_linear(context.param(0));
        for i in 0..buffer.num_samples() {{
            self.gain += 0.001 * (target - self.gain);
            let g = self.gain as f32;
            for ch in 0..buffer.channels() {{
                let (inp, out) = buffer.io(ch);
                out[i] = inp[i] * g;
            }}
        }}
        ProcessStatus::Normal
    }}

    fn layout(&self) -> truce_gui::layout::GridLayout {{
        use truce_gui::layout::{{GridLayout, GridWidget}};
        GridLayout::build("{struct_name}", "V0.1", 2, 80.0, vec![
            GridWidget::knob(0, "Gain"),
        ], vec![])
    }}
}}

truce::export_plugin!({struct_name});
"#))?;

    // --- Shell crate ---

    fs::write(dir.join("shell/Cargo.toml"), format!(r#"[package]
name = "{shell_crate}"
version.workspace = true
edition.workspace = true
license.workspace = true

[lib]
crate-type = ["cdylib", "rlib"]

[features]
default = ["clap", "hot-reload"]
clap = ["dep:truce-clap", "dep:clap-sys"]
vst3 = ["dep:truce-vst3"]
vst2 = ["dep:truce-vst2"]
au = ["dep:truce-au"]
hot-reload = []
static-logic = ["dep:{logic_crate}"]

[dependencies]
truce = {{ workspace = true }}
truce-core = {{ workspace = true }}
truce-params = {{ workspace = true }}
truce-params-derive = {{ workspace = true }}
truce-loader = {{ workspace = true, features = ["shell"] }}
truce-gui = {{ workspace = true }}
{logic_crate} = {{ path = "../logic", optional = true }}
truce-clap = {{ workspace = true, optional = true }}
truce-vst3 = {{ workspace = true, optional = true }}
truce-vst2 = {{ workspace = true, optional = true }}
truce-au = {{ workspace = true, optional = true }}
clap-sys = {{ version = "0.5", optional = true }}

[build-dependencies]
truce-build = {{ workspace = true }}
"#))?;

    fs::write(dir.join("shell/build.rs"), "fn main() { truce_build::emit_plugin_env(); }\n")?;

    fs::write(dir.join("shell/src/lib.rs"), format!(r#"use truce::prelude::*;

#[derive(Params)]
pub struct {struct_name}Params {{
    #[param(id = 0, name = "Gain", range = "linear(-60, 6)", unit = "dB", smooth = "exp(5)")]
    pub gain: FloatParam,
}}

#[cfg(feature = "hot-reload")]
truce_loader::export_hot! {{
    params: {struct_name}Params,
    info: plugin_info!(),
    bus_layouts: [BusLayout::stereo()],
    logic_dylib: "{logic_lib}",
    editor: {{ |builtin| -> Box<dyn truce_core::editor::Editor> {{ Box::new(builtin) }} }},
}}

#[cfg(feature = "static-logic")]
truce_loader::export_static! {{
    params: {struct_name}Params,
    info: plugin_info!(),
    bus_layouts: [BusLayout::stereo()],
    logic: {logic_lib}::{struct_name},
    editor: {{ |builtin| -> Box<dyn truce_core::editor::Editor> {{ Box::new(builtin) }} }},
}}

#[cfg(feature = "clap")]
truce_clap::export_clap!(__HotShellWrapper);
#[cfg(feature = "vst3")]
truce_vst3::export_vst3!(__HotShellWrapper);
#[cfg(feature = "vst2")]
truce_vst2::export_vst2!(__HotShellWrapper);
#[cfg(feature = "au")]
truce_au::export_au!(__HotShellWrapper);
"#))?;

    eprintln!("Created examples/{name}/shell/ and examples/{name}/logic/");
    eprintln!();
    eprintln!("Next steps:");
    eprintln!("  1. Add \"{logic_crate}\" and \"{shell_crate}\" to [workspace.members] in Cargo.toml");
    eprintln!("  2. Add a [[plugin]] entry to truce.toml with suffix = \"{name}/shell\"");
    eprintln!("  3. cargo build -p {logic_crate}             # build the logic dylib");
    eprintln!("  4. cargo xtask install --clap -p {name}/shell  # install the shell once");
    eprintln!("  5. cargo watch -x \"build -p {logic_crate}\"   # iterate with hot-reload");
    Ok(())
}

fn to_pascal_case(s: &str) -> String {
    s.split('-')
        .map(|part| {
            let mut chars = part.chars();
            match chars.next() {
                Some(c) => c.to_uppercase().to_string() + chars.as_str(),
                None => String::new(),
            }
        })
        .collect()
}

fn to_fourcc(s: &str) -> String {
    let pascal = to_pascal_case(s);
    let bytes: Vec<char> = pascal.chars().take(4).collect();
    if bytes.len() >= 4 {
        bytes.into_iter().collect()
    } else {
        format!("{:X<4}", pascal.chars().take(4).collect::<String>())
    }
}

// ---------------------------------------------------------------------------
// Project configuration (read from truce.toml)
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct Config {
    #[serde(default)]
    macos: MacosConfig,
    vendor: VendorConfig,
    plugin: Vec<PluginDef>,
}

#[derive(Deserialize, Default)]
struct MacosConfig {
    #[serde(default = "default_signing_identity")]
    signing_identity: String,
    #[serde(default = "default_deployment_target")]
    deployment_target: String,
    /// Path to the AAX SDK root directory. Falls back to the AAX_SDK_PATH env var.
    aax_sdk_path: Option<String>,
}

fn default_signing_identity() -> String {
    "-".to_string()
}
fn default_deployment_target() -> String {
    "11.0".to_string()
}

#[derive(Deserialize)]
struct VendorConfig {
    name: String,
    id: String,
    au_manufacturer: String,
}

#[derive(Deserialize)]
struct PluginDef {
    name: String,
    suffix: String,
    #[serde(rename = "crate")]
    crate_name: String,
    au_type: String,
    au_subtype: String,
    #[serde(default)]
    au3_subtype: Option<String>,
    #[serde(default = "default_au_tag")]
    au_tag: String,
}

impl PluginDef {
    fn au3_sub(&self) -> &str {
        self.au3_subtype.as_deref().unwrap_or(&self.au_subtype)
    }
    fn fw_name(&self) -> String {
        let cap = format!("{}{}", self.suffix[..1].to_uppercase(), &self.suffix[1..]);
        format!("Truce{}AU", cap)
    }
    /// Dylib filename stem derived from the crate name (hyphens → underscores).
    fn dylib_stem(&self) -> String {
        self.crate_name.replace('-', "_")
    }
}

fn default_au_tag() -> String {
    "Effects".to_string()
}

/// Resolve the AAX SDK path: truce.toml `[macos].aax_sdk_path` → `AAX_SDK_PATH` env var → None.
fn resolve_aax_sdk_path(config: &Config) -> Option<PathBuf> {
    if let Some(ref p) = config.macos.aax_sdk_path {
        let path = PathBuf::from(p);
        if path.exists() {
            return Some(path);
        }
        eprintln!("warning: [macos].aax_sdk_path = {p:?} in truce.toml but directory not found");
    }
    if let Ok(p) = std::env::var("AAX_SDK_PATH") {
        let path = PathBuf::from(&p);
        if path.exists() {
            return Some(path);
        }
        eprintln!("warning: AAX_SDK_PATH={p} but directory not found");
    }
    None
}

fn load_config() -> std::result::Result<Config, BoxErr> {
    let root = project_root();
    let path = root.join("truce.toml");
    if !path.exists() {
        return Err(format!(
            "truce.toml not found at {}. Copy truce.toml.example to get started.",
            path.display()
        )
        .into());
    }
    let content = fs::read_to_string(&path)?;
    let config: Config = toml::from_str(&content)?;
    if config.plugin.is_empty() {
        return Err("No [[plugin]] entries in truce.toml".into());
    }
    Ok(config)
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

type Res = std::result::Result<(), Box<dyn std::error::Error>>;
type BoxErr = Box<dyn std::error::Error>;

fn project_root() -> PathBuf {
    // Walk up from the current directory looking for truce.toml.
    // This works from both `cargo xtask` (workspace) and `cargo truce`
    // (globally installed binary run from any project directory).
    let cwd = env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let mut dir = cwd.as_path();
    loop {
        if dir.join("truce.toml").exists() {
            return dir.to_path_buf();
        }
        match dir.parent() {
            Some(parent) => dir = parent,
            None => break,
        }
    }
    // Fallback: CARGO_MANIFEST_DIR (works inside `cargo xtask`)
    if let Ok(manifest) = env::var("CARGO_MANIFEST_DIR") {
        let p = Path::new(&manifest).parent().unwrap().to_path_buf();
        if p.join("truce.toml").exists() {
            return p;
        }
    }
    cwd
}

fn run_sudo(cmd: &str, args: &[&str]) -> Res {
    let status = Command::new("sudo").arg(cmd).args(args).status()?;
    if !status.success() {
        return Err(format!("sudo {cmd} failed with {status}").into());
    }
    Ok(())
}

fn run_quiet(cmd: &str, args: &[&str]) -> std::result::Result<String, BoxErr> {
    let output = Command::new(cmd).args(args).output()?;
    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

fn cargo_build(env_vars: &[(&str, &str)], extra_args: &[&str], deployment_target: &str) -> Res {
    let mut cmd = Command::new("cargo");
    cmd.arg("build").arg("--release");
    cmd.env("MACOSX_DEPLOYMENT_TARGET", deployment_target);
    for (k, v) in env_vars {
        cmd.env(k, v);
    }
    for arg in extra_args {
        cmd.arg(arg);
    }
    let status = cmd.status()?;
    if !status.success() {
        return Err("cargo build failed".into());
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// install
// ---------------------------------------------------------------------------

fn cmd_install(args: &[String]) -> Res {
    let config = load_config()?;

    let mut clap = false;
    let mut vst3 = false;
    let mut vst2 = false;
    let mut au2 = false;
    let mut au3 = false;
    let mut aax = false;
    let mut no_build = false;
    let mut dev_mode = false;
    let mut gpu_mode = false;
    let mut plugin_filter: Option<String> = None;

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--clap" => clap = true,
            "--vst3" => vst3 = true,
            "--vst2" => vst2 = true,
            "--au2" => au2 = true,
            "--au3" => au3 = true,
            "--aax" => aax = true,
            "--no-build" => no_build = true,
            "--dev" => dev_mode = true,
            "--gpu" => gpu_mode = true,
            "-p" => {
                i += 1;
                if i >= args.len() {
                    return Err("-p requires a plugin suffix (e.g. -p gain)".into());
                }
                plugin_filter = Some(args[i].clone());
            }
            other => return Err(format!("Unknown flag: {other}").into()),
        }
        i += 1;
    }

    if !clap && !vst3 && !vst2 && !au2 && !au3 && !aax {
        clap = true;
        vst3 = true;
        vst2 = true;
        au2 = true;
        au3 = true;
        aax = true;
    }

    // Filter plugins if -p specified
    let plugins: Vec<&PluginDef> = if let Some(ref filter) = plugin_filter {
        let matched: Vec<_> = config
            .plugin
            .iter()
            .filter(|p| p.suffix == *filter)
            .collect();
        if matched.is_empty() {
            return Err(format!(
                "No plugin with suffix '{filter}'. Available: {}",
                config
                    .plugin
                    .iter()
                    .map(|p| p.suffix.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            )
            .into());
        }
        matched
    } else {
        config.plugin.iter().collect()
    };

    let root = project_root();
    let dt = &config.macos.deployment_target;

    // Compute extra features string
    let mut extra_features = Vec::new();
    if dev_mode { extra_features.push("dev"); }
    if gpu_mode { extra_features.push("gpu"); }
    let features_str = extra_features.join(",");

    // --- Build ---
    if !no_build {
        if clap || vst3 {
            if !extra_features.is_empty() {
                let label = extra_features.join(" + ");
                eprintln!("Building CLAP + VST3 ({label})...");
                let mut args: Vec<&str> = Vec::new();
                for p in &plugins {
                    args.push("-p");
                    args.push(&p.crate_name);
                }
                args.extend_from_slice(&["--features", &features_str]);
                cargo_build(&[], &args, dt)?;
            } else {
                eprintln!("Building CLAP + VST3...");
                cargo_build(&[], &[], dt)?;
            }
            for p in &plugins {
                let src = root.join(format!(
                    "target/release/lib{}.dylib",
                    p.dylib_stem()
                ));
                let dst = root.join(format!(
                    "target/release/lib{}_plugin.dylib",
                    p.dylib_stem()
                ));
                if src.exists() {
                    fs::copy(&src, &dst)?;
                }
            }
        }

        if vst2 {
            eprintln!("Building VST2...");
            let mut args: Vec<&str> = Vec::new();
            for p in &plugins {
                args.push("-p");
                args.push(&p.crate_name);
            }
            args.extend_from_slice(&["--no-default-features", "--features", "vst2"]);
            cargo_build(&[], &args, dt)?;
            for p in &plugins {
                let src = root.join(format!(
                    "target/release/lib{}.dylib",
                    p.dylib_stem()
                ));
                let dst = root.join(format!(
                    "target/release/lib{}_vst2.dylib",
                    p.dylib_stem()
                ));
                fs::copy(&src, &dst)?;
            }
        }

        if au2 {
            eprintln!("Building AU v2...");
            for p in &plugins {
                cargo_build(
                    &[("TRUCE_AU_VERSION", "2"), ("TRUCE_AU_PLUGIN_ID", &p.suffix)],
                    &[
                        "-p",
                        &p.crate_name,
                        "--no-default-features",
                        "--features",
                        "au",
                    ],
                    dt,
                )?;
                let src = root.join(format!(
                    "target/release/lib{}.dylib",
                    p.dylib_stem()
                ));
                let dst = root.join(format!(
                    "target/release/lib{}_au.dylib",
                    p.dylib_stem()
                ));
                fs::copy(&src, &dst)?;
            }
        }

        if aax {
            eprintln!("Building AAX...");
            let mut args: Vec<&str> = Vec::new();
            for p in &plugins {
                args.push("-p");
                args.push(&p.crate_name);
            }
            args.extend_from_slice(&["--no-default-features", "--features", "aax"]);
            cargo_build(&[], &args, dt)?;
            for p in &plugins {
                let src = root.join(format!(
                    "target/release/lib{}.dylib",
                    p.dylib_stem()
                ));
                let dst = root.join(format!(
                    "target/release/lib{}_aax.dylib",
                    p.dylib_stem()
                ));
                fs::copy(&src, &dst)?;
            }
        }

        if clap || vst3 {
            for p in &plugins {
                let saved = root.join(format!(
                    "target/release/lib{}_plugin.dylib",
                    p.dylib_stem()
                ));
                let dst = root.join(format!(
                    "target/release/lib{}.dylib",
                    p.dylib_stem()
                ));
                if saved.exists() {
                    fs::copy(&saved, &dst)?;
                }
            }
        }

        // In dev mode, also build the debug dylibs (the logic that
        // the hot-reload shells watch and load).
        if dev_mode {
            eprintln!("Building debug dylibs (logic for hot-reload)...");
            let mut cmd = Command::new("cargo");
            cmd.arg("build").arg("--workspace");
            cmd.env("MACOSX_DEPLOYMENT_TARGET", dt);
            let status = cmd.status()?;
            if !status.success() {
                return Err("debug workspace build failed".into());
            }
        }
    }

    // --- Install ---
    for p in &plugins {
        if clap {
            install_clap(&root, p)?;
        }
        if vst3 {
            install_vst3(&root, p, &config)?;
        }
        if vst2 {
            install_vst2(&root, p)?;
        }
        if au2 {
            install_au(&root, p, &config)?;
        }
        if aax {
            install_aax(&root, p, &config)?;
        }
    }

    if au3 {
        install_all_au_v3_filtered(&root, &config, &plugins, no_build)?;
    }

    if au2 {
        let cache = dirs::home_dir()
            .unwrap()
            .join("Library/Caches/AudioUnitCache");
        let _ = fs::remove_dir_all(&cache);
        eprintln!("Cleared AU cache.");
    }

    eprintln!("\nDone. Restart your DAW to rescan.");
    Ok(())
}

fn install_clap(root: &Path, p: &PluginDef) -> Res {
    let dylib = root.join(format!(
        "target/release/lib{}.dylib",
        p.dylib_stem()
    ));
    if !dylib.exists() {
        return Err(format!("Missing: {}", dylib.display()).into());
    }
    let clap_dir = dirs::home_dir()
        .unwrap()
        .join("Library/Audio/Plug-Ins/CLAP");
    fs::create_dir_all(&clap_dir)?;
    let dst = clap_dir.join(format!("{}.clap", p.name));
    fs::copy(&dylib, &dst)?;
    eprintln!("CLAP: {}", dst.display());
    Ok(())
}

fn install_vst3(root: &Path, p: &PluginDef, config: &Config) -> Res {
    let dylib = root.join(format!(
        "target/release/lib{}.dylib",
        p.dylib_stem()
    ));
    if !dylib.exists() {
        return Err(format!("Missing: {}", dylib.display()).into());
    }
    let vst3_bundle = format!("/Library/Audio/Plug-Ins/VST3/{}.vst3", p.name);
    let contents = format!("{vst3_bundle}/Contents");

    run_sudo("mkdir", &["-p", &format!("{contents}/MacOS")])?;
    run_sudo(
        "cp",
        &[
            dylib.to_str().unwrap(),
            &format!("{contents}/MacOS/{}", p.name),
        ],
    )?;

    let plist = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleExecutable</key>
    <string>{name}</string>
    <key>CFBundleIdentifier</key>
    <string>{vendor_id}.{suffix}</string>
    <key>CFBundleName</key>
    <string>{name}</string>
    <key>CFBundlePackageType</key>
    <string>BNDL</string>
    <key>CFBundleVersion</key>
    <string>1</string>
</dict>
</plist>"#,
        name = p.name,
        suffix = p.suffix,
        vendor_id = config.vendor.id,
    );
    let plist_tmp = format!("/tmp/truce_{}_vst3.plist", p.suffix);
    fs::write(&plist_tmp, &plist)?;
    run_sudo("cp", &[&plist_tmp, &format!("{contents}/Info.plist")])?;
    run_sudo(
        "codesign",
        &["--force", "--deep", "--sign", "-", &vst3_bundle],
    )?;
    eprintln!("VST3: {vst3_bundle}");
    Ok(())
}

fn install_vst2(root: &Path, p: &PluginDef) -> Res {
    let dylib = root.join(format!(
        "target/release/lib{}_vst2.dylib",
        p.dylib_stem()
    ));
    if !dylib.exists() {
        return Err(format!("Missing: {}", dylib.display()).into());
    }
    let vst_dir = dirs::home_dir().unwrap().join("Library/Audio/Plug-Ins/VST");
    let bundle = vst_dir.join(format!("{}.vst", p.name));

    let _ = fs::remove_dir_all(&bundle);
    let macos_dir = bundle.join("Contents/MacOS");
    fs::create_dir_all(&macos_dir)?;
    fs::copy(&dylib, macos_dir.join(&p.name))?;

    let plist = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleExecutable</key>
    <string>{name}</string>
    <key>CFBundleIdentifier</key>
    <string>com.truce.{suffix}.vst2</string>
    <key>CFBundleName</key>
    <string>{name}</string>
    <key>CFBundlePackageType</key>
    <string>BNDL</string>
    <key>CFBundleVersion</key>
    <string>1</string>
</dict>
</plist>"#,
        name = p.name,
        suffix = p.suffix,
    );
    fs::write(bundle.join("Contents/Info.plist"), &plist)?;
    fs::write(bundle.join("Contents/PkgInfo"), "BNDL????")?;

    // Ad-hoc sign the bundle (required by some hosts like Ableton)
    let _ = Command::new("codesign")
        .args(["--force", "--deep", "--sign", "-"])
        .arg(&bundle)
        .status();

    eprintln!("VST2: {}", bundle.display());
    Ok(())
}

fn install_au(root: &Path, p: &PluginDef, config: &Config) -> Res {
    let dylib = root.join(format!(
        "target/release/lib{}_au.dylib",
        p.dylib_stem()
    ));
    if !dylib.exists() {
        return Err(format!("Missing: {}", dylib.display()).into());
    }
    let bundle = format!("/Library/Audio/Plug-Ins/Components/{}.component", p.name);
    let contents = format!("{bundle}/Contents");

    let _ = run_sudo("rm", &["-rf", &bundle]);
    run_sudo("mkdir", &["-p", &format!("{contents}/MacOS")])?;
    run_sudo(
        "cp",
        &[
            dylib.to_str().unwrap(),
            &format!("{contents}/MacOS/{}", p.name),
        ],
    )?;

    let plist = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleExecutable</key>
    <string>{name}</string>
    <key>CFBundleIdentifier</key>
    <string>{vendor_id}.{suffix}.component</string>
    <key>CFBundleName</key>
    <string>{name}</string>
    <key>CFBundlePackageType</key>
    <string>BNDL</string>
    <key>CFBundleVersion</key>
    <string>1</string>
    <key>AudioComponents</key>
    <array>
        <dict>
            <key>type</key>
            <string>{au_type}</string>
            <key>subtype</key>
            <string>{au_subtype}</string>
            <key>manufacturer</key>
            <string>{au_mfr}</string>
            <key>name</key>
            <string>{vendor}: {name}</string>
            <key>description</key>
            <string>{name}</string>
            <key>version</key>
            <integer>65536</integer>
            <key>factoryFunction</key>
            <string>TruceAUFactory</string>
            <key>sandboxSafe</key>
            <true/>
            <key>tags</key>
            <array>
                <string>{au_tag}</string>
            </array>
        </dict>
    </array>
</dict>
</plist>"#,
        name = p.name,
        suffix = p.suffix,
        vendor_id = config.vendor.id,
        vendor = config.vendor.name,
        au_type = p.au_type,
        au_subtype = p.au_subtype,
        au_mfr = config.vendor.au_manufacturer,
        au_tag = p.au_tag,
    );
    let plist_tmp = format!("/tmp/truce_{}_au.plist", p.suffix);
    fs::write(&plist_tmp, &plist)?;
    run_sudo("cp", &[&plist_tmp, &format!("{contents}/Info.plist")])?;
    run_sudo(
        "codesign",
        &[
            "--force",
            "--deep",
            "--sign",
            &config.macos.signing_identity,
            &bundle,
        ],
    )?;
    eprintln!("AU:   {bundle}");
    Ok(())
}

// ---------------------------------------------------------------------------
// AAX install
// ---------------------------------------------------------------------------

fn build_aax_template(_root: &Path, sdk_path: &Path) -> Res {
    // Write embedded template files to a temp directory
    let template_dir = PathBuf::from("/tmp/truce_aax_template");
    let src_dir = template_dir.join("src");
    let _ = fs::remove_dir_all(&template_dir);
    fs::create_dir_all(&src_dir)?;

    fs::write(template_dir.join("CMakeLists.txt"), templates::aax::CMAKE_LISTS)?;
    fs::write(src_dir.join("TruceAAX_Bridge.cpp"), templates::aax::BRIDGE_CPP)?;
    fs::write(src_dir.join("TruceAAX_Bridge.h"), templates::aax::BRIDGE_H)?;
    fs::write(src_dir.join("TruceAAX_Describe.cpp"), templates::aax::DESCRIBE_CPP)?;
    fs::write(src_dir.join("TruceAAX_GUI.cpp"), templates::aax::GUI_CPP)?;
    fs::write(src_dir.join("TruceAAX_GUI.h"), templates::aax::GUI_H)?;
    fs::write(src_dir.join("TruceAAX_Parameters.cpp"), templates::aax::PARAMETERS_CPP)?;
    fs::write(src_dir.join("TruceAAX_Parameters.h"), templates::aax::PARAMETERS_H)?;
    fs::write(src_dir.join("Info.plist.in"), templates::aax::INFO_PLIST_IN)?;
    fs::write(src_dir.join("truce_aax_bridge.h"), templates::aax::BRIDGE_HEADER)?;

    let build_dir = template_dir.join("build");
    let status = Command::new("cmake")
        .arg("-B")
        .arg(&build_dir)
        .arg(format!("-DAAX_SDK_PATH={}", sdk_path.display()))
        .current_dir(&template_dir)
        .status()?;
    if !status.success() {
        return Err("cmake configure failed for AAX template".into());
    }
    let status = Command::new("cmake")
        .arg("--build")
        .arg(&build_dir)
        .status()?;
    if !status.success() {
        return Err("cmake build failed for AAX template".into());
    }
    Ok(())
}

fn install_aax(root: &Path, p: &PluginDef, config: &Config) -> Res {
    let template = PathBuf::from(
        "/tmp/truce_aax_template/build/TruceAAXTemplate.aaxplugin/Contents/MacOS/TruceAAXTemplate",
    );
    if !template.exists() {
        if let Some(sdk_path) = resolve_aax_sdk_path(config) {
            eprintln!("AAX: building template with SDK at {}", sdk_path.display());
            build_aax_template(root, &sdk_path)?;
        } else {
            eprintln!(
                "AAX: template not built, skipping.\n  \
                 Set [macos].aax_sdk_path in truce.toml or AAX_SDK_PATH env var."
            );
            return Ok(());
        }
    }
    if !template.exists() {
        return Err("AAX template build succeeded but binary not found".into());
    }

    let dylib = root.join(format!(
        "target/release/lib{}_aax.dylib",
        p.dylib_stem()
    ));
    if !dylib.exists() {
        eprintln!("AAX: {} not found, skipping {}", dylib.display(), p.name);
        return Ok(());
    }

    let aax_dir = "/Library/Application Support/Avid/Audio/Plug-Ins";
    let bundle = format!("{aax_dir}/{}.aaxplugin", p.name);
    let contents = format!("{bundle}/Contents");

    run_sudo("rm", &["-rf", &bundle])?;
    run_sudo("mkdir", &["-p", &format!("{contents}/MacOS")])?;
    run_sudo("mkdir", &["-p", &format!("{contents}/Resources")])?;

    // Copy template binary
    run_sudo(
        "cp",
        &[
            template.to_str().unwrap(),
            &format!("{contents}/MacOS/{}", p.name),
        ],
    )?;

    // Copy Rust cdylib
    run_sudo(
        "cp",
        &[
            dylib.to_str().unwrap(),
            &format!("{contents}/Resources/"),
        ],
    )?;

    // Info.plist
    let plist = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleExecutable</key>
    <string>{name}</string>
    <key>CFBundleIdentifier</key>
    <string>com.truce.{suffix}.aax</string>
    <key>CFBundleName</key>
    <string>{name}</string>
    <key>CFBundlePackageType</key>
    <string>TDMw</string>
    <key>CFBundleVersion</key>
    <string>1</string>
</dict>
</plist>"#,
        name = p.name,
        suffix = p.suffix,
    );
    let plist_tmp = format!("/tmp/truce_{}_aax.plist", p.suffix);
    fs::write(&plist_tmp, &plist)?;
    run_sudo("cp", &[&plist_tmp, &format!("{contents}/Info.plist")])?;

    // Ad-hoc sign (sufficient for Pro Tools Developer)
    run_sudo(
        "codesign",
        &["--force", "--deep", "--sign", "-", &bundle],
    )?;

    eprintln!("AAX:  {bundle}");
    Ok(())
}

// ---------------------------------------------------------------------------
// AU v3 appex install (Swift + xcodebuild)
// ---------------------------------------------------------------------------

fn install_all_au_v3_filtered(
    root: &Path,
    config: &Config,
    plugins: &[&PluginDef],
    no_build: bool,
) -> Res {
    let sign_id = &config.macos.signing_identity;
    let team_id = extract_team_id(sign_id);
    let dt = &config.macos.deployment_target;
    // shim_dir used to point at root/crates/truce-au/shim, but now
    // au_shim_types.h is embedded and written to build_dir/AUExt.

    for p in plugins {
        let fw_name = p.fw_name();
        let au_v3_sub = p.au3_sub();
        let app_dir = format!("/Applications/{} v3.app", p.name);
        let appex_id = format!(
            "com.{}.{}.v3.ext",
            config.vendor.id.trim_start_matches("com."),
            p.suffix
        );
        let build_dir = PathBuf::from(format!("/tmp/truce_au_v3_build_{}", p.suffix));
        let fw_build = PathBuf::from(format!("/tmp/truce_au_v3_fw_{}", p.suffix));

        eprintln!("=== AU v3: {} ===", p.name);

        // Pre-clean
        let _ = Command::new("pluginkit")
            .args(["-e", "ignore", "-i", &appex_id])
            .output();
        let _ = run_sudo("rm", &["-rf", &app_dir]);

        if !no_build {
            // Step 1: Build Rust framework
            eprintln!("  Building Rust framework...");
            cargo_build(
                &[("TRUCE_AU_VERSION", "3"), ("TRUCE_AU_PLUGIN_ID", &p.suffix)],
                &[
                    "-p",
                    &p.crate_name,
                    "--no-default-features",
                    "--features",
                    "au",
                ],
                dt,
            )?;
            let src = root.join(format!(
                "target/release/lib{}.dylib",
                p.dylib_stem()
            ));
            let dst = root.join(format!(
                "target/release/lib{}_v3.dylib",
                p.dylib_stem()
            ));
            fs::copy(&src, &dst)?;

            // Step 2: Create .framework bundle
            let _ = fs::remove_dir_all(&fw_build);
            let fw_dir = fw_build.join(format!("{}.framework/Versions/A", fw_name));
            fs::create_dir_all(fw_dir.join("Resources"))?;
            fs::copy(&dst, fw_dir.join(&fw_name))?;

            let status = Command::new("install_name_tool")
                .args([
                    "-id",
                    &format!("@rpath/{}.framework/Versions/A/{}", fw_name, fw_name),
                ])
                .arg(fw_dir.join(&fw_name))
                .status()?;
            if !status.success() {
                return Err("install_name_tool failed".into());
            }

            let fw_root = fw_build.join(format!("{}.framework", fw_name));
            std::os::unix::fs::symlink("A", fw_root.join("Versions/Current"))?;
            std::os::unix::fs::symlink(
                format!("Versions/Current/{}", fw_name),
                fw_root.join(&fw_name),
            )?;
            std::os::unix::fs::symlink("Versions/Current/Resources", fw_root.join("Resources"))?;

            fs::write(
                fw_dir.join("Resources/Info.plist"),
                format!(
                    r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
<key>CFBundleExecutable</key><string>{fw}</string>
<key>CFBundleIdentifier</key><string>com.{vid}.{suf}.framework</string>
<key>CFBundlePackageType</key><string>FMWK</string>
<key>CFBundleVersion</key><string>1</string>
</dict></plist>"#,
                    fw = fw_name,
                    vid = config.vendor.id.trim_start_matches("com."),
                    suf = p.suffix,
                ),
            )?;

            let _ = Command::new("codesign")
                .args(["--force", "--sign", sign_id])
                .arg(&fw_root)
                .status();

            // Step 3: Prepare Xcode project from embedded templates
            let _ = fs::remove_dir_all(&build_dir);
            fs::create_dir_all(build_dir.join("AUExt"))?;
            fs::create_dir_all(build_dir.join("App"))?;
            fs::create_dir_all(build_dir.join("XcodeAUv3.xcodeproj"))?;

            fs::write(build_dir.join("AUExt/AudioUnitFactory.swift"), templates::au3::SWIFT_SOURCE)?;
            fs::write(build_dir.join("AUExt/BridgingHeader.h"), templates::au3::BRIDGING_HEADER)?;
            fs::write(build_dir.join("AUExt/au_shim_types.h"), templates::au3::SHIM_TYPES_H)?;
            fs::write(build_dir.join("AUExt/AUExt.entitlements"), templates::au3::APPEX_ENTITLEMENTS)?;
            fs::write(build_dir.join("App/main.m"), templates::au3::APP_MAIN_M)?;
            fs::write(build_dir.join("App/App.entitlements"), templates::au3::APP_ENTITLEMENTS)?;

            // Patch AUExt/Info.plist with plugin-specific values
            let plist_path = build_dir.join("AUExt/Info.plist");
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap();
            let ver = format!("{}.{}", now.as_secs(), now.subsec_millis());

            let plist = templates::au3::APPEX_INFO_PLIST
                .replace("AUVER", &ver)
                .replace("AUTYPE", &p.au_type)
                .replace("AUSUB", au_v3_sub)
                .replace("AUMFR", &config.vendor.au_manufacturer)
                .replace(
                    "AUNAME",
                    &format!("{}: {} (v3)", config.vendor.name, p.name),
                )
                .replace("AUTAG", &p.au_tag);
            fs::write(&plist_path, plist)?;

            // Generate pbxproj (the template dir has an empty xcodeproj)
            let pbx_path = build_dir.join("XcodeAUv3.xcodeproj/project.pbxproj");
            fs::write(
                &pbx_path,
                generate_pbxproj(
                    &team_id,
                    &format!("{}.v3", p.suffix),
                    &format!("{}.v3.ext", p.suffix),
                    build_dir.join("AUExt").to_str().unwrap(),
                    fw_build.to_str().unwrap(),
                    &fw_name,
                ),
            )?;

            // Write App Info.plist from embedded template
            fs::write(build_dir.join("App/Info.plist"), templates::au3::APP_INFO_PLIST)?;

            // Step 4: xcodebuild
            eprintln!("  Building with xcodebuild...");
            let output = Command::new("xcodebuild")
                .current_dir(&build_dir)
                .args([
                    "-project",
                    "XcodeAUv3.xcodeproj",
                    "-target",
                    "TruceAUv3",
                    "-configuration",
                    "Release",
                    "ARCHS=arm64",
                ])
                .arg(format!("SYMROOT={}/build", build_dir.display()))
                .output()?;
            if !output.status.success() {
                let stderr = String::from_utf8_lossy(&output.stderr);
                let stdout = String::from_utf8_lossy(&output.stdout);
                // Find error lines
                for line in stdout.lines().chain(stderr.lines()) {
                    if line.contains("error:") || line.contains("BUILD FAILED") {
                        eprintln!("  {line}");
                    }
                }
                return Err(format!("xcodebuild failed for {}", p.name).into());
            }
        }

        // Step 5: Install
        let built_app = build_dir.join("build/Release/TruceAUv3.app");
        if !built_app.exists() {
            return Err(format!("Built app not found: {}", built_app.display()).into());
        }

        run_sudo("cp", &["-R", built_app.to_str().unwrap(), &app_dir])?;
        run_sudo("mkdir", &["-p", &format!("{app_dir}/Contents/Frameworks")])?;
        let fw_src = fw_build.join(format!("{fw_name}.framework"));
        run_sudo(
            "cp",
            &[
                "-R",
                fw_src.to_str().unwrap(),
                &format!("{app_dir}/Contents/Frameworks/{fw_name}.framework"),
            ],
        )?;

        // Step 6: Sign inside-out
        run_sudo(
            "codesign",
            &[
                "--force",
                "--sign",
                sign_id,
                &format!("{app_dir}/Contents/Frameworks/{fw_name}.framework"),
            ],
        )?;
        let entitlements_appex = build_dir.join("AUExt/AUExt.entitlements");
        let entitlements_app = build_dir.join("App/App.entitlements");
        run_sudo(
            "codesign",
            &[
                "--force",
                "--sign",
                sign_id,
                "--entitlements",
                entitlements_appex.to_str().unwrap(),
                "--generate-entitlement-der",
                &format!("{app_dir}/Contents/PlugIns/AUExt.appex"),
            ],
        )?;
        run_sudo(
            "codesign",
            &[
                "--force",
                "--sign",
                sign_id,
                "--entitlements",
                entitlements_app.to_str().unwrap(),
                "--generate-entitlement-der",
                &app_dir,
            ],
        )?;

        // Step 7: Cache bust + register
        let _ = Command::new("/System/Library/Frameworks/CoreServices.framework/Frameworks/LaunchServices.framework/Support/lsregister")
            .args(["-f", "-R", &app_dir]).output();
        let _ = run_sudo("killall", &["-9", "pkd"]);
        let _ = run_sudo("killall", &["-9", "AudioComponentRegistrar"]);
        let home = dirs::home_dir().unwrap();
        let _ = fs::remove_dir_all(home.join("Library/Caches/AudioUnitCache"));
        std::thread::sleep(std::time::Duration::from_secs(2));
        let _ = Command::new("pluginkit")
            .args(["-a", &format!("{app_dir}/Contents/PlugIns/AUExt.appex")])
            .output();

        eprintln!("  Installed: {app_dir}");
    }
    Ok(())
}

fn generate_pbxproj(
    team_id: &str,
    app_bundle_id: &str,
    appex_bundle_id: &str,
    shim_dir: &str,
    fw_search: &str,
    fw_name: &str,
) -> String {
    format!(
        r#"// !$*UTF8*$!
{{
	archiveVersion = 1;
	classes = {{}};
	objectVersion = 56;
	objects = {{
		AA000001 = {{isa = PBXGroup; children = (AA000010, AA000011, AA000012); name = App; sourceTree = "<group>";}};
		AA000010 = {{isa = PBXFileReference; path = "App/main.m"; sourceTree = SOURCE_ROOT;}};
		AA000011 = {{isa = PBXFileReference; path = "App/Info.plist"; sourceTree = SOURCE_ROOT;}};
		AA000012 = {{isa = PBXFileReference; path = "App/App.entitlements"; sourceTree = SOURCE_ROOT;}};
		AA000020 = {{isa = PBXBuildFile; fileRef = AA000010;}};
		BB000001 = {{isa = PBXGroup; children = (BB000010, BB000011, BB000012, BB000013); name = AUExt; sourceTree = "<group>";}};
		BB000010 = {{isa = PBXFileReference; path = "AUExt/AudioUnitFactory.swift"; sourceTree = SOURCE_ROOT;}};
		BB000011 = {{isa = PBXFileReference; path = "AUExt/Info.plist"; sourceTree = SOURCE_ROOT;}};
		BB000012 = {{isa = PBXFileReference; path = "AUExt/AUExt.entitlements"; sourceTree = SOURCE_ROOT;}};
		BB000013 = {{isa = PBXFileReference; path = "AUExt/BridgingHeader.h"; sourceTree = SOURCE_ROOT;}};
		BB000020 = {{isa = PBXBuildFile; fileRef = BB000010;}};
		CC000001 = {{isa = PBXFileReference; explicitFileType = wrapper.application; path = "TruceAUv3.app"; sourceTree = BUILT_PRODUCTS_DIR;}};
		CC000002 = {{isa = PBXFileReference; explicitFileType = "wrapper.app-extension"; path = "AUExt.appex"; sourceTree = BUILT_PRODUCTS_DIR;}};
		CC000003 = {{isa = PBXGroup; children = (CC000001, CC000002); name = Products; sourceTree = "<group>";}};
		DD000001 = {{isa = PBXBuildFile; fileRef = CC000002; settings = {{ATTRIBUTES = (RemoveHeadersOnCopy,);}}; }};
		DD000002 = {{isa = PBXCopyFilesBuildPhase; buildActionMask = 2147483647; dstPath = ""; dstSubfolderSpec = 13; files = (DD000001,); name = "Embed Extensions";}};
		EE000001 = {{isa = PBXBuildFile; fileRef = EE000010;}};
		EE000002 = {{isa = PBXBuildFile; fileRef = EE000011;}};
		EE000003 = {{isa = PBXBuildFile; fileRef = EE000012;}};
		EE000010 = {{isa = PBXFileReference; lastKnownFileType = wrapper.framework; name = AudioToolbox.framework; path = System/Library/Frameworks/AudioToolbox.framework; sourceTree = SDKROOT;}};
		EE000011 = {{isa = PBXFileReference; lastKnownFileType = wrapper.framework; name = CoreAudioKit.framework; path = System/Library/Frameworks/CoreAudioKit.framework; sourceTree = SDKROOT;}};
		EE000012 = {{isa = PBXFileReference; lastKnownFileType = wrapper.framework; name = AVFAudio.framework; path = System/Library/Frameworks/AVFAudio.framework; sourceTree = SDKROOT;}};
		EE000020 = {{isa = PBXFrameworksBuildPhase; files = (EE000001, EE000002, EE000003);}};
		FF000001 = {{isa = PBXSourcesBuildPhase; files = (AA000020,);}};
		FF000002 = {{isa = PBXSourcesBuildPhase; files = (BB000020,);}};
		GG000010 = {{isa = PBXFileReference; lastKnownFileType = wrapper.framework; name = Cocoa.framework; path = System/Library/Frameworks/Cocoa.framework; sourceTree = SDKROOT;}};
		GG000020 = {{isa = PBXBuildFile; fileRef = GG000010;}};
		FF000003 = {{isa = PBXFrameworksBuildPhase; files = (GG000020,);}};
		00000001 = {{isa = PBXGroup; children = (AA000001, BB000001, CC000003); sourceTree = "<group>";}};
		11000001 = {{
			isa = PBXNativeTarget;
			buildConfigurationList = 11000010;
			buildPhases = (FF000001, FF000003, DD000002);
			dependencies = (11000020,);
			name = TruceAUv3;
			productName = TruceAUv3;
			productReference = CC000001;
			productType = "com.apple.product-type.application";
		}};
		11000010 = {{isa = XCConfigurationList; buildConfigurations = (11000011,);}};
		11000011 = {{
			isa = XCBuildConfiguration;
			buildSettings = {{
				PRODUCT_BUNDLE_IDENTIFIER = "com.truce.{app_id}";
				PRODUCT_NAME = "$(TARGET_NAME)";
				INFOPLIST_FILE = "App/Info.plist";
				CODE_SIGN_ENTITLEMENTS = "App/App.entitlements";
				CODE_SIGN_STYLE = Manual;
				CODE_SIGN_IDENTITY = "Developer ID Application";
				DEVELOPMENT_TEAM = {team};
				SWIFT_VERSION = 5.0;
				MACOSX_DEPLOYMENT_TARGET = 13.0;
			}};
			name = Release;
		}};
		11000020 = {{isa = PBXTargetDependency; target = 22000001;}};
		22000001 = {{
			isa = PBXNativeTarget;
			buildConfigurationList = 22000010;
			buildPhases = (FF000002, EE000020);
			dependencies = ();
			name = AUExt;
			productName = AUExt;
			productReference = CC000002;
			productType = "com.apple.product-type.app-extension";
		}};
		22000010 = {{isa = XCConfigurationList; buildConfigurations = (22000011,);}};
		22000011 = {{
			isa = XCBuildConfiguration;
			buildSettings = {{
				PRODUCT_BUNDLE_IDENTIFIER = "com.truce.{appex_id}";
				PRODUCT_NAME = "$(TARGET_NAME)";
				INFOPLIST_FILE = "AUExt/Info.plist";
				CODE_SIGN_ENTITLEMENTS = "AUExt/AUExt.entitlements";
				CODE_SIGN_STYLE = Manual;
				CODE_SIGN_IDENTITY = "Developer ID Application";
				DEVELOPMENT_TEAM = {team};
				SWIFT_VERSION = 5.0;
				MACOSX_DEPLOYMENT_TARGET = 13.0;
				APPLICATION_EXTENSION_API_ONLY = YES;
				SWIFT_OBJC_BRIDGING_HEADER = "AUExt/BridgingHeader.h";
				HEADER_SEARCH_PATHS = "{shim}";
				FRAMEWORK_SEARCH_PATHS = "{fw_search}";
				LD_RUNPATH_SEARCH_PATHS = "@executable_path/../../../../Frameworks";
				OTHER_LDFLAGS = ("-framework", "{fw}");
			}};
			name = Release;
		}};
		99000001 = {{
			isa = PBXProject;
			buildConfigurationList = 99000010;
			mainGroup = 00000001;
			productRefGroup = CC000003;
			targets = (11000001, 22000001);
		}};
		99000010 = {{isa = XCConfigurationList; buildConfigurations = (99000011,);}};
		99000011 = {{
			isa = XCBuildConfiguration;
			buildSettings = {{
				SDKROOT = macosx;
				MACOSX_DEPLOYMENT_TARGET = 13.0;
				ARCHS = arm64;
			}};
			name = Release;
		}};
	}};
	rootObject = 99000001;
}}"#,
        team = team_id,
        app_id = app_bundle_id,
        appex_id = appex_bundle_id,
        shim = shim_dir,
        fw_search = fw_search,
        fw = fw_name,
    )
}

fn extract_team_id(sign_id: &str) -> String {
    // Extract team ID from signing identity like "Developer ID Application: Name (TEAMID)"
    if let Some(start) = sign_id.rfind('(') {
        if let Some(end) = sign_id.rfind(')') {
            return sign_id[start + 1..end].to_string();
        }
    }
    String::new()
}

#[allow(dead_code)]
fn copy_dir_recursive(src: &Path, dst: &Path) -> Res {
    fs::create_dir_all(dst)?;
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let src_path = entry.path();
        let dst_path = dst.join(entry.file_name());
        if src_path.is_dir() {
            copy_dir_recursive(&src_path, &dst_path)?;
        } else {
            fs::copy(&src_path, &dst_path)?;
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// status
// ---------------------------------------------------------------------------

fn cmd_test() -> Res {
    let config = load_config()?;

    eprintln!("Running plugin tests...\n");
    let mut all_passed = true;

    for p in &config.plugin {
        eprint!("  {} ... ", p.name);
        let status = Command::new("cargo")
            .args(["test", "-p", &p.crate_name, "--", "--quiet"])
            .output()?;
        let stderr = String::from_utf8_lossy(&status.stderr);
        if status.status.success() {
            // Count tests from stderr (cargo test output goes to stderr)
            let test_line = stderr.lines().find(|l| l.contains("test result"));
            if let Some(line) = test_line {
                eprintln!("{}", line.trim());
            } else {
                eprintln!("PASS");
            }
        } else {
            eprintln!("FAIL");
            eprint!("{}", String::from_utf8_lossy(&status.stderr));
            all_passed = false;
        }
    }

    // --- VST2 binary tests ---
    let root = project_root();
    let test_src = root.join("tests/test_vst2_binary.c");
    let test_bin = root.join("target/test_vst2");
    if test_src.exists() {
        eprintln!("Running VST2 binary tests...\n");
        let cc_status = Command::new("cc")
            .args(["-o", test_bin.to_str().unwrap(), test_src.to_str().unwrap()])
            .status()?;
        if cc_status.success() {
            // Build VST2 plugins
            for p in &config.plugin {
                eprint!("  VST2 {} ... ", p.name);
                let build = Command::new("cargo")
                    .args(["build", "--release", "-p", &p.crate_name,
                           "--no-default-features", "--features", "vst2"])
                    .env("MACOSX_DEPLOYMENT_TARGET", &config.macos.deployment_target)
                    .output()?;
                if !build.status.success() {
                    eprintln!("BUILD FAILED");
                    all_passed = false;
                    continue;
                }
                let dylib = root.join(format!("target/release/lib{}.dylib", p.dylib_stem()));
                let is_synth = p.au_type == "aumu";
                let mut cmd = Command::new(test_bin.to_str().unwrap());
                cmd.arg(dylib.to_str().unwrap());
                if is_synth { cmd.arg("--synth"); }
                let output = cmd.output()?;
                let stdout = String::from_utf8_lossy(&output.stdout);
                if output.status.success() {
                    if let Some(line) = stdout.lines().last() {
                        eprintln!("{}", line);
                    }
                } else {
                    eprintln!("FAIL");
                    eprint!("{}", stdout);
                    all_passed = false;
                }
            }
            eprintln!();
        }
    }

    if all_passed {
        eprintln!("All tests passed.");
        Ok(())
    } else {
        Err("Some tests failed".into())
    }
}

fn cmd_status() -> Res {
    let config = load_config()?;
    let vendor = &config.vendor.name;

    eprintln!("=== AU v2 Components ===");
    let comp_dir = Path::new("/Library/Audio/Plug-Ins/Components");
    if comp_dir.exists() {
        for entry in fs::read_dir(comp_dir)? {
            let name = entry?.file_name();
            let name = name.to_string_lossy();
            if name.contains(vendor) {
                eprintln!("  {name}");
            }
        }
    }

    eprintln!("\n=== CLAP ===");
    let clap_dir = dirs::home_dir()
        .unwrap()
        .join("Library/Audio/Plug-Ins/CLAP");
    if clap_dir.exists() {
        for entry in fs::read_dir(&clap_dir)? {
            let name = entry?.file_name();
            let name = name.to_string_lossy();
            if name.contains(vendor) {
                eprintln!("  {name}");
            }
        }
    }

    eprintln!("\n=== VST2 ===");
    let vst2_dir = dirs::home_dir().unwrap().join("Library/Audio/Plug-Ins/VST");
    if vst2_dir.exists() {
        for entry in fs::read_dir(&vst2_dir)? {
            let name = entry?.file_name();
            let name = name.to_string_lossy();
            if name.contains(vendor) {
                eprintln!("  {name}");
            }
        }
    }

    eprintln!("\n=== VST3 ===");
    let vst3_dir = Path::new("/Library/Audio/Plug-Ins/VST3");
    if vst3_dir.exists() {
        for entry in fs::read_dir(vst3_dir)? {
            let name = entry?.file_name();
            let name = name.to_string_lossy();
            if name.contains(vendor) {
                eprintln!("  {name}");
            }
        }
    }

    eprintln!("\n=== auval ===");
    if let Ok(output) = run_quiet("auval", &["-a"]) {
        let vendor_lower = vendor.to_lowercase();
        for line in output.lines() {
            if line.to_lowercase().contains(&vendor_lower) {
                eprintln!("  {line}");
            }
        }
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// clean
// ---------------------------------------------------------------------------

fn cmd_clean() -> Res {
    eprintln!("Clearing AU/DAW caches...");
    let home = dirs::home_dir().unwrap();

    // AU caches (system + sandboxed DAW containers)
    let cache_dirs = [
        home.join("Library/Caches/AudioUnitCache"),
        home.join("Library/Containers/com.apple.garageband10/Data/Library/Caches/AudioUnitCache"),
        home.join("Library/Containers/com.apple.logicpro10/Data/Library/Caches/AudioUnitCache"),
        home.join("Library/Caches/com.apple.logic10/AudioUnitCache"),
    ];
    for dir in &cache_dirs {
        if dir.exists() {
            let _ = fs::remove_dir_all(dir);
            eprintln!("  Removed: {}", dir.display());
        }
    }

    // Audio preferences
    let prefs = [
        home.join("Library/Preferences/com.apple.audio.InfoHelper.plist"),
        home.join("Library/Preferences/com.apple.audio.SandboxHelper.plist"),
    ];
    for pref in &prefs {
        if pref.exists() {
            let _ = fs::remove_file(pref);
        }
    }

    // Reaper AU cache
    let reaper_cache = home.join("Library/Application Support/REAPER/reaper-auplugins_arm64.ini");
    if reaper_cache.exists() {
        if let Ok(content) = fs::read_to_string(&reaper_cache) {
            if let Ok(config) = load_config() {
                let filtered: String = content
                    .lines()
                    .filter(|l| !l.contains(&config.vendor.name))
                    .collect::<Vec<_>>()
                    .join("\n");
                let _ = fs::write(&reaper_cache, filtered);
                eprintln!("  Cleaned Reaper AU cache");
            }
        }
    }

    // Flush pluginkit registrations (AU v3 appex cache)
    eprintln!("Flushing pluginkit registrations...");
    if let Ok(config) = load_config() {
        for p in &config.plugin {
            for pattern in [
                format!("com.{}.{}.v3.ext", config.vendor.id, p.suffix),
                format!("com.{}.{}.au", config.vendor.id, p.suffix),
            ] {
                let _ = Command::new("pluginkit")
                    .args(["-e", "ignore", "-i", &pattern])
                    .output();
                let _ = Command::new("pluginkit")
                    .args(["-e", "use", "-i", &pattern])
                    .output();
            }
        }

        // Force LaunchServices to re-scan v3 app bundles
        for p in &config.plugin {
            let app_path = format!("/Applications/{} v3.app", p.name);
            if Path::new(&app_path).exists() {
                let _ = Command::new("/System/Library/Frameworks/CoreServices.framework/Frameworks/LaunchServices.framework/Support/lsregister")
                    .args(["-f", "-R", &app_path])
                    .output();
            }
        }
    }

    // AAX plugin cache (Pro Tools)
    let aax_cache = PathBuf::from("/Users/Shared/Pro Tools/AAXPlugInCache");
    if aax_cache.exists() {
        if let Ok(entries) = fs::read_dir(&aax_cache) {
            for entry in entries.flatten() {
                let name = entry.file_name();
                let name = name.to_string_lossy();
                if let Ok(ref config) = load_config() {
                    if name.contains(&config.vendor.name) {
                        let _ = fs::remove_file(entry.path());
                        eprintln!("  Removed AAX cache: {}", name);
                    }
                }
            }
        }
    }

    // Clean AU v3 build temp dirs
    eprintln!("Cleaning AU v3 temp dirs...");
    if let Ok(entries) = fs::read_dir("/tmp") {
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name.starts_with("truce_au_v3_build_") || name.starts_with("truce_au_v3_fw_") {
                let _ = fs::remove_dir_all(entry.path());
                eprintln!("  Removed: {}", entry.path().display());
            }
        }
    }

    // Kill daemons to drop in-memory caches
    eprintln!("Restarting audio daemons...");
    let _ = run_sudo("killall", &["-9", "AudioComponentRegistrar"]);
    let _ = run_sudo("killall", &["-9", "pkd"]);

    eprintln!("Done. Restart your DAW to rescan.");
    Ok(())
}

// ---------------------------------------------------------------------------
// nuke — nuclear reset for stale AU v3 appex cache
// ---------------------------------------------------------------------------

fn cmd_nuke(args: &[String]) -> Res {
    let config = load_config()?;

    let mut plugin_filter: Option<String> = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "-p" => {
                i += 1;
                if i >= args.len() { return Err("-p requires a plugin suffix".into()); }
                plugin_filter = Some(args[i].clone());
            }
            other => return Err(format!("Unknown flag: {other}").into()),
        }
        i += 1;
    }

    let plugins: Vec<&PluginDef> = if let Some(ref filter) = plugin_filter {
        config.plugin.iter().filter(|p| p.suffix == *filter).collect()
    } else {
        config.plugin.iter().collect()
    };

    // 1. Unregister from LaunchServices + pluginkit
    eprintln!("Unregistering AU v3 plugins...");
    for p in &plugins {
        let app_dir = format!("/Applications/{} v3.app", p.name);
        // Unregister from LaunchServices
        let _ = Command::new("/System/Library/Frameworks/CoreServices.framework/Frameworks/LaunchServices.framework/Support/lsregister")
            .args(["-u", &app_dir])
            .output();
        // Full remove from pluginkit (not just disable)
        let vid = config.vendor.id.trim_start_matches("com.");
        for pattern in [
            format!("com.{}.{}.v3.ext", vid, p.suffix),
            format!("com.{}.{}.au", vid, p.suffix),
        ] {
            let _ = Command::new("pluginkit")
                .args(["-r", "-i", &pattern])
                .output();
        }
        // Remove the app bundle
        if Path::new(&app_dir).exists() {
            let _ = run_sudo("rm", &["-rf", &app_dir]);
            eprintln!("  Removed: {app_dir}");
        }
    }

    // 2. Kill daemons
    eprintln!("Killing audio daemons...");
    let _ = run_sudo("killall", &["-9", "pkd"]);
    let _ = run_sudo("killall", &["-9", "AudioComponentRegistrar"]);

    // 4. Clear all caches
    eprintln!("Clearing all caches...");
    let home = dirs::home_dir().unwrap();
    for dir in [
        home.join("Library/Caches/AudioUnitCache"),
        home.join("Library/Containers/com.apple.garageband10/Data/Library/Caches/AudioUnitCache"),
        home.join("Library/Containers/com.apple.logicpro10/Data/Library/Caches/AudioUnitCache"),
        home.join("Library/Caches/com.apple.logic10/AudioUnitCache"),
    ] {
        if dir.exists() {
            let _ = fs::remove_dir_all(&dir);
            eprintln!("  Removed: {}", dir.display());
        }
    }

    // 5. Clean AU v3 temp dirs
    if let Ok(entries) = fs::read_dir("/tmp") {
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name.starts_with("truce_au_v3_build_") || name.starts_with("truce_au_v3_fw_") {
                let _ = fs::remove_dir_all(entry.path());
                eprintln!("  Removed: {}", entry.path().display());
            }
        }
    }

    // 6. Cargo clean
    eprintln!("Running cargo clean...");
    let status = Command::new("cargo")
        .arg("clean")
        .status()?;
    if !status.success() {
        eprintln!("  cargo clean failed");
    }

    eprintln!("\nNuke complete. Wait a few seconds, then run:");
    eprintln!("  cargo xtask install --au3");
    Ok(())
}

// ---------------------------------------------------------------------------
// log — stream AU v3 appex logs
// ---------------------------------------------------------------------------

fn cmd_log() -> Res {
    eprintln!("Streaming AU v3 appex logs (Ctrl-C to stop)...\n");
    let status = Command::new("/usr/bin/log")
        .args([
            "stream",
            "--predicate",
            "subsystem == \"com.truce.au3\"",
            "--style", "compact",
            "--level", "debug",
        ])
        .status()?;
    if !status.success() {
        return Err("log stream exited with error".into());
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// validate
// ---------------------------------------------------------------------------

fn cmd_validate(args: &[String]) -> Res {
    let config = load_config()?;

    let mut run_auval = false;
    let mut run_auval_v3 = false;
    let mut run_pluginval = false;
    let mut run_clap = false;
    let mut plugin_filter: Option<String> = None;

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--auval" => run_auval = true,
            "--auval3" => run_auval_v3 = true,
            "--pluginval" => run_pluginval = true,
            "--clap" => run_clap = true,
            "--all" => {
                run_auval = true;
                run_auval_v3 = true;
                run_pluginval = true;
                run_clap = true;
            }
            "-p" => {
                i += 1;
                if i >= args.len() {
                    return Err("-p requires a plugin suffix".into());
                }
                plugin_filter = Some(args[i].clone());
            }
            other => return Err(format!("Unknown flag: {other}").into()),
        }
        i += 1;
    }
    if !run_auval && !run_auval_v3 && !run_pluginval && !run_clap {
        run_auval = true;
        run_auval_v3 = true;
        run_pluginval = true;
        run_clap = true;
    }

    let plugins: Vec<&PluginDef> = if let Some(ref filter) = plugin_filter {
        config.plugin.iter().filter(|p| p.suffix == *filter).collect()
    } else {
        config.plugin.iter().collect()
    };

    let mut failures = 0;

    // --- auval (macOS only, AU v2) ---
    if run_auval {
        eprintln!("=== auval (AU v2) ===\n");
        if Command::new("auval").arg("-h").output().is_ok() {
            for p in &plugins {
                eprint!(
                    "  {} ({} {} {}) ... ",
                    p.name, p.au_type, p.au_subtype, config.vendor.au_manufacturer
                );
                let output = Command::new("auval")
                    .args(["-v", &p.au_type, &p.au_subtype, &config.vendor.au_manufacturer])
                    .output()?;
                let stdout = String::from_utf8_lossy(&output.stdout);
                if stdout.contains("VALIDATION SUCCEEDED") {
                    eprintln!("PASS");
                } else {
                    eprintln!("FAIL");
                    failures += 1;
                }
            }
        } else {
            eprintln!("  auval not found (macOS only)");
        }
    }

    // --- auval (AU v3 appex) ---
    if run_auval_v3 {
        eprintln!("\n=== auval (AU v3) ===\n");
        if Command::new("auval").arg("-h").output().is_ok() {
            for p in &plugins {
                let sub = p.au3_sub();
                eprint!(
                    "  {} ({} {} {}) ... ",
                    p.name, p.au_type, sub, config.vendor.au_manufacturer
                );
                let output = Command::new("auval")
                    .args(["-v", &p.au_type, sub, &config.vendor.au_manufacturer])
                    .output()?;
                let stdout = String::from_utf8_lossy(&output.stdout);
                if stdout.contains("VALIDATION SUCCEEDED") {
                    eprintln!("PASS");
                } else {
                    eprintln!("FAIL");
                    failures += 1;
                }
            }
        } else {
            eprintln!("  auval not found (macOS only)");
        }
    }

    // --- pluginval (VST3) ---
    if run_pluginval {
        eprintln!("\n=== pluginval (VST3) ===\n");
        let pluginval = find_pluginval();
        if let Some(pv) = pluginval {
            for p in &plugins {
                let vst3_path = format!("/Library/Audio/Plug-Ins/VST3/{}.vst3", p.name);
                if !Path::new(&vst3_path).exists() {
                    eprintln!("  {} ... SKIP (not installed)", p.name);
                    continue;
                }
                eprint!("  {} ... ", p.name);
                let output = Command::new(&pv)
                    .args(["--validate", &vst3_path, "--strictness-level", "5"])
                    .output()?;
                let stdout = String::from_utf8_lossy(&output.stdout);
                if stdout.contains("SUCCESS") || output.status.success() {
                    eprintln!("PASS");
                } else {
                    eprintln!("FAIL");
                    failures += 1;
                }
            }
        } else {
            eprintln!("  pluginval not found. Install from https://github.com/Tracktion/pluginval");
        }
    }

    // --- clap-validator (CLAP) ---
    if run_clap {
        eprintln!("\n=== clap-validator (CLAP) ===\n");
        let clap_validator = find_clap_validator();
        if let Some(cv) = clap_validator {
            let clap_dir = dirs::home_dir()
                .map(|h| h.join("Library/Audio/Plug-Ins/CLAP"))
                .unwrap_or_default();
            let tmp_dir = std::env::temp_dir().join("truce-clap-validate");
            let _ = fs::create_dir_all(&tmp_dir);

            for p in &plugins {
                let clap_name = format!("{}.clap", p.name);
                let installed = clap_dir.join(&clap_name);

                if !installed.exists() {
                    eprintln!("  {} ... SKIP (not installed)", p.name);
                    continue;
                }

                // clap-validator requires bundle format (Plugin.clap/Contents/MacOS/Plugin).
                // If the installed file is a bare dylib, create a temporary bundle.
                let validate_path = if installed.join("Contents/MacOS").is_dir() {
                    installed.clone()
                } else {
                    let bundle = tmp_dir.join(&clap_name);
                    let macos = bundle.join("Contents/MacOS");
                    let _ = fs::create_dir_all(&macos);
                    let bin_name = clap_name.trim_end_matches(".clap");
                    let _ = fs::copy(&installed, macos.join(bin_name));
                    bundle
                };

                eprint!("  {} ... ", p.name);
                let output = Command::new(&cv)
                    .args(["validate", &validate_path.to_string_lossy()])
                    .output()?;

                let stdout = String::from_utf8_lossy(&output.stdout);
                let stderr = String::from_utf8_lossy(&output.stderr);
                let combined = format!("{}{}", stdout, stderr);

                if output.status.success() && !combined.contains("FAILED") {
                    // Count passed/failed from output
                    let passed = combined.matches("passed").count();
                    eprintln!("PASS ({} tests)", passed);
                } else {
                    eprintln!("FAIL");
                    if !stdout.is_empty() { eprintln!("{}", stdout); }
                    if !stderr.is_empty() { eprintln!("{}", stderr); }
                    failures += 1;
                }
            }

            let _ = fs::remove_dir_all(&tmp_dir);
        } else {
            eprintln!("  clap-validator not found.");
            eprintln!("  Install: cargo install --git https://github.com/free-audio/clap-validator");
            eprintln!("  Or set CLAP_VALIDATOR=/path/to/clap-validator");
        }
    }

    eprintln!();
    if failures > 0 {
        Err(format!("{failures} validation(s) failed").into())
    } else {
        eprintln!("All validations passed.");
        Ok(())
    }
}

fn find_pluginval() -> Option<String> {
    // Check common locations
    let candidates = [
        "/Applications/pluginval.app/Contents/MacOS/pluginval",
        "/usr/local/bin/pluginval",
    ];
    for c in candidates {
        if Path::new(c).exists() {
            return Some(c.to_string());
        }
    }
    // Check PATH
    if Command::new("pluginval").arg("--help").output().is_ok() {
        return Some("pluginval".to_string());
    }
    None
}

fn find_clap_validator() -> Option<String> {
    // Check env var override
    if let Ok(path) = std::env::var("CLAP_VALIDATOR") {
        if Path::new(&path).exists() {
            return Some(path);
        }
    }
    // Check PATH
    if Command::new("clap-validator").arg("--version").output().is_ok() {
        return Some("clap-validator".to_string());
    }
    // Check cargo install location
    if let Some(home) = dirs::home_dir() {
        let cargo_bin = home.join(".cargo/bin/clap-validator");
        if cargo_bin.exists() {
            return Some(cargo_bin.to_string_lossy().into());
        }
    }
    None
}

// ---------------------------------------------------------------------------
// dirs helper (minimal, avoids adding a dependency)
// ---------------------------------------------------------------------------

mod dirs {
    use std::path::PathBuf;

    pub fn home_dir() -> Option<PathBuf> {
        std::env::var_os("HOME").map(PathBuf::from)
    }
}

// ---------------------------------------------------------------------------
// build — produce bundles without installing
// ---------------------------------------------------------------------------

fn cmd_build(args: &[String]) -> Res {
    let config = load_config()?;
    let root = project_root();
    let dt = &config.macos.deployment_target;

    let mut plugin_filter: Option<String> = None;
    let mut dev_mode = false;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--dev" => dev_mode = true,
            "-p" => {
                i += 1;
                plugin_filter = Some(args.get(i).cloned().ok_or("-p requires a suffix")?);
            }
            other => return Err(format!("unknown flag: {other}").into()),
        }
        i += 1;
    }

    let plugins: Vec<&PluginDef> = if let Some(ref f) = plugin_filter {
        config.plugin.iter().filter(|p| p.suffix == *f).collect()
    } else {
        config.plugin.iter().collect()
    };

    if plugins.is_empty() {
        return Err("no matching plugins".into());
    }

    let bundles_dir = root.join("target/bundles");
    fs::create_dir_all(&bundles_dir)?;

    // Build CLAP + VST3 (default features)
    if dev_mode {
        eprintln!("Building (dev mode)...");
        for p in &plugins {
            cargo_build(&[], &["-p", &p.crate_name, "--features", "dev"], dt)?;
        }
        // Also build debug dylibs
        let mut cmd = Command::new("cargo");
        cmd.arg("build").arg("--workspace");
        cmd.env("MACOSX_DEPLOYMENT_TARGET", dt);
        cmd.status()?;
    } else {
        eprintln!("Building...");
        cargo_build(&[], &[], dt)?;
    }

    // Create CLAP bundles
    for p in &plugins {
        let src = root.join(format!("target/release/lib{}.dylib", p.crate_name.replace('-', "_")));
        if src.exists() {
            let clap_dir = bundles_dir.join(format!("{}.clap/Contents/MacOS", p.name));
            fs::create_dir_all(&clap_dir)?;
            fs::copy(&src, clap_dir.join(&p.name))?;

            // Codesign
            let bundle = bundles_dir.join(format!("{}.clap", p.name));
            let _ = Command::new("codesign")
                .args(["--force", "--deep", "--sign", "-"])
                .arg(&bundle)
                .status();

            eprintln!("  CLAP: {}", bundle.display());
        }
    }

    eprintln!("Bundles in {}", bundles_dir.display());
    Ok(())
}

// ---------------------------------------------------------------------------
// run — launch standalone
// ---------------------------------------------------------------------------

fn cmd_run(args: &[String]) -> Res {
    let config = load_config()?;
    let root = project_root();
    let dt = &config.macos.deployment_target;

    let mut plugin_filter: Option<String> = None;
    let mut extra_args: Vec<String> = Vec::new();
    let mut past_separator = false;
    let mut i = 0;
    while i < args.len() {
        if past_separator {
            extra_args.push(args[i].clone());
        } else {
            match args[i].as_str() {
                "-p" => {
                    i += 1;
                    plugin_filter = Some(args.get(i).cloned().ok_or("-p requires a suffix")?);
                }
                "--" => past_separator = true,
                other => return Err(format!("unknown flag: {other}").into()),
            }
        }
        i += 1;
    }

    let plugin = if let Some(ref f) = plugin_filter {
        config.plugin.iter().find(|p| p.suffix == *f)
            .ok_or_else(|| format!("no plugin with suffix '{f}'"))?
    } else {
        config.plugin.first().ok_or("no plugins in truce.toml")?
    };

    // Build with standalone feature
    eprintln!("Building {} standalone...", plugin.name);
    cargo_build(
        &[],
        &["-p", &plugin.crate_name, "--features", "standalone"],
        dt,
    )?;

    // Find the standalone binary
    let bin_name = format!("{}-standalone", plugin.suffix);
    let bin_path = root.join(format!("target/release/{bin_name}"));
    if !bin_path.exists() {
        return Err(format!(
            "standalone binary not found at {}. \
             Does your plugin have a [[bin]] target named '{bin_name}'?",
            bin_path.display()
        ).into());
    }

    eprintln!("Running {}...", bin_path.display());
    let status = Command::new(&bin_path)
        .args(&extra_args)
        .status()?;

    if !status.success() {
        return Err(format!("{} exited with {status}", bin_path.display()).into());
    }
    Ok(())
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

    // macOS tools
    eprintln!();
    eprintln!("  macOS");
    check_cmd("xcode-select", &["-p"], "Xcode CLI tools");
    check_cmd("xcodebuild", &["-version"], "xcodebuild (AU v3)");
    check_cmd("codesign", &["--help"], "codesign");

    // Compilers
    eprintln!();
    eprintln!("  Compilers");
    check_cmd("cc", &["--version"], "C compiler");
    check_cmd("c++", &["--version"], "C++ compiler (VST3 shim)");

    // Validation tools
    eprintln!();
    eprintln!("  Validation Tools");
    check_cmd("auval", &["-h"], "auval");
    check_which("pluginval");
    check_which("clap-validator");

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
        None => eprintln!("    ⚠️  AAX SDK not configured (set [macos].aax_sdk_path in truce.toml or AAX_SDK_PATH env var)"),
    }

    // Installed plugins
    eprintln!();
    eprintln!("  Installed Plugins");
    let home = dirs::home_dir().unwrap_or_default();
    count_plugins(&home.join("Library/Audio/Plug-Ins/CLAP"), "CLAP");
    count_plugins(&Path::new("/Library/Audio/Plug-Ins/VST3").to_path_buf(), "VST3");
    count_plugins(&Path::new("/Library/Audio/Plug-Ins/Components").to_path_buf(), "AU v2");
    count_plugins(&home.join("Library/Audio/Plug-Ins/VST"), "VST2");
    if root.join("rust-toolchain.toml").exists() {
        eprintln!("    ✅ rust-toolchain.toml present");
    }

    eprintln!();
    eprintln!("─────────────────────────────────────────");
    Ok(())
}

fn check_cmd(cmd: &str, args: &[&str], label: &str) {
    match Command::new(cmd).args(args).output() {
        Ok(o) if o.status.success() => {
            let ver = String::from_utf8_lossy(&o.stdout);
            let first_line = ver.lines().next().unwrap_or("").trim();
            if first_line.is_empty() {
                eprintln!("    ✅ {label}");
            } else {
                eprintln!("    ✅ {label}: {first_line}");
            }
        }
        Ok(_) => eprintln!("    ✅ {label}"),
        Err(_) => eprintln!("    ❌ {label}: not found"),
    }
}

fn check_which(name: &str) {
    match Command::new("which").arg(name).output() {
        Ok(o) if o.status.success() => {
            let path = String::from_utf8_lossy(&o.stdout).trim().to_string();
            eprintln!("    ✅ {name}: {path}");
        }
        _ => eprintln!("    ⚠️  {name}: not found"),
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
