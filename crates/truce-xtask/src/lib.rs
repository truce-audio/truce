mod templates;

#[cfg(target_os = "windows")]
mod packaging_windows;

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
        "package" => cmd_package(&args[1..]),
        "remove" => cmd_remove(&args[1..]),
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

  clean
      Clear all AU/DAW caches and restart audio daemons.

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
# CLAP and VST3 are enabled by default. VST2 and AU are opt-in.
# NOTE: VST2 is a legacy format. The Steinberg VST2 SDK was deprecated
# in 2018 and distributing VST2 plugins may require agreement with
# Steinberg's licensing terms. Enable `vst2` only if you understand
# the implications.
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
            GridWidget::knob(0, "Gain").into(),
        ])
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
            GridWidget::knob(0, "Gain").into(),
        ])
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
}}

#[cfg(feature = "static-logic")]
truce_loader::export_static! {{
    params: {struct_name}Params,
    info: plugin_info!(),
    bus_layouts: [BusLayout::stereo()],
    logic: {logic_lib}::{struct_name},
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
    let segments: Vec<&str> = s.split('-').filter(|seg| !seg.is_empty()).collect();

    let mut code: Vec<char> = segments
        .iter()
        .map(|seg| {
            seg.chars()
                .next()
                .unwrap()
                .to_uppercase()
                .next()
                .unwrap()
        })
        .collect();

    if code.len() >= 4 {
        code.truncate(4);
        return code.into_iter().collect();
    }

    let needed = 4 - code.len();
    let mut fill: Vec<char> = Vec::new();
    for seg in segments.iter().rev() {
        fill.extend(seg.chars().skip(1));
        if fill.len() >= needed {
            break;
        }
    }
    code.extend(fill.into_iter().take(needed));

    while code.len() < 4 {
        code.push('X');
    }

    code.into_iter().collect()
}

// ---------------------------------------------------------------------------
// Project configuration (read from truce.toml)
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
pub(crate) struct Config {
    #[serde(default)]
    macos: MacosConfig,
    #[serde(default)]
    pub(crate) windows: WindowsConfig,
    pub(crate) vendor: VendorConfig,
    pub(crate) plugin: Vec<PluginDef>,
    #[serde(default)]
    pub(crate) packaging: PackagingConfig,
}

#[derive(Deserialize, Default)]
pub(crate) struct WindowsConfig {
    /// Path to the AAX SDK root directory. Falls back to the AAX_SDK_PATH env var.
    aax_sdk_path: Option<String>,
    #[serde(default)]
    pub(crate) signing: WindowsSigningConfig,
    #[serde(default)]
    pub(crate) packaging: WindowsPackagingConfig,
}

/// Authenticode signing credentials for signtool. First non-empty option wins,
/// in the order Azure → thumbprint → pfx file.
#[derive(Deserialize, Default)]
pub(crate) struct WindowsSigningConfig {
    /// Azure Trusted Signing account name.
    pub(crate) azure_account: Option<String>,
    /// Azure Trusted Signing certificate profile.
    pub(crate) azure_profile: Option<String>,
    /// Azure Code Signing Dlib.dll path (defaults to standard install location).
    pub(crate) azure_dlib: Option<String>,
    /// Cert SHA1 thumbprint for a cert already in the current user's cert store.
    pub(crate) sha1: Option<String>,
    /// Cert store name. Defaults to "My".
    pub(crate) cert_store: Option<String>,
    /// Path to a .pfx file. Password via TRUCE_PFX_PASSWORD env var.
    pub(crate) pfx_path: Option<String>,
    /// RFC 3161 timestamp URL. Defaults to DigiCert.
    pub(crate) timestamp_url: Option<String>,
}

impl WindowsSigningConfig {
    /// True when any credential source is configured.
    pub(crate) fn is_configured(&self) -> bool {
        self.azure_account.is_some() || self.sha1.is_some() || self.pfx_path.is_some()
    }

    pub(crate) fn resolved_timestamp_url(&self) -> &str {
        self.timestamp_url.as_deref().unwrap_or("http://timestamp.digicert.com")
    }
}

#[derive(Deserialize, Default)]
pub(crate) struct WindowsPackagingConfig {
    /// Publisher name shown in the installer and Apps & Features.
    /// Defaults to [vendor].name when absent.
    pub(crate) publisher: Option<String>,
    /// Publisher URL shown in the installer.
    /// Defaults to [vendor].url when absent.
    pub(crate) publisher_url: Option<String>,
    /// Installer-window icon (.ico, relative to workspace root).
    pub(crate) installer_icon: Option<String>,
    /// Welcome/finish wizard bitmap (.bmp, 164x314, relative to workspace root).
    pub(crate) welcome_bmp: Option<String>,
    /// License shown on the wizard's license page (.rtf or .txt).
    pub(crate) license_rtf: Option<String>,
    /// Override for the stable `AppId` Inno Setup uses to detect upgrades.
    /// Defaults to `{vendor_id}.{suffix}` when absent.
    pub(crate) app_id: Option<String>,
}

#[derive(Deserialize, Default)]
struct MacosConfig {
    /// Path to the AAX SDK root directory. Falls back to the AAX_SDK_PATH env var.
    aax_sdk_path: Option<String>,
    #[serde(default)]
    signing: MacosSigningConfig,
    #[serde(default)]
    packaging: MacosPackagingConfig,
}

/// macOS code-signing identities. Parallels `[windows.signing]`: credentials
/// live here, installer appearance lives in `[macos.packaging]`.
#[derive(Deserialize, Default)]
struct MacosSigningConfig {
    /// `codesign -s` identity for bundles. Typically
    /// "Developer ID Application: Name (TEAMID)" or `-` for ad-hoc.
    /// Falls back to `TRUCE_SIGNING_IDENTITY` env var.
    application_identity: Option<String>,
    /// `productbuild --sign` identity for `.pkg` installers. Typically
    /// "Developer ID Installer: Name (TEAMID)".
    /// Falls back to `TRUCE_INSTALLER_SIGNING_IDENTITY` env var.
    installer_identity: Option<String>,
}

impl MacosConfig {
    /// Resolved application signing identity. `"-"` means ad-hoc / unsigned.
    /// Populated by `load_config` from `[macos.signing].application_identity`
    /// or the `TRUCE_SIGNING_IDENTITY` env var.
    fn application_identity(&self) -> &str {
        self.signing.application_identity.as_deref().unwrap_or("-")
    }

    /// Resolved installer signing identity. `None` means the installer won't
    /// be signed. Populated from `[macos.signing].installer_identity` or the
    /// `TRUCE_INSTALLER_SIGNING_IDENTITY` env var.
    fn installer_identity(&self) -> Option<&str> {
        self.signing.installer_identity.as_deref()
    }
}

#[derive(Deserialize, Default)]
struct MacosPackagingConfig {
    #[serde(default)]
    notarize: bool,
    apple_id: Option<String>,
    team_id: Option<String>,
}

#[derive(Deserialize, Default)]
pub(crate) struct PackagingConfig {
    #[serde(default)]
    pub(crate) formats: Vec<String>,
    welcome_html: Option<String>,
    license_html: Option<String>,
}

/// Resolve the application signing identity:
/// `[macos.signing].application_identity` → `TRUCE_SIGNING_IDENTITY` env →
/// `.cargo/config.toml` `[env].TRUCE_SIGNING_IDENTITY` → ad-hoc.
fn resolve_signing_identity(config: &Config) -> String {
    // 1. truce.toml explicit value
    if let Some(id) = &config.macos.signing.application_identity {
        if !id.is_empty() && id != "-" {
            return id.clone();
        }
    }
    // 2. Environment variable
    if let Ok(id) = std::env::var("TRUCE_SIGNING_IDENTITY") {
        if !id.is_empty() {
            return id;
        }
    }
    // 3. .cargo/config.toml [env] section
    if let Some(id) = read_cargo_config_env("TRUCE_SIGNING_IDENTITY") {
        return id;
    }
    "-".to_string()
}

/// Read an env var from .cargo/config.toml's [env] section.
fn read_cargo_config_env(key: &str) -> Option<String> {
    let root = project_root();
    let path = root.join(".cargo/config.toml");
    let content = fs::read_to_string(&path).ok()?;
    let doc: toml::Table = content.parse().ok()?;
    let env = doc.get("env")?.as_table()?;
    // Supports both `KEY = "value"` and `KEY = { value = "...", force = true }`
    match env.get(key)? {
        toml::Value::String(s) => Some(s.clone()),
        toml::Value::Table(t) => t.get("value")?.as_str().map(|s| s.to_string()),
        _ => None,
    }
}

/// Resolve the installer signing identity:
/// `[macos.signing].installer_identity` → `TRUCE_INSTALLER_SIGNING_IDENTITY`
/// env → `.cargo/config.toml` → None.
fn resolve_installer_identity(config: &Config) -> Option<String> {
    if let Some(ref id) = config.macos.signing.installer_identity {
        if !id.is_empty() {
            return Some(id.clone());
        }
    }
    if let Ok(id) = std::env::var("TRUCE_INSTALLER_SIGNING_IDENTITY") {
        if !id.is_empty() {
            return Some(id);
        }
    }
    if let Some(id) = read_cargo_config_env("TRUCE_INSTALLER_SIGNING_IDENTITY") {
        return Some(id);
    }
    None
}

/// Read MACOSX_DEPLOYMENT_TARGET from the environment, defaulting to "11.0".
fn deployment_target() -> String {
    std::env::var("MACOSX_DEPLOYMENT_TARGET").unwrap_or_else(|_| "11.0".to_string())
}

#[derive(Deserialize)]
pub(crate) struct VendorConfig {
    pub(crate) name: String,
    pub(crate) id: String,
    #[serde(default)]
    pub(crate) url: Option<String>,
    pub(crate) au_manufacturer: String,
}

#[derive(Deserialize)]
pub(crate) struct PluginDef {
    pub(crate) name: String,
    pub(crate) suffix: String,
    #[serde(rename = "crate")]
    pub(crate) crate_name: String,
    #[serde(default)]
    fourcc: Option<String>,
    category: String,
    #[serde(default)]
    au_type: Option<String>,
    #[serde(default)]
    au_subtype: Option<String>,
    #[serde(default)]
    au3_subtype: Option<String>,
    #[serde(default = "default_au_tag")]
    au_tag: String,
}

impl PluginDef {
    fn resolved_fourcc(&self) -> &str {
        self.fourcc.as_deref()
            .or(self.au_subtype.as_deref())
            .expect("truce.toml: each [[plugin]] requires `fourcc` or `au_subtype`")
    }
    fn resolved_au_type(&self) -> &str {
        self.au_type.as_deref().unwrap_or(
            match self.category.as_str() {
                "instrument" => "aumu",
                _ => "aufx",
            }
        )
    }
    fn au3_sub(&self) -> &str {
        self.au3_subtype.as_deref().unwrap_or(self.resolved_fourcc())
    }
    fn fw_name(&self) -> String {
        let cap = format!("{}{}", self.suffix[..1].to_uppercase(), &self.suffix[1..]);
        format!("Truce{}AU", cap)
    }
    /// Dylib filename stem derived from the crate name (hyphens → underscores).
    pub(crate) fn dylib_stem(&self) -> String {
        self.crate_name.replace('-', "_")
    }
}

/// Return the platform-specific shared library filename for a given stem.
/// macOS: `lib{stem}.dylib`, Windows: `{stem}.dll`, Linux: `lib{stem}.so`
pub(crate) fn shared_lib_name(stem: &str) -> String {
    if cfg!(target_os = "windows") {
        format!("{stem}.dll")
    } else if cfg!(target_os = "linux") {
        format!("lib{stem}.so")
    } else {
        format!("lib{stem}.dylib")
    }
}

/// Return `target/release/{shared_lib_name}` for a plugin.
pub(crate) fn release_lib(root: &Path, stem: &str) -> PathBuf {
    root.join("target/release").join(shared_lib_name(stem))
}

/// Return the release-mode library path for a specific cargo target triple,
/// or the default `target/release/` when `target` is `None`.
pub(crate) fn release_lib_for_target(
    root: &Path,
    stem: &str,
    target: Option<&str>,
) -> PathBuf {
    match target {
        Some(t) => root
            .join("target")
            .join(t)
            .join("release")
            .join(shared_lib_name(stem)),
        None => release_lib(root, stem),
    }
}

/// Return the Windows `%COMMONPROGRAMFILES%` directory (typically `C:\Program Files\Common Files`).
#[cfg(target_os = "windows")]
pub(crate) fn common_program_files() -> PathBuf {
    if let Ok(v) = env::var("CommonProgramFiles") {
        PathBuf::from(v)
    } else {
        PathBuf::from(r"C:\Program Files\Common Files")
    }
}

/// Return the Windows `%PROGRAMFILES%` directory (typically `C:\Program Files`).
#[cfg(target_os = "windows")]
pub(crate) fn program_files() -> PathBuf {
    if let Ok(v) = env::var("ProgramFiles") {
        PathBuf::from(v)
    } else {
        PathBuf::from(r"C:\Program Files")
    }
}

fn default_au_tag() -> String {
    "Effects".to_string()
}

/// Resolve the AAX SDK path: platform-specific section in truce.toml
/// → `AAX_SDK_PATH` env var → `.cargo/config.toml` → None.
pub(crate) fn resolve_aax_sdk_path(config: &Config) -> Option<PathBuf> {
    let toml_path = if cfg!(target_os = "windows") {
        (&config.windows.aax_sdk_path, "[windows].aax_sdk_path")
    } else {
        (&config.macos.aax_sdk_path, "[macos].aax_sdk_path")
    };
    if let Some(ref p) = toml_path.0 {
        let path = PathBuf::from(p);
        if path.exists() {
            return Some(path);
        }
        eprintln!("warning: {} = {:?} in truce.toml but directory not found", toml_path.1, p);
    }
    if let Ok(p) = std::env::var("AAX_SDK_PATH") {
        let path = PathBuf::from(&p);
        if path.exists() {
            return Some(path);
        }
        eprintln!("warning: AAX_SDK_PATH={p} but directory not found");
    }
    if let Some(p) = read_cargo_config_env("AAX_SDK_PATH") {
        let path = PathBuf::from(&p);
        if path.exists() {
            return Some(path);
        }
        eprintln!("warning: AAX_SDK_PATH={p} in .cargo/config.toml but directory not found");
    }
    None
}

pub(crate) fn load_config() -> std::result::Result<Config, BoxErr> {
    let root = project_root();
    let path = root.join("truce.toml");
    if !path.exists() {
        return Err(format!(
            "truce.toml not found at {}. Run 'cargo truce new' to scaffold a project, or create truce.toml manually.",
            path.display()
        )
        .into());
    }
    let content = fs::read_to_string(&path)?;
    let mut config: Config = toml::from_str(&content)?;
    if config.plugin.is_empty() {
        return Err("No [[plugin]] entries in truce.toml".into());
    }
    // Resolve both signing identities against truce.toml + env vars + .cargo/config.toml.
    // Accessor methods on MacosConfig read these resolved values.
    let resolved_app = resolve_signing_identity(&config);
    config.macos.signing.application_identity = Some(resolved_app);
    if config.macos.signing.installer_identity.is_none() {
        config.macos.signing.installer_identity = resolve_installer_identity(&config);
    }
    Ok(config)
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

pub(crate) type Res = std::result::Result<(), Box<dyn std::error::Error>>;
pub(crate) type BoxErr = Box<dyn std::error::Error>;

/// Read the version from Cargo.toml.
/// Checks `[workspace.package] version` first, then `[package] version`.
pub(crate) fn read_workspace_version(root: &Path) -> Option<String> {
    let content = fs::read_to_string(root.join("Cargo.toml")).ok()?;
    let doc: toml::Table = content.parse().ok()?;
    // Workspace layout: [workspace.package] version
    if let Some(v) = doc.get("workspace")
        .and_then(|w| w.get("package"))
        .and_then(|p| p.get("version"))
        .and_then(|v| v.as_str())
    {
        return Some(v.to_string());
    }
    // Single crate: [package] version
    doc.get("package")?
        .get("version")?
        .as_str()
        .map(|s| s.to_string())
}

/// Read the default features from the project's Cargo.toml.
pub(crate) fn detect_default_features() -> std::collections::HashSet<String> {
    let root = project_root();
    if let Ok(content) = fs::read_to_string(root.join("Cargo.toml")) {
        if let Ok(doc) = content.parse::<toml::Table>() {
            if let Some(toml::Value::Table(feat)) = doc.get("features") {
                if let Some(toml::Value::Array(defaults)) = feat.get("default") {
                    return defaults.iter()
                        .filter_map(|v| v.as_str().map(|s| s.to_string()))
                        .collect();
                }
            }
        }
    }
    // Fallback: assume all formats (workspace with multiple crates)
    ["clap", "vst3", "vst2", "au", "aax"].iter().map(|s| s.to_string()).collect()
}

pub(crate) fn project_root() -> PathBuf {
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

/// Whether the signing identity is a real Developer ID (not ad-hoc).
fn is_production_identity(identity: &str) -> bool {
    identity != "-"
}

/// Return the project-local temp directory (`target/tmp/`), creating it if needed.
pub(crate) fn tmp_dir() -> PathBuf {
    let dir = project_root().join("target/tmp");
    let _ = fs::create_dir_all(&dir);
    dir
}

/// Write entitlements.plist to a temp file and return its path.
fn write_entitlements_plist() -> PathBuf {
    let path = tmp_dir().join("entitlements.plist");
    let content = r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN"
  "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>com.apple.security.cs.allow-unsigned-executable-memory</key>
    <true/>
</dict>
</plist>"#;
    let _ = fs::write(&path, content);
    path
}

/// Code-sign a bundle. When `identity` is a Developer ID, adds hardened
/// runtime, timestamp, and entitlements (required for notarization).
/// When ad-hoc (`"-"`), performs a simple ad-hoc sign.
/// If `use_sudo` is true the codesign command runs via sudo.
fn codesign_bundle(bundle: &str, identity: &str, use_sudo: bool) -> Res {
    let production = is_production_identity(identity);
    let entitlements = write_entitlements_plist();
    let ent_path = entitlements.to_str().unwrap();

    let mut args: Vec<&str> = vec!["--force", "--deep", "--sign", identity];
    if production {
        args.extend_from_slice(&["--options", "runtime", "--timestamp"]);
        args.extend_from_slice(&["--entitlements", ent_path]);
    }
    args.push(bundle);

    if use_sudo {
        run_sudo("codesign", &args)?;
    } else {
        let status = Command::new("codesign").args(&args).status()?;
        if !status.success() {
            return Err(format!("codesign failed for {bundle}").into());
        }
    }

    // Verify signature
    if production {
        let verify_args = ["--verify", "--strict", bundle];
        if use_sudo {
            run_sudo("codesign", &verify_args)?;
        } else {
            let status = Command::new("codesign").args(verify_args).status()?;
            if !status.success() {
                return Err(format!("codesign verification failed for {bundle}").into());
            }
        }
    }

    Ok(())
}

/// Return true if `rustup` reports `triple` among its installed targets.
/// Used by `doctor` to surface cross-compile readiness.
pub(crate) fn rustup_has_target(triple: &str) -> bool {
    let out = Command::new("rustup")
        .args(["target", "list", "--installed"])
        .output();
    match out {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout)
            .lines()
            .any(|l| l.trim() == triple),
        _ => false,
    }
}

#[allow(unused_variables)]
pub(crate) fn cargo_build(env_vars: &[(&str, &str)], extra_args: &[&str], deployment_target: &str) -> Res {
    let mut cmd = Command::new("cargo");
    cmd.arg("build").arg("--release");
    #[cfg(target_os = "macos")]
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
        // No format flags specified — enable all formats that the project supports.
        // Check which features are defined in the first plugin's Cargo.toml.
        let available = detect_default_features();
        clap = available.contains("clap");
        vst3 = available.contains("vst3");
        vst2 = available.contains("vst2");
        #[cfg(target_os = "macos")]
        {
            au2 = available.contains("au");
            au3 = available.contains("au");
        }
        aax = available.contains("aax");
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
    let dt = &deployment_target();

    // Compute extra features string
    let mut extra_features = Vec::new();
    if dev_mode { extra_features.push("dev"); }
    let features_str = extra_features.join(",");

    // --- Build ---
    if !no_build {
        if clap || vst3 {
            // Build with explicit features to avoid pulling in AU ObjC
            // classes (which would collide if both AU and CLAP/VST3 bundles
            // are loaded in the same host process).
            let mut format_features: Vec<&str> = Vec::new();
            if clap { format_features.push("clap"); }
            if vst3 { format_features.push("vst3"); }
            for f in &extra_features { format_features.push(f); }
            let features_combined = format_features.join(",");

            if !extra_features.is_empty() {
                let label = extra_features.join(" + ");
                eprintln!("Building CLAP + VST3 ({label})...");
            } else {
                eprintln!("Building CLAP + VST3...");
            }
            let mut args: Vec<&str> = Vec::new();
            for p in &plugins {
                args.push("-p");
                args.push(&p.crate_name);
            }
            args.extend_from_slice(&["--no-default-features", "--features", &features_combined]);
            cargo_build(&[], &args, dt)?;
            for p in &plugins {
                let src = release_lib(&root, &p.dylib_stem());
                let dst = release_lib(&root, &format!("{}_plugin", p.dylib_stem()));
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
                let src = release_lib(&root, &p.dylib_stem());
                let dst = release_lib(&root, &format!("{}_vst2", p.dylib_stem()));
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
                let src = release_lib(&root, &p.dylib_stem());
                let dst = release_lib(&root, &format!("{}_au", p.dylib_stem()));
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
                let src = release_lib(&root, &p.dylib_stem());
                let dst = release_lib(&root, &format!("{}_aax", p.dylib_stem()));
                fs::copy(&src, &dst)?;
            }
        }

        if clap || vst3 {
            for p in &plugins {
                let saved = release_lib(&root, &format!("{}_plugin", p.dylib_stem()));
                let dst = release_lib(&root, &p.dylib_stem());
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
            #[cfg(target_os = "macos")]
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
            install_clap(&root, p, &config)?;
        }
        if vst3 {
            install_vst3(&root, p, &config)?;
        }
        if vst2 {
            install_vst2(&root, p, &config)?;
        }
        if au2 {
            install_au(&root, p, &config)?;
        }
        if aax {
            install_aax(&root, p, &config)?;
        }
    }

    if au3 {
        build_and_install_au_v3(&root, &config, &plugins, no_build)?;
    }

    #[cfg(target_os = "macos")]
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

fn install_clap(root: &Path, p: &PluginDef, config: &Config) -> Res {
    let dylib = release_lib(root, &p.dylib_stem());
    if !dylib.exists() {
        return Err(format!("Missing: {}", dylib.display()).into());
    }

    #[cfg(target_os = "macos")]
    {
        let clap_dir = dirs::home_dir()
            .unwrap()
            .join("Library/Audio/Plug-Ins/CLAP");
        fs::create_dir_all(&clap_dir)?;
        let dst = clap_dir.join(format!("{}.clap", p.name));
        fs::copy(&dylib, &dst)?;
        codesign_bundle(dst.to_str().unwrap(), config.macos.application_identity(), false)?;
        eprintln!("CLAP: {}", dst.display());
    }

    #[cfg(target_os = "windows")]
    {
        let clap_dir = common_program_files().join("CLAP");
        fs::create_dir_all(&clap_dir)?;
        let dst = clap_dir.join(format!("{}.clap", p.name));
        fs::copy(&dylib, &dst)?;
        eprintln!("CLAP: {}", dst.display());
    }

    #[cfg(target_os = "linux")]
    {
        let clap_dir = dirs::home_dir().unwrap().join(".clap");
        fs::create_dir_all(&clap_dir)?;
        let dst = clap_dir.join(format!("{}.clap", p.name));
        fs::copy(&dylib, &dst)?;
        eprintln!("CLAP: {}", dst.display());
    }

    Ok(())
}

fn install_vst3(root: &Path, p: &PluginDef, config: &Config) -> Res {
    let dylib = release_lib(root, &p.dylib_stem());
    if !dylib.exists() {
        return Err(format!("Missing: {}", dylib.display()).into());
    }

    #[cfg(target_os = "macos")]
    {
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
        let plist_tmp = tmp_dir().join(format!("{}_vst3.plist", p.suffix)).to_string_lossy().to_string();
        fs::write(&plist_tmp, &plist)?;
        run_sudo("cp", &[&plist_tmp, &format!("{contents}/Info.plist")])?;
        codesign_bundle(&vst3_bundle, config.macos.application_identity(), true)?;
        eprintln!("VST3: {vst3_bundle}");
    }

    #[cfg(target_os = "windows")]
    {
        // VST3 on Windows: %COMMONPROGRAMFILES%\VST3\{name}.vst3\Contents\x86_64-win\{name}.vst3
        let vst3_dir = common_program_files().join("VST3");
        let bundle = vst3_dir.join(format!("{}.vst3", p.name));
        let arch_dir = bundle.join("Contents").join("x86_64-win");
        fs::create_dir_all(&arch_dir)?;
        let dst = arch_dir.join(format!("{}.vst3", p.name));
        fs::copy(&dylib, &dst)?;
        eprintln!("VST3: {}", bundle.display());
    }

    #[cfg(target_os = "linux")]
    {
        let vst3_dir = dirs::home_dir().unwrap().join(".vst3");
        let bundle = vst3_dir.join(format!("{}.vst3", p.name));
        let arch_dir = bundle.join("Contents").join("x86_64-linux");
        fs::create_dir_all(&arch_dir)?;
        let dst = arch_dir.join(format!("{}.so", p.name));
        fs::copy(&dylib, &dst)?;
        eprintln!("VST3: {}", bundle.display());
    }

    Ok(())
}

fn install_vst2(root: &Path, p: &PluginDef, config: &Config) -> Res {
    let dylib = release_lib(root, &format!("{}_vst2", p.dylib_stem()));
    if !dylib.exists() {
        return Err(format!("Missing: {}", dylib.display()).into());
    }

    #[cfg(target_os = "macos")]
    {
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

        codesign_bundle(bundle.to_str().unwrap(), config.macos.application_identity(), false)?;
        eprintln!("VST2: {}", bundle.display());
    }

    #[cfg(target_os = "windows")]
    {
        // VST2 on Windows: %PROGRAMFILES%\Steinberg\VstPlugins\{name}.dll
        // This is the Steinberg default path that Reaper and most hosts scan by default.
        let vst_dir = program_files().join("Steinberg").join("VstPlugins");
        fs::create_dir_all(&vst_dir)?;
        let dst = vst_dir.join(format!("{}.dll", p.name));
        fs::copy(&dylib, &dst)?;
        eprintln!("VST2: {}", dst.display());
    }

    #[cfg(target_os = "linux")]
    {
        let vst_dir = dirs::home_dir().unwrap().join(".vst");
        fs::create_dir_all(&vst_dir)?;
        let dst = vst_dir.join(format!("{}.so", p.name));
        fs::copy(&dylib, &dst)?;
        eprintln!("VST2: {}", dst.display());
    }

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
        au_type = p.resolved_au_type(),
        au_subtype = p.resolved_fourcc(),
        au_mfr = config.vendor.au_manufacturer,
        au_tag = p.au_tag,
    );
    let plist_tmp = tmp_dir().join(format!("{}_au.plist", p.suffix)).to_string_lossy().to_string();
    fs::write(&plist_tmp, &plist)?;
    run_sudo("cp", &[&plist_tmp, &format!("{contents}/Info.plist")])?;
    codesign_bundle(&bundle, config.macos.application_identity(), true)?;
    eprintln!("AU:   {bundle}");
    Ok(())
}

// ---------------------------------------------------------------------------
// AAX install
// ---------------------------------------------------------------------------

/// Build the AAX C++ template bundle.
///
/// `universal_mac` is only meaningful on macOS: when `true`, the cmake
/// invocation sets `CMAKE_OSX_ARCHITECTURES="arm64;x86_64"` so the resulting
/// `TruceAAXTemplate` binary is a fat Mach-O that runs on both Apple Silicon
/// and Intel hosts. On Windows (and for host-only macOS builds) the flag is
/// ignored — cmake builds for the host arch only, consistent with the Windows
/// `packaging_windows.rs` side of the story, which keeps AAX host-arch even in
/// `--universal` mode.
pub(crate) fn build_aax_template(_root: &Path, sdk_path: &Path, universal_mac: bool) -> Res {
    // Referenced only by the macOS cmake branch below; touch it on Windows so
    // the parameter doesn't trip the unused-variable lint.
    #[cfg(target_os = "windows")]
    let _ = universal_mac;

    // Write embedded template files to a temp directory
    let template_dir = tmp_dir().join("aax_template");
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

    #[cfg(not(target_os = "windows"))]
    {
        let mut configure = Command::new("cmake");
        configure
            .arg("-B")
            .arg(&build_dir)
            .arg(format!("-DAAX_SDK_PATH={}", sdk_path.display()));
        if universal_mac {
            configure.arg("-DCMAKE_OSX_ARCHITECTURES=arm64;x86_64");
        }
        let status = configure.current_dir(&template_dir).status()?;
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
    }

    // Windows: cmake's "Visual Studio N YYYY" generators are tied to a specific
    // VS version the cmake binary ships with. If the user's cmake predates the
    // installed VS (common: VS 2026 with an older cmake), the VS generator
    // fails to find MSBuild. Work around this by using the Ninja generator and
    // wrapping the invocation in a vcvars-setup .bat so cl.exe/link.exe are
    // reachable. Ninja also avoids the multi-config output layout.
    #[cfg(target_os = "windows")]
    {
        let vcvars = locate_vcvars64()
            .ok_or("could not locate vcvars64.bat — install VS 2022+ with the C++ workload")?;

        // cmake + ninja aren't necessarily on %PATH% when running outside the
        // truce repo (truce's .cargo/config.toml historically set it). vcvars
        // doesn't add them either. Resolve both explicitly and prepend their
        // directories to the .bat's PATH so the build works from any project.
        let cmake = locate_cmake()
            .ok_or("could not locate cmake.exe — install cmake or the VS \"C++ CMake tools\" component")?;
        let ninja = locate_ninja()
            .ok_or("could not locate ninja.exe — install ninja or the VS \"C++ CMake tools\" component (which bundles it)")?;
        let cmake_dir = cmake.parent().unwrap().display().to_string();
        let ninja_dir = ninja.parent().unwrap().display().to_string();

        // CMake 3.20+ rejects `\U` etc. as invalid escape sequences when a
        // backslash path is interpolated into a generated string literal.
        // Convert all paths we pass to cmake to forward slashes.
        let to_fwd = |p: &Path| p.display().to_string().replace('\\', "/");

        let bat_path = tmp_dir().join("truce_aax_build.bat");
        let bat = format!(
            "@echo off\r\n\
             call \"{vcvars}\" >nul || exit /b 1\r\n\
             set \"PATH={cmake_dir};{ninja_dir};%PATH%\"\r\n\
             cmake -S \"{src}\" -B \"{build}\" -G Ninja -DCMAKE_BUILD_TYPE=Release \"-DAAX_SDK_PATH={sdk}\" || exit /b 1\r\n\
             cmake --build \"{build}\" || exit /b 1\r\n",
            vcvars = vcvars.display(),
            cmake_dir = cmake_dir,
            ninja_dir = ninja_dir,
            src = to_fwd(&template_dir),
            build = to_fwd(&build_dir),
            sdk = to_fwd(sdk_path),
        );
        fs::write(&bat_path, bat)?;

        let status = Command::new("cmd")
            .arg("/c")
            .arg(&bat_path)
            .status()?;
        if !status.success() {
            return Err("AAX cmake+ninja build failed".into());
        }
    }
    Ok(())
}

/// Search for `name` (must include `.exe`) on `%PATH%`, returning the first
/// hit. Cross-platform equivalent of `where.exe`.
#[cfg(target_os = "windows")]
fn which_exe(name: &str) -> Option<PathBuf> {
    let path = env::var_os("PATH")?;
    for dir in env::split_paths(&path) {
        let candidate = dir.join(name);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

/// Locate `cmake.exe`. Tries `%PATH%` first, then the CMake that ships with
/// Visual Studio's "C++ CMake tools" component, then the standalone installer
/// default. Returns `None` if none are present.
#[cfg(target_os = "windows")]
fn locate_cmake() -> Option<PathBuf> {
    if let Some(p) = which_exe("cmake.exe") {
        return Some(p);
    }
    for vs_install in vs_install_paths() {
        let bundled = vs_install
            .join(r"Common7\IDE\CommonExtensions\Microsoft\CMake\CMake\bin\cmake.exe");
        if bundled.is_file() {
            return Some(bundled);
        }
    }
    for c in [
        r"C:\Program Files\CMake\bin\cmake.exe",
        r"C:\Program Files (x86)\CMake\bin\cmake.exe",
    ] {
        let p = PathBuf::from(c);
        if p.is_file() {
            return Some(p);
        }
    }
    None
}

/// Locate `ninja.exe`. Same strategy as cmake — the VS CMake component bundles
/// Ninja next to it, so that's the most common path on machines that have VS
/// with "C++ CMake tools" installed.
#[cfg(target_os = "windows")]
fn locate_ninja() -> Option<PathBuf> {
    if let Some(p) = which_exe("ninja.exe") {
        return Some(p);
    }
    for vs_install in vs_install_paths() {
        let bundled = vs_install
            .join(r"Common7\IDE\CommonExtensions\Microsoft\CMake\Ninja\ninja.exe");
        if bundled.is_file() {
            return Some(bundled);
        }
    }
    None
}

/// Enumerate all VS installation roots known to `vswhere.exe`. Returned in
/// the order vswhere produces (latest first when called with `-latest`, or
/// all installs otherwise). We pass no filter here so we also pick up the old
/// VS 2022 install that's useful for CMake/Ninja even when its C++ workload
/// is broken.
#[cfg(target_os = "windows")]
pub(crate) fn vs_install_paths() -> Vec<PathBuf> {
    let vswhere = PathBuf::from(
        r"C:\Program Files (x86)\Microsoft Visual Studio\Installer\vswhere.exe",
    );
    if !vswhere.exists() {
        return Vec::new();
    }
    let out = Command::new(&vswhere)
        .args(["-all", "-property", "installationPath", "-format", "value"])
        .output();
    match out {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout)
            .lines()
            .map(|l| l.trim())
            .filter(|l| !l.is_empty())
            .map(PathBuf::from)
            .collect(),
        _ => Vec::new(),
    }
}

/// Locate `vcvars64.bat` via `vswhere.exe`. Returns `None` if VS isn't
/// installed with the C++ tools component.
#[cfg(target_os = "windows")]
fn locate_vcvars64() -> Option<PathBuf> {
    let vswhere = PathBuf::from(
        r"C:\Program Files (x86)\Microsoft Visual Studio\Installer\vswhere.exe",
    );
    if !vswhere.exists() {
        return None;
    }
    let out = Command::new(&vswhere)
        .args([
            "-latest",
            "-requires",
            "Microsoft.VisualStudio.Component.VC.Tools.x86.x64",
            "-property",
            "installationPath",
            "-format",
            "value",
        ])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let install = String::from_utf8(out.stdout).ok()?;
    let install = install.trim();
    if install.is_empty() {
        return None;
    }
    let vcvars = PathBuf::from(install).join(r"VC\Auxiliary\Build\vcvars64.bat");
    if vcvars.exists() {
        Some(vcvars)
    } else {
        None
    }
}

fn install_aax(root: &Path, p: &PluginDef, config: &Config) -> Res {
    // AAX is only supported on macOS and Windows. On Linux (including WSL
    // builds that happen to target Linux), short-circuit before referencing
    // any platform-specific helpers.
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        let _ = (root, p, config);
        eprintln!("AAX: not supported on this platform, skipping {}", p.name);
        return Ok(());
    }

    /// Template binary path inside the cmake build directory.
    #[cfg(target_os = "macos")]
    fn template_binary() -> PathBuf {
        tmp_dir().join("aax_template/build/TruceAAXTemplate.aaxplugin/Contents/MacOS/TruceAAXTemplate")
    }
    #[cfg(target_os = "windows")]
    fn template_binary() -> PathBuf {
        // Ninja is single-config — target lands directly in the build dir.
        // CMakeLists.txt sets SUFFIX=.aaxplugin, PREFIX="".
        tmp_dir().join("aax_template/build/TruceAAXTemplate.aaxplugin")
    }

    #[cfg(any(target_os = "macos", target_os = "windows"))]
    let template = template_binary();
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    let template: PathBuf = unreachable!();
    if !template.exists() {
        if let Some(sdk_path) = resolve_aax_sdk_path(config) {
            eprintln!("AAX: building template with SDK at {}", sdk_path.display());
            // `install` only needs the host arch — universal template builds
            // are reserved for the packaging path (`cargo truce package`).
            build_aax_template(root, &sdk_path, false)?;
        } else {
            let hint = if cfg!(target_os = "windows") {
                "[windows].aax_sdk_path"
            } else {
                "[macos].aax_sdk_path"
            };
            eprintln!(
                "AAX: template not built, skipping.\n  \
                 Set {hint} in truce.toml or AAX_SDK_PATH env var."
            );
            return Ok(());
        }
    }
    if !template.exists() {
        return Err(format!(
            "AAX template build succeeded but binary not found at {}",
            template.display()
        )
        .into());
    }

    let dylib = release_lib(root, &format!("{}_aax", p.dylib_stem()));
    if !dylib.exists() {
        eprintln!("AAX: {} not found, skipping {}", dylib.display(), p.name);
        return Ok(());
    }

    #[cfg(target_os = "macos")]
    {
        let aax_dir = "/Library/Application Support/Avid/Audio/Plug-Ins";
        let bundle = format!("{aax_dir}/{}.aaxplugin", p.name);
        let contents = format!("{bundle}/Contents");

        run_sudo("rm", &["-rf", &bundle])?;
        run_sudo("mkdir", &["-p", &format!("{contents}/MacOS")])?;
        run_sudo("mkdir", &["-p", &format!("{contents}/Resources")])?;

        run_sudo(
            "cp",
            &[
                template.to_str().unwrap(),
                &format!("{contents}/MacOS/{}", p.name),
            ],
        )?;

        run_sudo(
            "cp",
            &[
                dylib.to_str().unwrap(),
                &format!("{contents}/Resources/"),
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
        let plist_tmp = tmp_dir().join(format!("{}_aax.plist", p.suffix)).to_string_lossy().to_string();
        fs::write(&plist_tmp, &plist)?;
        run_sudo("cp", &[&plist_tmp, &format!("{contents}/Info.plist")])?;

        codesign_bundle(&bundle, config.macos.application_identity(), true)?;
        eprintln!("AAX:  {bundle}");
    }

    #[cfg(target_os = "windows")]
    {
        // Windows AAX bundle layout:
        //   Plugin.aaxplugin/
        //     Contents/
        //       x64/
        //         Plugin.aaxplugin       (template binary, the .dll we built)
        //       Resources/
        //         {name}_aax.dll         (Rust cdylib)
        //
        // Install to %COMMONPROGRAMFILES%\Avid\Audio\Plug-Ins\
        let aax_dir = common_program_files().join("Avid").join("Audio").join("Plug-Ins");
        let bundle = aax_dir.join(format!("{}.aaxplugin", p.name));
        let contents = bundle.join("Contents");
        let x64_dir = contents.join("x64");
        let resources_dir = contents.join("Resources");

        let _ = fs::remove_dir_all(&bundle);
        fs::create_dir_all(&x64_dir)?;
        fs::create_dir_all(&resources_dir)?;

        fs::copy(&template, x64_dir.join(format!("{}.aaxplugin", p.name)))?;
        fs::copy(&dylib, resources_dir.join(format!("{}_aax.dll", p.dylib_stem())))?;

        eprintln!("AAX:  {}", bundle.display());
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// AU v3 appex install (Swift + xcodebuild)
// ---------------------------------------------------------------------------

/// Build AU v3 appex bundles (Rust framework + xcodebuild). Does not install.
///
/// `archs` controls the Mach-O slices produced for the embedded Rust
/// framework and the xcodebuild `ARCHS` flag. Callers pass `&[host]` for the
/// default / `install` path and `&[X86_64, Arm64]` for universal `package`
/// runs.
fn build_au_v3(
    root: &Path,
    config: &Config,
    plugins: &[&PluginDef],
    no_build: bool,
    archs: &[MacArch],
) -> Res {
    let sign_id = config.macos.application_identity();
    let team_id = extract_team_id(sign_id);
    let dt = &deployment_target();

    if team_id.is_empty() {
        eprintln!("AU v3: skipping — requires a Developer ID signing identity with a team ID.");
        eprintln!("  Set [macos.signing].application_identity in truce.toml to your Developer ID certificate,");
        eprintln!("  e.g., \"Developer ID Application: Your Name (TEAMID)\"");
        eprintln!("  Ad-hoc signing (\"-\") is not supported for AU v3 appex bundles.");
        return Ok(());
    }

    if archs.is_empty() {
        return Err("build_au_v3: empty archs list".into());
    }

    for p in plugins {
        let fw_name = p.fw_name();
        let au_v3_sub = p.au3_sub();
        let app_dir = format!("/Applications/{} v3.app", p.name);
        let appex_id = format!(
            "com.{}.{}.v3.ext",
            config.vendor.id.trim_start_matches("com."),
            p.suffix
        );
        let build_dir = tmp_dir().join(format!("au_v3_build_{}", p.suffix));
        let fw_build = tmp_dir().join(format!("au_v3_fw_{}", p.suffix));

        eprintln!("Building AU v3 ({})...", p.name);

        if !no_build {
            // Step 1: Build Rust framework, once per arch, then lipo into the
            // canonical `lib{stem}_v3.dylib` location.
            for &arch in archs {
                eprintln!("  Building Rust framework ({})...", arch.triple());
                cargo_build_for_arch(
                    &[("TRUCE_AU_VERSION", "3"), ("TRUCE_AU_PLUGIN_ID", &p.suffix)],
                    &[
                        "-p",
                        &p.crate_name,
                        "--no-default-features",
                        "--features",
                        "au",
                    ],
                    arch,
                    dt,
                )?;
                let src = release_lib_for_target(
                    root,
                    &p.dylib_stem(),
                    Some(arch.triple()),
                );
                let saved = release_lib_for_target(
                    root,
                    &format!("{}_v3", p.dylib_stem()),
                    Some(arch.triple()),
                );
                fs::copy(&src, &saved)?;
            }
            let fw_inputs: Vec<PathBuf> = archs
                .iter()
                .map(|a| {
                    release_lib_for_target(
                        root,
                        &format!("{}_v3", p.dylib_stem()),
                        Some(a.triple()),
                    )
                })
                .collect();
            let dst = root.join(format!(
                "target/release/lib{}_v3.dylib",
                p.dylib_stem()
            ));
            lipo_into(&fw_inputs, &dst)?;

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
            #[cfg(unix)]
            {
                std::os::unix::fs::symlink("A", fw_root.join("Versions/Current"))?;
                std::os::unix::fs::symlink(
                    format!("Versions/Current/{}", fw_name),
                    fw_root.join(&fw_name),
                )?;
                std::os::unix::fs::symlink("Versions/Current/Resources", fw_root.join("Resources"))?;
            }
            #[cfg(not(unix))]
            {
                return Err("AU v3 framework builds are only supported on macOS".into());
            }

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

            {
                let mut cs_args = vec!["--force", "--sign", sign_id];
                if is_production_identity(sign_id) {
                    cs_args.extend_from_slice(&["--options", "runtime", "--timestamp"]);
                }
                cs_args.push(fw_root.to_str().unwrap());
                let status = Command::new("codesign").args(&cs_args).status()?;
                if !status.success() {
                    return Err("codesign failed for AU v3 framework".into());
                }
            }

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
                .replace("AUTYPE", &p.resolved_au_type())
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
            // ARCHS reflects the requested slices. ONLY_ACTIVE_ARCH=NO forces
            // xcodebuild to build every listed arch regardless of host — the
            // default flips to YES in Debug and NO in Release, but we pin it
            // explicitly so dev paths (Debug) also produce the full set.
            let archs_flag = format!(
                "ARCHS={}",
                archs
                    .iter()
                    .map(|a| match a {
                        MacArch::X86_64 => "x86_64",
                        MacArch::Arm64 => "arm64",
                    })
                    .collect::<Vec<_>>()
                    .join(" ")
            );
            let output = Command::new("xcodebuild")
                .current_dir(&build_dir)
                .args([
                    "-project",
                    "XcodeAUv3.xcodeproj",
                    "-target",
                    "TruceAUv3",
                    "-configuration",
                    "Release",
                ])
                .arg(&archs_flag)
                .arg("ONLY_ACTIVE_ARCH=NO")
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

        let built_app = build_dir.join("build/Release/TruceAUv3.app");
        if !built_app.exists() {
            return Err(format!("Built app not found: {}", built_app.display()).into());
        }
    }
    Ok(())
}

/// Install pre-built AU v3 appex bundles to /Applications/ and register.
fn install_au_v3(
    config: &Config,
    plugins: &[&PluginDef],
) -> Res {
    let sign_id = config.macos.application_identity();

    for p in plugins {
        let fw_name = p.fw_name();
        let app_dir = format!("/Applications/{} v3.app", p.name);
        let appex_id = format!(
            "com.{}.{}.v3.ext",
            config.vendor.id.trim_start_matches("com."),
            p.suffix
        );
        let build_dir = tmp_dir().join(format!("au_v3_build_{}", p.suffix));
        let fw_build = tmp_dir().join(format!("au_v3_fw_{}", p.suffix));
        let built_app = build_dir.join("build/Release/TruceAUv3.app");
        if !built_app.exists() {
            return Err(format!("AU v3 not built for {}. Run build first.", p.name).into());
        }

        {
            // Pre-clean
            let _ = Command::new("pluginkit")
                .args(["-e", "ignore", "-i", &appex_id])
                .output();
            let _ = run_sudo("rm", &["-rf", &app_dir]);

            // Install to /Applications/
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
            let production = is_production_identity(sign_id);
            let runtime_flags: &[&str] = if production {
                &["--options", "runtime", "--timestamp"]
            } else {
                &[]
            };

            {
                let fw_path = format!("{app_dir}/Contents/Frameworks/{fw_name}.framework");
                let mut args = vec!["--force", "--sign", sign_id];
                args.extend_from_slice(runtime_flags);
                args.push(&fw_path);
                run_sudo("codesign", &args)?;
            }
            let entitlements_appex = build_dir.join("AUExt/AUExt.entitlements");
            let entitlements_app = build_dir.join("App/App.entitlements");
            {
                let appex_path = format!("{app_dir}/Contents/PlugIns/AUExt.appex");
                let ent = entitlements_appex.to_str().unwrap();
                let mut args = vec!["--force", "--sign", sign_id, "--entitlements", ent, "--generate-entitlement-der"];
                args.extend_from_slice(runtime_flags);
                args.push(&appex_path);
                run_sudo("codesign", &args)?;
            }
            {
                let ent = entitlements_app.to_str().unwrap();
                let mut args = vec!["--force", "--sign", sign_id, "--entitlements", ent, "--generate-entitlement-der"];
                args.extend_from_slice(runtime_flags);
                args.push(&app_dir);
                run_sudo("codesign", &args)?;
            }

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
    }
    Ok(())
}

fn build_and_install_au_v3(
    root: &Path,
    config: &Config,
    plugins: &[&PluginDef],
    no_build: bool,
) -> Res {
    // `cargo truce install` only needs the host arch — universal builds are
    // reserved for the packaging path.
    build_au_v3(root, config, plugins, no_build, &[MacArch::host()])?;
    install_au_v3(config, plugins)
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
pub(crate) fn copy_dir_recursive(src: &Path, dst: &Path) -> Res {
    fs::create_dir_all(dst)?;
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let src_path = entry.path();
        let dst_path = dst.join(entry.file_name());
        let ft = entry.file_type()?;
        // Preserve symlinks (critical for macOS .framework bundles)
        #[cfg(unix)]
        if ft.is_symlink() {
            let target = fs::read_link(&src_path)?;
            let _ = fs::remove_file(&dst_path);
            std::os::unix::fs::symlink(&target, &dst_path)?;
            continue;
        }
        if ft.is_dir() {
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
                    .env("MACOSX_DEPLOYMENT_TARGET", &deployment_target())
                    .output()?;
                if !build.status.success() {
                    eprintln!("BUILD FAILED");
                    all_passed = false;
                    continue;
                }
                let dylib = root.join(format!("target/release/lib{}.dylib", p.dylib_stem()));
                let is_synth = p.resolved_au_type() == "aumu";
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
    let tmp = tmp_dir();
    if let Ok(entries) = fs::read_dir(&tmp) {
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name.starts_with("au_v3_build_") || name.starts_with("au_v3_fw_") {
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
    if let Ok(entries) = fs::read_dir(&tmp_dir()) {
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name.starts_with("au_v3_build_") || name.starts_with("au_v3_fw_") {
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
// remove — uninstall plugin bundles
// ---------------------------------------------------------------------------

struct RemoveTarget {
    format: &'static str,
    path: PathBuf,
    needs_sudo: bool,
}

fn confirm_prompt(message: &str) -> bool {
    eprint!("{message} [y/N] ");
    let mut input = String::new();
    std::io::stdin().read_line(&mut input).ok();
    matches!(input.trim(), "y" | "Y" | "yes" | "YES")
}

fn unregister_au3(config: &Config, plugin: &PluginDef, app_path: &Path) {
    let vid = config.vendor.id.trim_start_matches("com.");
    for pattern in [
        format!("com.{}.{}.v3.ext", vid, plugin.suffix),
        format!("com.{}.{}.au", vid, plugin.suffix),
    ] {
        let _ = Command::new("pluginkit")
            .args(["-e", "ignore", "-i", &pattern])
            .output();
        let _ = Command::new("pluginkit")
            .args(["-r", "-i", &pattern])
            .output();
    }
    let _ = Command::new(
        "/System/Library/Frameworks/CoreServices.framework/\
         Frameworks/LaunchServices.framework/Support/lsregister",
    )
    .args(["-u", app_path.to_str().unwrap_or("")])
    .output();
}

fn clear_au_caches() {
    let home = dirs::home_dir().unwrap();
    for dir in [
        home.join("Library/Caches/AudioUnitCache"),
        home.join("Library/Containers/com.apple.garageband10/Data/Library/Caches/AudioUnitCache"),
        home.join("Library/Containers/com.apple.logicpro10/Data/Library/Caches/AudioUnitCache"),
        home.join("Library/Caches/com.apple.logic10/AudioUnitCache"),
    ] {
        let _ = fs::remove_dir_all(&dir);
    }
    let _ = Command::new("killall")
        .args(["-9", "AudioComponentRegistrar"])
        .output();
}

fn cmd_remove(args: &[String]) -> Res {
    let config = load_config()?;

    let mut clap = false;
    let mut vst3 = false;
    let mut vst2 = false;
    let mut au2 = false;
    let mut au3 = false;
    let mut aax = false;
    let mut dry_run = false;
    let mut yes = false;
    let mut stale = false;
    let mut suffix_filter: Option<String> = None;
    let mut name_filter: Option<String> = None;

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--clap" => clap = true,
            "--vst3" => vst3 = true,
            "--vst2" => vst2 = true,
            "--au2" => au2 = true,
            "--au3" => au3 = true,
            "--aax" => aax = true,
            "--dry-run" => dry_run = true,
            "--yes" | "-y" => yes = true,
            "--stale" => stale = true,
            "-p" => {
                i += 1;
                suffix_filter = Some(
                    args.get(i)
                        .cloned()
                        .ok_or("-p requires a plugin suffix")?,
                );
            }
            "-n" => {
                i += 1;
                name_filter = Some(
                    args.get(i)
                        .cloned()
                        .ok_or("-n requires a plugin name")?,
                );
            }
            other => return Err(format!("Unknown flag: {other}").into()),
        }
        i += 1;
    }

    // Default: all formats if none specified
    if !clap && !vst3 && !vst2 && !au2 && !au3 && !aax {
        clap = true;
        vst3 = true;
        vst2 = true;
        au2 = true;
        au3 = true;
        aax = true;
    }

    let home = dirs::home_dir().unwrap();
    let vendor = &config.vendor.name;
    let known_names: Vec<&str> = config.plugin.iter().map(|p| p.name.as_str()).collect();

    let mut targets: Vec<RemoveTarget> = Vec::new();

    if stale {
        // --stale: find vendor-matching bundles NOT in the current project
        let scan = |dir: &Path, ext: &str, format: &'static str, needs_sudo: bool, targets: &mut Vec<RemoveTarget>| {
            if let Ok(entries) = fs::read_dir(dir) {
                for entry in entries.flatten() {
                    let name = entry.file_name();
                    let name = name.to_string_lossy();
                    if !name.contains(vendor) { continue; }
                    // Strip extension to get the display name
                    let display = name.trim_end_matches(&format!(".{ext}"));
                    if known_names.iter().any(|k| *k == display) { continue; }
                    targets.push(RemoveTarget { format, path: entry.path(), needs_sudo });
                }
            }
        };

        if clap {
            scan(&home.join("Library/Audio/Plug-Ins/CLAP"), "clap", "CLAP", false, &mut targets);
            scan(Path::new("/Library/Audio/Plug-Ins/CLAP"), "clap", "CLAP", true, &mut targets);
        }
        if vst3 {
            scan(Path::new("/Library/Audio/Plug-Ins/VST3"), "vst3", "VST3", true, &mut targets);
            scan(&home.join("Library/Audio/Plug-Ins/VST3"), "vst3", "VST3", false, &mut targets);
        }
        if vst2 {
            scan(&home.join("Library/Audio/Plug-Ins/VST"), "vst", "VST2", false, &mut targets);
            scan(Path::new("/Library/Audio/Plug-Ins/VST"), "vst", "VST2", true, &mut targets);
        }
        if au2 {
            scan(Path::new("/Library/Audio/Plug-Ins/Components"), "component", "AU v2", true, &mut targets);
            scan(&home.join("Library/Audio/Plug-Ins/Components"), "component", "AU v2", false, &mut targets);
        }
        if au3 {
            // Scan /Applications for vendor-matching v3 apps not in project
            if let Ok(entries) = fs::read_dir("/Applications") {
                for entry in entries.flatten() {
                    let name = entry.file_name();
                    let name_str = name.to_string_lossy();
                    if !name_str.contains(vendor) || !name_str.ends_with("v3.app") { continue; }
                    let display = name_str.trim_end_matches(" v3.app");
                    if known_names.iter().any(|k| *k == display) { continue; }
                    targets.push(RemoveTarget { format: "AU v3", path: entry.path(), needs_sudo: true });
                }
            }
        }
        if aax {
            scan(Path::new("/Library/Application Support/Avid/Audio/Plug-Ins"), "aaxplugin", "AAX", true, &mut targets);
        }

        // Apply -p (substring match on filename) or -n (exact display name match)
        if let Some(ref filter) = suffix_filter {
            let filter_lower = filter.to_lowercase();
            targets.retain(|t| {
                t.path.file_name()
                    .map(|f| f.to_string_lossy().to_lowercase().contains(&filter_lower))
                    .unwrap_or(false)
            });
        } else if let Some(ref filter) = name_filter {
            let filter_lower = filter.to_lowercase();
            targets.retain(|t| {
                let fname = t.path.file_stem()
                    .map(|f| f.to_string_lossy().to_lowercase())
                    .unwrap_or_default();
                // Strip " v3" suffix for AU v3 app names
                let display = fname.trim_end_matches(" v3");
                display == filter_lower
            });
        }
    } else {
        // Normal mode: remove bundles for plugins in the project

        // Filter plugins by suffix (-p) or display name (-n)
        let plugins: Vec<&PluginDef> = if let Some(ref filter) = suffix_filter {
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
                        .map(|p| format!("{} (-p {})", p.name, p.suffix))
                        .collect::<Vec<_>>()
                        .join(", ")
                )
                .into());
            }
            matched
        } else if let Some(ref filter) = name_filter {
            let filter_lower = filter.to_lowercase();
            let matched: Vec<_> = config
                .plugin
                .iter()
                .filter(|p| p.name.to_lowercase() == filter_lower)
                .collect();
            if matched.is_empty() {
                return Err(format!(
                    "No plugin with name '{filter}'. Available: {}",
                    config
                        .plugin
                        .iter()
                        .map(|p| format!("\"{}\" (-p {})", p.name, p.suffix))
                        .collect::<Vec<_>>()
                        .join(", ")
                )
                .into());
            }
            matched
        } else {
            config.plugin.iter().collect()
        };

        for p in &plugins {
            if clap {
                let path = home.join(format!("Library/Audio/Plug-Ins/CLAP/{}.clap", p.name));
                if path.exists() {
                    targets.push(RemoveTarget { format: "CLAP", path, needs_sudo: false });
                }
            }
            if vst3 {
                let path = PathBuf::from(format!("/Library/Audio/Plug-Ins/VST3/{}.vst3", p.name));
                if path.exists() {
                    targets.push(RemoveTarget { format: "VST3", path, needs_sudo: true });
                }
            }
            if vst2 {
                let path = home.join(format!("Library/Audio/Plug-Ins/VST/{}.vst", p.name));
                if path.exists() {
                    targets.push(RemoveTarget { format: "VST2", path, needs_sudo: false });
                }
            }
            if au2 {
                let path = PathBuf::from(format!("/Library/Audio/Plug-Ins/Components/{}.component", p.name));
                if path.exists() {
                    targets.push(RemoveTarget { format: "AU v2", path, needs_sudo: true });
                }
            }
            if au3 {
                let path = PathBuf::from(format!("/Applications/{} v3.app", p.name));
                if path.exists() {
                    targets.push(RemoveTarget { format: "AU v3", path, needs_sudo: true });
                }
            }
            if aax {
                let path = PathBuf::from(format!(
                    "/Library/Application Support/Avid/Audio/Plug-Ins/{}.aaxplugin", p.name
                ));
                if path.exists() {
                    targets.push(RemoveTarget { format: "AAX", path, needs_sudo: true });
                }
            }
        }
    }

    if targets.is_empty() {
        eprintln!("No installed plugins found to remove.");
        return Ok(());
    }

    // Show summary
    eprintln!("The following plugins will be removed:\n");
    for t in &targets {
        eprintln!("  {:<5} {}", t.format, t.path.display());
    }
    eprintln!();

    if dry_run {
        eprintln!("Dry run — nothing was removed.");
        return Ok(());
    }

    if !yes && !confirm_prompt(&format!("Remove {} bundle(s)?", targets.len())) {
        eprintln!("Cancelled.");
        return Ok(());
    }

    // Remove bundles
    let mut removed_au = false;
    let mut errors = 0u32;

    for t in &targets {
        // AU v3 special handling: unregister before deleting
        if t.format == "AU v3" {
            // Try to find a matching plugin def for precise unregistration
            let matched_plugin = config.plugin.iter().find(|p| {
                t.path == Path::new(&format!("/Applications/{} v3.app", p.name))
            });
            if let Some(p) = matched_plugin {
                unregister_au3(&config, p, &t.path);
            } else {
                // Stale AU v3 — unregister by path only (lsregister)
                let _ = Command::new(
                    "/System/Library/Frameworks/CoreServices.framework/\
                     Frameworks/LaunchServices.framework/Support/lsregister",
                ).args(["-u", t.path.to_str().unwrap_or("")]).output();
            }
            removed_au = true;
        }
        if t.format == "AU v2" {
            removed_au = true;
        }

        let result = if t.needs_sudo {
            run_sudo("rm", &["-rf", t.path.to_str().unwrap()])
        } else {
            fs::remove_dir_all(&t.path)
                .or_else(|_| fs::remove_file(&t.path))
                .map_err(|e| e.into())
        };

        let name = t.path.file_name().unwrap_or_default().to_string_lossy();
        match result {
            Ok(()) => eprintln!("  \u{2713} {:<5} {}", t.format, name),
            Err(e) => {
                eprintln!("  \u{2717} {:<5} {} ({})", t.format, name, e);
                errors += 1;
            }
        }
    }

    // Clear AU caches if any AU bundles were removed
    if removed_au {
        clear_au_caches();
        eprintln!("\nCleared AU caches.");
    }

    if errors > 0 {
        eprintln!("\n{errors} error(s). Check permissions or run with sudo.");
    } else {
        eprintln!("\nDone. Restart your DAW to rescan.");
    }
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
                    p.name, p.resolved_au_type(), p.resolved_fourcc(), config.vendor.au_manufacturer
                );
                let output = Command::new("auval")
                    .args(["-v", &p.resolved_au_type(), p.resolved_fourcc(), &config.vendor.au_manufacturer])
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
                    p.name, p.resolved_au_type(), sub, config.vendor.au_manufacturer
                );
                let output = Command::new("auval")
                    .args(["-v", &p.resolved_au_type(), sub, &config.vendor.au_manufacturer])
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
// package — build, sign, and create .pkg installers
// ---------------------------------------------------------------------------

/// Parsed format flags for the package command.
#[derive(Clone, PartialEq)]
pub(crate) enum PkgFormat {
    Clap,
    Vst3,
    Vst2,
    Au2,
    Au3,
    Aax,
}

impl PkgFormat {
    pub(crate) fn parse_list(s: &str) -> Result<Vec<PkgFormat>, BoxErr> {
        let mut out = Vec::new();
        for token in s.split(',') {
            match token.trim() {
                "clap" => out.push(PkgFormat::Clap),
                "vst3" => out.push(PkgFormat::Vst3),
                "vst2" => out.push(PkgFormat::Vst2),
                "au2" => out.push(PkgFormat::Au2),
                "au3" => out.push(PkgFormat::Au3),
                "aax" => out.push(PkgFormat::Aax),
                other => return Err(format!("unknown format: {other}").into()),
            }
        }
        Ok(out)
    }

    pub(crate) fn label(&self) -> &'static str {
        match self {
            PkgFormat::Clap => "CLAP",
            PkgFormat::Vst3 => "VST3",
            PkgFormat::Vst2 => "VST2",
            PkgFormat::Au2 => "AU2",
            PkgFormat::Au3 => "AU3",
            PkgFormat::Aax => "AAX",
        }
    }

    fn extension(&self) -> &'static str {
        match self {
            PkgFormat::Clap => "clap",
            PkgFormat::Vst3 => "vst3",
            PkgFormat::Vst2 => "vst",
            PkgFormat::Au2 => "component",
            PkgFormat::Au3 => "app",
            PkgFormat::Aax => "aaxplugin",
        }
    }

    fn install_location(&self) -> &'static str {
        match self {
            PkgFormat::Clap => "/Library/Audio/Plug-Ins/CLAP/",
            PkgFormat::Vst3 => "/Library/Audio/Plug-Ins/VST3/",
            PkgFormat::Vst2 => "/Library/Audio/Plug-Ins/VST/",
            PkgFormat::Au2 => "/Library/Audio/Plug-Ins/Components/",
            PkgFormat::Au3 => "/Applications/",
            PkgFormat::Aax => "/Library/Application Support/Avid/Audio/Plug-Ins/",
        }
    }

    fn pkg_id_suffix(&self) -> &'static str {
        match self {
            PkgFormat::Clap => "clap",
            PkgFormat::Vst3 => "vst3",
            PkgFormat::Vst2 => "vst2",
            PkgFormat::Au2 => "au2",
            PkgFormat::Au3 => "au3",
            PkgFormat::Aax => "aax",
        }
    }

    /// Whether pkgbuild recognizes this as a native macOS bundle type.
    /// If false, we use --root instead of --component.
    fn is_native_bundle(&self) -> bool {
        matches!(self, PkgFormat::Vst3 | PkgFormat::Au2 | PkgFormat::Au3)
    }

    /// Bundle directory name for a given plugin.
    fn bundle_name(&self, plugin_name: &str) -> String {
        match self {
            PkgFormat::Au3 => format!("{} v3.app", plugin_name),
            _ => format!("{}.{}", plugin_name, self.extension()),
        }
    }

    fn choice_description(&self) -> &'static str {
        match self {
            PkgFormat::Clap => "For Reaper, Bitwig",
            PkgFormat::Vst3 => "For Ableton, FL Studio, Reaper, Cubase",
            PkgFormat::Vst2 => "Legacy — for hosts without VST3 support",
            PkgFormat::Au2 => "For Logic Pro, GarageBand, Ableton",
            PkgFormat::Au3 => "Audio Unit v3 (appex)",
            PkgFormat::Aax => "For Pro Tools",
        }
    }
}

/// Stage a CLAP bundle into the staging directory.
fn stage_clap(root: &Path, p: &PluginDef, staging: &Path, identity: &str) -> Res {
    let dylib = root.join(format!("target/release/lib{}.dylib", p.dylib_stem()));
    if !dylib.exists() {
        return Err(format!("Missing: {}", dylib.display()).into());
    }
    let dst = staging.join(format!("{}.clap", p.name));
    fs::copy(&dylib, &dst)?;
    codesign_bundle(dst.to_str().unwrap(), identity, false)?;
    Ok(())
}

/// Stage a VST3 bundle into the staging directory.
fn stage_vst3(root: &Path, p: &PluginDef, config: &Config, staging: &Path) -> Res {
    let dylib = root.join(format!("target/release/lib{}.dylib", p.dylib_stem()));
    if !dylib.exists() {
        return Err(format!("Missing: {}", dylib.display()).into());
    }
    let bundle = staging.join(format!("{}.vst3", p.name));
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
    fs::write(bundle.join("Contents/Info.plist"), &plist)?;
    codesign_bundle(bundle.to_str().unwrap(), config.macos.application_identity(), false)?;
    Ok(())
}

/// Stage a VST2 bundle into the staging directory.
fn stage_vst2(root: &Path, p: &PluginDef, config: &Config, staging: &Path) -> Res {
    let dylib = root.join(format!("target/release/lib{}_vst2.dylib", p.dylib_stem()));
    if !dylib.exists() {
        return Err(format!("Missing: {}", dylib.display()).into());
    }
    let bundle = staging.join(format!("{}.vst", p.name));
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
    codesign_bundle(bundle.to_str().unwrap(), config.macos.application_identity(), false)?;
    Ok(())
}

/// Stage an AU v2 bundle into the staging directory.
fn stage_au2(root: &Path, p: &PluginDef, config: &Config, staging: &Path) -> Res {
    let dylib = root.join(format!("target/release/lib{}_au.dylib", p.dylib_stem()));
    if !dylib.exists() {
        return Err(format!("Missing: {}", dylib.display()).into());
    }
    let bundle = staging.join(format!("{}.component", p.name));
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
        au_type = p.resolved_au_type(),
        au_subtype = p.resolved_fourcc(),
        au_mfr = config.vendor.au_manufacturer,
        au_tag = p.au_tag,
    );
    fs::write(bundle.join("Contents/Info.plist"), &plist)?;
    codesign_bundle(bundle.to_str().unwrap(), config.macos.application_identity(), false)?;
    Ok(())
}

/// Stage an AAX bundle into the staging directory.
///
/// `universal_mac` controls whether the AAX C++ template (the wrapper binary
/// Pro Tools launches) is built fat — the Rust cdylib in Resources/ is
/// already lipo'd universal when the caller passes `universal_mac = true`.
fn stage_aax(
    root: &Path,
    p: &PluginDef,
    config: &Config,
    staging: &Path,
    universal_mac: bool,
) -> Res {
    let template = tmp_dir().join("aax_template/build/TruceAAXTemplate.aaxplugin/Contents/MacOS/TruceAAXTemplate");
    if !template.exists() {
        if let Some(sdk_path) = resolve_aax_sdk_path(config) {
            eprintln!("AAX: building template with SDK at {}", sdk_path.display());
            build_aax_template(root, &sdk_path, universal_mac)?;
        } else {
            return Err("AAX SDK not configured. Set [macos].aax_sdk_path in truce.toml or AAX_SDK_PATH env var.".into());
        }
    }
    if !template.exists() {
        return Err("AAX template build succeeded but binary not found".into());
    }

    let dylib = root.join(format!("target/release/lib{}_aax.dylib", p.dylib_stem()));
    if !dylib.exists() {
        return Err(format!("Missing: {}", dylib.display()).into());
    }

    let bundle = staging.join(format!("{}.aaxplugin", p.name));
    let contents = bundle.join("Contents");
    fs::create_dir_all(contents.join("MacOS"))?;
    fs::create_dir_all(contents.join("Resources"))?;
    fs::copy(&template, contents.join("MacOS").join(&p.name))?;
    fs::copy(&dylib, contents.join("Resources").join(format!("lib{}_aax.dylib", p.dylib_stem())))?;

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
    fs::write(contents.join("Info.plist"), &plist)?;

    // Sign inside-out: inner dylib first, then the outer bundle.
    // notarization rejects bundles where nested binaries lack hardened
    // runtime + timestamp.
    let inner_dylib = contents.join("Resources").join(format!("lib{}_aax.dylib", p.dylib_stem()));
    codesign_bundle(inner_dylib.to_str().unwrap(), config.macos.application_identity(), false)?;
    let inner_exe = contents.join("MacOS").join(&p.name);
    codesign_bundle(inner_exe.to_str().unwrap(), config.macos.application_identity(), false)?;
    codesign_bundle(bundle.to_str().unwrap(), config.macos.application_identity(), false)?;
    Ok(())
}

/// Stage an AU v3 .app bundle into the staging directory.
/// Copies from the xcodebuild output in target/tmp/.
fn stage_au3(_root: &Path, p: &PluginDef, config: &Config, staging: &Path) -> Res {
    let app_name = format!("{} v3.app", p.name);
    let build_dir = tmp_dir().join(format!("au_v3_build_{}", p.suffix));
    let built_app = build_dir.join("build/Release/TruceAUv3.app");
    if !built_app.exists() {
        return Err(format!("AU v3 app not built: {}. Run the build step first.", built_app.display()).into());
    }

    let dst = staging.join(&app_name);
    // May be root-owned from a previous install-based run
    if dst.exists() {
        if fs::remove_dir_all(&dst).is_err() {
            let _ = Command::new("rm").args(["-rf", dst.to_str().unwrap()]).status();
        }
    }
    copy_dir_recursive(&built_app, &dst)?;

    // Copy framework into app
    let fw_name = p.fw_name();
    let fw_build = tmp_dir().join(format!("au_v3_fw_{}", p.suffix));
    let fw_src = fw_build.join(format!("{fw_name}.framework"));
    if fw_src.exists() {
        let fw_dst = dst.join("Contents/Frameworks");
        fs::create_dir_all(&fw_dst)?;
        copy_dir_recursive(&fw_src, &fw_dst.join(format!("{fw_name}.framework")))?;
    }

    codesign_bundle(dst.to_str().unwrap(), config.macos.application_identity(), false)?;
    Ok(())
}

/// Generate the distribution.xml for the macOS .pkg installer.
fn generate_distribution_xml(
    plugin_name: &str,
    vendor_id: &str,
    suffix: &str,
    formats: &[PkgFormat],
    version: &str,
    resources: Option<&PackagingConfig>,
) -> String {
    let mut choices_outline = String::new();
    let mut choices = String::new();
    let mut pkg_refs = String::new();

    for fmt in formats {
        let id = fmt.pkg_id_suffix();
        let pkg_id = format!("{vendor_id}.{suffix}.{id}");
        let label = fmt.label();
        let desc = fmt.choice_description();
        let component_file = format!("{plugin_name}-{label}.pkg");

        // AAX unchecked by default (may need PACE signing for distribution)
        let enabled_attr = if *fmt == PkgFormat::Aax {
            "\n            selected=\"false\""
        } else {
            ""
        };

        choices_outline.push_str(&format!("        <line choice=\"{id}\"/>\n"));
        choices.push_str(&format!(
            r#"
    <choice id="{id}" title="{label}" description="{desc}"{enabled_attr}>
        <pkg-ref id="{pkg_id}"/>
    </choice>
"#
        ));
        pkg_refs.push_str(&format!(
            "    <pkg-ref id=\"{pkg_id}\" version=\"{version}\"\
             >{component_file}</pkg-ref>\n"
        ));
    }

    let welcome = resources
        .and_then(|r| r.welcome_html.as_deref())
        .map(|_| "    <welcome file=\"welcome.html\"/>\n")
        .unwrap_or("");
    let license = resources
        .and_then(|r| r.license_html.as_deref())
        .map(|_| "    <license file=\"license.html\"/>\n")
        .unwrap_or("");

    format!(
        r#"<?xml version="1.0" encoding="utf-8"?>
<installer-gui-script minSpecVersion="2">
    <title>{plugin_name}</title>
{welcome}{license}
    <options customize="always" require-scripts="false"/>

    <choices-outline>
{choices_outline}    </choices-outline>
{choices}
{pkg_refs}</installer-gui-script>
"#
    )
}

/// Write AU cache clearing post-install script for AU component packages.
fn write_postinstall_script(dir: &Path) -> Res {
    let scripts_dir = dir.join("scripts");
    fs::create_dir_all(&scripts_dir)?;
    let script = scripts_dir.join("postinstall");
    fs::write(
        &script,
        "#!/bin/bash\n\
         killall -9 AudioComponentRegistrar 2>/dev/null || true\n\
         rm -rf ~/Library/Caches/AudioUnitCache/ 2>/dev/null || true\n\
         rm -f ~/Library/Preferences/com.apple.audio.InfoHelper.plist 2>/dev/null || true\n\
         exit 0\n",
    )?;
    // Make executable
    Command::new("chmod").args(["+x", script.to_str().unwrap()]).status()?;
    Ok(())
}

fn cmd_package(args: &[String]) -> Res {
    #[cfg(target_os = "windows")]
    {
        return packaging_windows::cmd_package(args);
    }
    #[cfg(not(target_os = "windows"))]
    cmd_package_macos(args)
}

/// macOS CPU architecture we can build for. Packaging defaults to
/// `[X86_64, Arm64]` (a universal Mach-O via `lipo`); `--host-only` opts out
/// for faster dev iteration.
///
/// Defined unconditionally (not cfg-gated) so cross-platform codepaths such
/// as `build_and_install_au_v3` can reference it without a cfg matrix — the
/// runtime on Windows never reaches the `lipo`/xcodebuild machinery that
/// would actually care about which slice is which.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MacArch {
    X86_64,
    Arm64,
}

impl MacArch {
    fn triple(self) -> &'static str {
        match self {
            MacArch::X86_64 => "x86_64-apple-darwin",
            MacArch::Arm64 => "aarch64-apple-darwin",
        }
    }

    fn host() -> Self {
        if cfg!(target_arch = "aarch64") {
            MacArch::Arm64
        } else {
            MacArch::X86_64
        }
    }
}

/// Combine per-arch dylibs into a single (fat) Mach-O at `output`.
///
/// Single-arch inputs are copied through; the output path matches the legacy
/// non-universal layout (`target/release/...`) so the per-format stage
/// functions don't need to know whether the build was universal.
fn lipo_into(inputs: &[PathBuf], output: &Path) -> Res {
    if inputs.is_empty() {
        return Err("lipo_into: no inputs".into());
    }
    if let Some(parent) = output.parent() {
        fs::create_dir_all(parent)?;
    }
    if inputs.len() == 1 {
        // No fattening needed — just copy to the canonical location so
        // downstream stage code reads from the same path in both modes.
        fs::copy(&inputs[0], output)?;
        return Ok(());
    }
    let mut cmd = Command::new("lipo");
    cmd.arg("-create");
    for i in inputs {
        cmd.arg(i);
    }
    cmd.arg("-output").arg(output);
    let status = cmd.status()?;
    if !status.success() {
        return Err(format!(
            "lipo -create failed combining {} slices into {}",
            inputs.len(),
            output.display()
        )
        .into());
    }
    Ok(())
}

/// Run a cargo release build for a specific Apple arch. Adds
/// `--target <triple>` to the caller's args so output lands under
/// `target/{triple}/release/` without colliding with other arches.
fn cargo_build_for_arch(
    env_vars: &[(&str, &str)],
    base_args: &[&str],
    arch: MacArch,
    dt: &str,
) -> Res {
    let mut args: Vec<String> = vec!["--target".into(), arch.triple().into()];
    for a in base_args {
        args.push((*a).into());
    }
    let arg_refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
    cargo_build(env_vars, &arg_refs, dt)
}

#[cfg(not(target_os = "windows"))]
fn cmd_package_macos(args: &[String]) -> Res {
    let config = load_config()?;
    let root = project_root();
    let dt = &deployment_target();

    let mut plugin_filter: Option<String> = None;
    let mut format_str: Option<String> = None;
    let mut no_notarize = false;
    let mut host_only = false;

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "-p" => {
                i += 1;
                plugin_filter = Some(args.get(i).cloned().ok_or("-p requires a plugin suffix")?);
            }
            "--formats" => {
                i += 1;
                format_str = Some(args.get(i).cloned().ok_or("--formats requires a value")?);
            }
            "--no-notarize" => no_notarize = true,
            // Universal is the default on macOS — accept the flag explicitly
            // as a no-op so cross-platform CI scripts (that also hit Windows)
            // keep working.
            "--universal" => {}
            "--host-only" => host_only = true,
            // --no-sign / --no-installer are Windows-only flags; accept and
            // ignore so cross-platform CI scripts don't break.
            "--no-sign" | "--no-installer" => {}
            other => return Err(format!("unknown flag: {other}").into()),
        }
        i += 1;
    }

    // Universal by default: produce a fat Mach-O covering both Apple arches.
    // `--host-only` falls back to the host-only build for faster dev iteration.
    let archs: Vec<MacArch> = if host_only {
        vec![MacArch::host()]
    } else {
        vec![MacArch::X86_64, MacArch::Arm64]
    };
    let universal = archs.len() > 1;

    // Resolve formats
    let formats: Vec<PkgFormat> = if let Some(ref s) = format_str {
        PkgFormat::parse_list(s)?
    } else if !config.packaging.formats.is_empty() {
        PkgFormat::parse_list(&config.packaging.formats.join(","))?
    } else {
        // Default: auto-detect from project features
        let available = detect_default_features();
        let mut fmts = Vec::new();
        if available.contains("clap") { fmts.push(PkgFormat::Clap); }
        if available.contains("vst3") { fmts.push(PkgFormat::Vst3); }
        if available.contains("vst2") { fmts.push(PkgFormat::Vst2); }
        if available.contains("au") {
            fmts.push(PkgFormat::Au2);
            fmts.push(PkgFormat::Au3);
        }
        if available.contains("aax") { fmts.push(PkgFormat::Aax); }
        fmts
    };

    if formats.is_empty() {
        return Err("no formats to package".into());
    }

    // Resolve plugins
    let plugins: Vec<&PluginDef> = if let Some(ref filter) = plugin_filter {
        let matched: Vec<_> = config.plugin.iter().filter(|p| p.suffix == *filter).collect();
        if matched.is_empty() {
            return Err(format!(
                "No plugin with suffix '{filter}'. Available: {}",
                config.plugin.iter().map(|p| p.suffix.as_str()).collect::<Vec<_>>().join(", ")
            ).into());
        }
        matched
    } else {
        config.plugin.iter().collect()
    };

    let has_clap = formats.contains(&PkgFormat::Clap);
    let has_vst3 = formats.contains(&PkgFormat::Vst3);
    let has_vst2 = formats.contains(&PkgFormat::Vst2);
    let has_au2 = formats.contains(&PkgFormat::Au2);
    let has_au3 = formats.contains(&PkgFormat::Au3);
    let has_aax = formats.contains(&PkgFormat::Aax);

    // ---------------------------------------------------------------
    // Step 1: Build all requested formats (release mode).
    //
    // Per format, build once per arch (adding `--target <triple>`) then
    // `lipo -create` the per-arch outputs into the canonical
    // `target/release/lib{stem}_{fmt}.dylib` location. The stage functions
    // below read from that path and don't need to know whether the build
    // was universal.
    // ---------------------------------------------------------------

    eprintln!(
        "Packaging archs: {}",
        archs.iter().map(|a| a.triple()).collect::<Vec<_>>().join(", ")
    );

    if has_clap || has_vst3 {
        for &arch in &archs {
            eprintln!("Building CLAP + VST3 ({})...", arch.triple());
            let mut base: Vec<&str> = Vec::new();
            for p in &plugins {
                base.push("-p");
                base.push(&p.crate_name);
            }
            cargo_build_for_arch(&[], &base, arch, dt)?;
            // Save a copy since subsequent-format builds reuse the same target dir
            for p in &plugins {
                let src = release_lib_for_target(
                    &root,
                    &p.dylib_stem(),
                    Some(arch.triple()),
                );
                let saved = release_lib_for_target(
                    &root,
                    &format!("{}_plugin", p.dylib_stem()),
                    Some(arch.triple()),
                );
                if src.exists() { fs::copy(&src, &saved)?; }
            }
        }
        // Lipo per-plugin into the canonical path.
        for p in &plugins {
            let inputs: Vec<PathBuf> = archs
                .iter()
                .map(|a| {
                    release_lib_for_target(
                        &root,
                        &format!("{}_plugin", p.dylib_stem()),
                        Some(a.triple()),
                    )
                })
                .collect();
            let output = root.join(format!(
                "target/release/lib{}_plugin.dylib",
                p.dylib_stem()
            ));
            lipo_into(&inputs, &output)?;
        }
    }

    if has_vst2 {
        for &arch in &archs {
            eprintln!("Building VST2 ({})...", arch.triple());
            let mut base: Vec<&str> = Vec::new();
            for p in &plugins {
                base.push("-p");
                base.push(&p.crate_name);
            }
            base.extend_from_slice(&["--no-default-features", "--features", "vst2"]);
            cargo_build_for_arch(&[], &base, arch, dt)?;
            for p in &plugins {
                let src = release_lib_for_target(
                    &root,
                    &p.dylib_stem(),
                    Some(arch.triple()),
                );
                let saved = release_lib_for_target(
                    &root,
                    &format!("{}_vst2", p.dylib_stem()),
                    Some(arch.triple()),
                );
                if src.exists() { fs::copy(&src, &saved)?; }
            }
        }
        for p in &plugins {
            let inputs: Vec<PathBuf> = archs
                .iter()
                .map(|a| {
                    release_lib_for_target(
                        &root,
                        &format!("{}_vst2", p.dylib_stem()),
                        Some(a.triple()),
                    )
                })
                .collect();
            let output = root.join(format!(
                "target/release/lib{}_vst2.dylib",
                p.dylib_stem()
            ));
            lipo_into(&inputs, &output)?;
        }
    }

    if has_au2 {
        // AU v2 is built per-plugin (distinct TRUCE_AU_PLUGIN_ID env var),
        // so the outer loop is plugins × archs rather than archs × plugins.
        for p in &plugins {
            for &arch in &archs {
                eprintln!("Building AU v2 ({}, {})...", p.name, arch.triple());
                cargo_build_for_arch(
                    &[("TRUCE_AU_VERSION", "2"), ("TRUCE_AU_PLUGIN_ID", &p.suffix)],
                    &["-p", &p.crate_name, "--no-default-features", "--features", "au"],
                    arch,
                    dt,
                )?;
                let src = release_lib_for_target(
                    &root,
                    &p.dylib_stem(),
                    Some(arch.triple()),
                );
                let saved = release_lib_for_target(
                    &root,
                    &format!("{}_au", p.dylib_stem()),
                    Some(arch.triple()),
                );
                fs::copy(&src, &saved)?;
            }
            let inputs: Vec<PathBuf> = archs
                .iter()
                .map(|a| {
                    release_lib_for_target(
                        &root,
                        &format!("{}_au", p.dylib_stem()),
                        Some(a.triple()),
                    )
                })
                .collect();
            let output = root.join(format!(
                "target/release/lib{}_au.dylib",
                p.dylib_stem()
            ));
            lipo_into(&inputs, &output)?;
        }
    }

    if has_aax {
        for &arch in &archs {
            eprintln!("Building AAX ({})...", arch.triple());
            let mut base: Vec<&str> = Vec::new();
            for p in &plugins {
                base.push("-p");
                base.push(&p.crate_name);
            }
            base.extend_from_slice(&["--no-default-features", "--features", "aax"]);
            cargo_build_for_arch(&[], &base, arch, dt)?;
            for p in &plugins {
                let src = release_lib_for_target(
                    &root,
                    &p.dylib_stem(),
                    Some(arch.triple()),
                );
                let saved = release_lib_for_target(
                    &root,
                    &format!("{}_aax", p.dylib_stem()),
                    Some(arch.triple()),
                );
                if src.exists() { fs::copy(&src, &saved)?; }
            }
        }
        for p in &plugins {
            let inputs: Vec<PathBuf> = archs
                .iter()
                .map(|a| {
                    release_lib_for_target(
                        &root,
                        &format!("{}_aax", p.dylib_stem()),
                        Some(a.triple()),
                    )
                })
                .collect();
            let output = root.join(format!(
                "target/release/lib{}_aax.dylib",
                p.dylib_stem()
            ));
            lipo_into(&inputs, &output)?;
        }
    }

    if has_au3 {
        // AU v3: build per-arch Rust framework, lipo, then xcodebuild.
        build_au_v3(&root, &config, &plugins, false, &archs)?;
    }

    // Restore the canonical `target/release/lib{stem}.dylib` from the CLAP/VST3
    // save so other consumers (e.g. subsequent `cargo truce install` runs on
    // the same tree) see a sensible host-arch single-slice build. Only fill
    // this with the host arch's slice — there's no host that would load both
    // arches from the bare `target/release/` path.
    if has_clap || has_vst3 {
        let host = MacArch::host();
        for p in &plugins {
            let saved = release_lib_for_target(
                &root,
                &format!("{}_plugin", p.dylib_stem()),
                Some(host.triple()),
            );
            let dst = root.join(format!("target/release/lib{}.dylib", p.dylib_stem()));
            if saved.exists() { fs::copy(&saved, &dst)?; }
        }
    }

    // ---------------------------------------------------------------
    // Step 2–7: Stage, sign, build .pkg per plugin
    // ---------------------------------------------------------------

    let dist_dir = root.join("dist");
    fs::create_dir_all(&dist_dir)?;

    let version = read_workspace_version(&root).unwrap_or_else(|| "0.0.0".to_string());

    for p in &plugins {
        eprintln!("\n=== Packaging: {} ===", p.name);

        let staging = root.join("target/package").join(&p.suffix);
        let _ = fs::remove_dir_all(&staging);
        fs::create_dir_all(&staging)?;

        // Step 2: Stage signed bundles
        for fmt in &formats {
            eprint!("  Staging {}... ", fmt.label());
            let result = match fmt {
                PkgFormat::Clap => stage_clap(&root, p, &staging, config.macos.application_identity()),
                PkgFormat::Vst3 => stage_vst3(&root, p, &config, &staging),
                PkgFormat::Vst2 => stage_vst2(&root, p, &config, &staging),
                PkgFormat::Au2 => stage_au2(&root, p, &config, &staging),
                PkgFormat::Au3 => stage_au3(&root, p, &config, &staging),
                PkgFormat::Aax => stage_aax(&root, p, &config, &staging, universal),
            };
            match result {
                Ok(()) => eprintln!("ok"),
                Err(e) => {
                    eprintln!("FAILED: {e}");
                    return Err(e);
                }
            }
        }

        // Step 3: Build component .pkg per format
        let components_dir = staging.join("components");
        fs::create_dir_all(&components_dir)?;

        // Prepare AU postinstall script
        let scripts_dir = staging.join("au_scripts");
        if has_au2 {
            write_postinstall_script(&scripts_dir)?;
        }

        for fmt in &formats {
            let bundle_name = fmt.bundle_name(&p.name);
            let component_path = staging.join(&bundle_name);
            let pkg_id = format!("{}.{}.{}", config.vendor.id, p.suffix, fmt.pkg_id_suffix());
            let component_pkg = components_dir.join(format!("{}-{}.pkg", p.name, fmt.label()));

            let mut pkgbuild_args = if fmt.is_native_bundle() {
                // VST3, AU2: recognized macOS bundle types
                vec![
                    "--component".to_string(),
                    component_path.to_str().unwrap().to_string(),
                    "--install-location".to_string(),
                    fmt.install_location().to_string(),
                ]
            } else {
                // CLAP, VST2, AAX: not recognized by pkgbuild --component.
                // Use --root with a temp directory containing just this bundle,
                // and set --install-location to the parent directory.
                let root_dir = staging.join(format!("_pkgroot_{}", fmt.label()));
                let _ = fs::remove_dir_all(&root_dir);
                fs::create_dir_all(&root_dir)?;
                let dst = root_dir.join(&bundle_name);
                if component_path.is_dir() {
                    copy_dir_recursive(&component_path, &dst)?;
                } else {
                    fs::copy(&component_path, &dst)?;
                }
                vec![
                    "--root".to_string(),
                    root_dir.to_str().unwrap().to_string(),
                    "--install-location".to_string(),
                    fmt.install_location().to_string(),
                ]
            };

            pkgbuild_args.extend_from_slice(&[
                "--identifier".to_string(),
                pkg_id,
                "--version".to_string(),
                version.to_string(),
            ]);

            // AU2 gets a postinstall script to clear caches
            if *fmt == PkgFormat::Au2 {
                pkgbuild_args.push("--scripts".to_string());
                pkgbuild_args.push(scripts_dir.to_str().unwrap().to_string());
            }

            pkgbuild_args.push(component_pkg.to_str().unwrap().to_string());

            let pkgbuild_refs: Vec<&str> = pkgbuild_args.iter().map(|s| s.as_str()).collect();
            eprintln!("  pkgbuild {}...", fmt.label());
            let status = Command::new("pkgbuild").args(&pkgbuild_refs).status()?;
            if !status.success() {
                return Err(format!("pkgbuild failed for {} {}", p.name, fmt.label()).into());
            }
        }

        // Step 4: Generate distribution.xml
        let dist_xml = generate_distribution_xml(
            &p.name,
            &config.vendor.id,
            &p.suffix,
            &formats,
            &version,
            Some(&config.packaging),
        );
        let dist_xml_path = staging.join("distribution.xml");
        fs::write(&dist_xml_path, &dist_xml)?;

        // Step 5: Prepare resources (optional welcome/license html)
        let resources_dir = staging.join("resources");
        fs::create_dir_all(&resources_dir)?;
        if let Some(ref html) = config.packaging.welcome_html {
            let src = root.join(html);
            if src.exists() {
                fs::copy(&src, resources_dir.join("welcome.html"))?;
            }
        }
        if let Some(ref html) = config.packaging.license_html {
            let src = root.join(html);
            if src.exists() {
                fs::copy(&src, resources_dir.join("license.html"))?;
            }
        }

        // Step 6: productbuild → signed .pkg
        let pkg_name = format!("{}-{}-macos.pkg", p.name, version);
        let pkg_path = dist_dir.join(&pkg_name);

        let mut pb_args = vec![
            "--distribution",
            dist_xml_path.to_str().unwrap(),
            "--package-path",
            components_dir.to_str().unwrap(),
            "--resources",
            resources_dir.to_str().unwrap(),
        ];

        let installer_id = config.macos.installer_identity();
        if let Some(id) = installer_id {
            pb_args.push("--sign");
            pb_args.push(id);
        }

        pb_args.push(pkg_path.to_str().unwrap());

        eprintln!("  productbuild...");
        let status = Command::new("productbuild").args(&pb_args).status()?;
        if !status.success() {
            return Err(format!("productbuild failed for {}", p.name).into());
        }

        // Step 7: Notarize + staple
        if config.macos.packaging.notarize && !no_notarize {
            notarize_and_staple(&pkg_path, &config)?;
        } else if !config.macos.packaging.notarize {
            eprintln!("  Skipped notarization (set notarize = true in [macos.packaging])");
        } else {
            eprintln!("  Skipped notarization (--no-notarize)");
        }

        eprintln!("  Package ready: {}", pkg_path.display());
    }

    eprintln!("\nDone. Installers in {}", dist_dir.display());
    Ok(())
}

/// Notarize a .pkg and staple the ticket. (Phase 3)
fn notarize_and_staple(pkg_path: &Path, config: &Config) -> Res {
    let pkg = pkg_path.to_str().unwrap();

    // Determine credential source: env vars or truce.toml
    let apple_id_env = std::env::var("APPLE_ID").unwrap_or_default();
    let team_id_env = std::env::var("TEAM_ID").unwrap_or_default();
    let apple_id = config.macos.packaging.apple_id.as_deref().unwrap_or(&apple_id_env);
    let team_id = config.macos.packaging.team_id.as_deref().unwrap_or(&team_id_env);

    // First try keychain profile, then fall back to explicit credentials
    let keychain_profile = std::env::var("TRUCE_NOTARY_PROFILE").unwrap_or_else(|_| "TRUCE_NOTARY".to_string());

    eprintln!("  Notarizing {}...", pkg_path.file_name().unwrap().to_str().unwrap());

    // Submit and capture output to check status + extract submission ID
    let output = Command::new("xcrun")
        .args([
            "notarytool", "submit", pkg,
            "--keychain-profile", &keychain_profile,
            "--wait",
        ])
        .output();

    let (succeeded, output_text) = match output {
        Ok(o) => {
            let text = format!("{}{}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr));
            // notarytool returns 0 even on Invalid — check the status string
            let ok = o.status.success() && !text.contains("status: Invalid") && !text.contains("status: Rejected");
            (ok, text)
        }
        Err(_) => (false, String::new()),
    };

    if !succeeded {
        // Try explicit credentials as fallback
        if !apple_id.is_empty() && !team_id.is_empty() {
            eprintln!("  Keychain profile failed, trying explicit credentials...");
            let password = std::env::var("APP_SPECIFIC_PASSWORD")
                .map_err(|_| "notarization requires APP_SPECIFIC_PASSWORD env var or a keychain profile")?;
            let output = Command::new("xcrun")
                .args([
                    "notarytool", "submit", pkg,
                    "--apple-id", apple_id,
                    "--team-id", team_id,
                    "--password", &password,
                    "--wait",
                ])
                .output()?;
            let text = format!("{}{}", String::from_utf8_lossy(&output.stdout), String::from_utf8_lossy(&output.stderr));
            if !output.status.success() || text.contains("status: Invalid") || text.contains("status: Rejected") {
                // Extract submission ID and fetch the log
                fetch_notarization_log(&text, &keychain_profile);
                return Err("notarization failed (status: Invalid). See log above for details.".into());
            }
        } else {
            // Extract submission ID and fetch the log
            fetch_notarization_log(&output_text, &keychain_profile);
            if output_text.contains("status: Invalid") || output_text.contains("status: Rejected") {
                return Err("notarization failed (status: Invalid). See log above for details.".into());
            }
            return Err(
                "notarization failed. Set up credentials via:\n  \
                 xcrun notarytool store-credentials TRUCE_NOTARY\n  \
                 or set apple_id/team_id in [macos.packaging] + APP_SPECIFIC_PASSWORD env var"
                    .into(),
            );
        }
    }

    // Staple
    eprintln!("  Stapling...");
    let status = Command::new("xcrun")
        .args(["stapler", "staple", pkg])
        .status()?;
    if !status.success() {
        return Err("stapler staple failed".into());
    }

    eprintln!("  Notarized and stapled.");
    Ok(())
}

/// Extract submission ID from notarytool output and fetch the detailed log.
fn fetch_notarization_log(output: &str, keychain_profile: &str) {
    // Look for "id: <uuid>" in the output
    let id = output.lines()
        .find(|l| l.trim().starts_with("id:"))
        .and_then(|l| l.trim().strip_prefix("id:"))
        .map(|s| s.trim().to_string());

    if let Some(id) = id {
        eprintln!("  Fetching notarization log for {}...", id);
        let log_output = Command::new("xcrun")
            .args(["notarytool", "log", &id, "--keychain-profile", keychain_profile])
            .output();
        if let Ok(o) = log_output {
            let log = String::from_utf8_lossy(&o.stdout);
            if !log.is_empty() {
                eprintln!("\n--- Notarization Log ---");
                eprintln!("{log}");
                eprintln!("--- End Log ---\n");
            }
        }
    }
}

// ---------------------------------------------------------------------------
// build — produce bundles without installing
// ---------------------------------------------------------------------------

fn cmd_build(args: &[String]) -> Res {
    let config = load_config()?;
    let root = project_root();
    let dt = &deployment_target();

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
            codesign_bundle(bundle.to_str().unwrap(), config.macos.application_identity(), false)?;

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
    let dt = &deployment_target();

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

    // Platform tools
    #[cfg(target_os = "macos")]
    {
        eprintln!();
        eprintln!("  macOS");
        check_cmd("xcode-select", &["-p"], "Xcode CLI tools");
        check_cmd("xcodebuild", &["-version"], "xcodebuild (AU v3)");
        check_cmd("codesign", &["--help"], "codesign");

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

pub(crate) fn check_cmd(cmd: &str, args: &[&str], label: &str) {
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
