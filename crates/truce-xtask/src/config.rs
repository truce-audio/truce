//! Project configuration (read from `truce.toml`).
//!
//! All on-disk types and the resolution logic for signing identities
//! and SDK paths. Identity resolution falls back through env vars and
//! `.cargo/config.toml` so per-developer credentials stay out of the
//! tracked `truce.toml`.

use crate::{project_root, BoxErr};
use serde::Deserialize;
use std::fs;
use std::path::PathBuf;

#[derive(Deserialize)]
pub(crate) struct Config {
    #[serde(default)]
    pub(crate) macos: MacosConfig,
    #[serde(default)]
    pub(crate) windows: WindowsConfig,
    pub(crate) vendor: VendorConfig,
    pub(crate) plugin: Vec<PluginDef>,
    #[serde(default)]
    pub(crate) packaging: PackagingConfig,
}

// Windows-only config fields. Consumed by `packaging_windows.rs`, which
// is `#[cfg(target_os = "windows")] mod packaging_windows`, so on macOS
// and Linux the dead_code lint sees these as unused. The allow keeps
// the structs single-source-of-truth across platforms.
#[derive(Deserialize, Default)]
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
pub(crate) struct WindowsConfig {
    /// Path to the AAX SDK root directory. Falls back to the AAX_SDK_PATH env var.
    pub(crate) aax_sdk_path: Option<String>,
    #[serde(default)]
    pub(crate) signing: WindowsSigningConfig,
    #[serde(default)]
    pub(crate) packaging: WindowsPackagingConfig,
}

/// Authenticode signing credentials for signtool. First non-empty option wins,
/// in the order Azure → thumbprint → pfx file.
#[derive(Deserialize, Default)]
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
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

#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
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
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
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
pub(crate) struct MacosConfig {
    /// Path to the AAX SDK root directory. Falls back to the AAX_SDK_PATH env var.
    pub(crate) aax_sdk_path: Option<String>,
    #[serde(default)]
    pub(crate) signing: MacosSigningConfig,
    #[serde(default)]
    pub(crate) packaging: MacosPackagingConfig,
}

/// macOS code-signing identities. Parallels `[windows.signing]`: credentials
/// live here, installer appearance lives in `[macos.packaging]`.
#[derive(Deserialize, Default)]
pub(crate) struct MacosSigningConfig {
    /// `codesign -s` identity for bundles. Typically
    /// "Developer ID Application: Name (TEAMID)" or `-` for ad-hoc.
    /// Falls back to `TRUCE_SIGNING_IDENTITY` env var.
    pub(crate) application_identity: Option<String>,
    /// `productbuild --sign` identity for `.pkg` installers. Typically
    /// "Developer ID Installer: Name (TEAMID)".
    /// Falls back to `TRUCE_INSTALLER_SIGNING_IDENTITY` env var.
    pub(crate) installer_identity: Option<String>,
}

impl MacosConfig {
    /// Resolved application signing identity. `"-"` means ad-hoc / unsigned.
    /// Populated by `load_config` from `[macos.signing].application_identity`
    /// or the `TRUCE_SIGNING_IDENTITY` env var.
    pub(crate) fn application_identity(&self) -> &str {
        self.signing.application_identity.as_deref().unwrap_or("-")
    }

    /// Resolved installer signing identity. `None` means the installer won't
    /// be signed. Populated from `[macos.signing].installer_identity` or the
    /// `TRUCE_INSTALLER_SIGNING_IDENTITY` env var.
    pub(crate) fn installer_identity(&self) -> Option<&str> {
        self.signing.installer_identity.as_deref()
    }
}

#[derive(Deserialize, Default)]
pub(crate) struct MacosPackagingConfig {
    #[serde(default)]
    pub(crate) notarize: bool,
    pub(crate) apple_id: Option<String>,
    pub(crate) team_id: Option<String>,
}

#[derive(Deserialize, Default)]
pub(crate) struct PackagingConfig {
    #[serde(default)]
    pub(crate) formats: Vec<String>,
    pub(crate) welcome_html: Option<String>,
    pub(crate) license_html: Option<String>,
}

#[derive(Deserialize)]
pub(crate) struct VendorConfig {
    pub(crate) name: String,
    pub(crate) id: String,
    /// Vendor website URL. Used by the Windows Inno Setup installer's
    /// "Publisher URL" field; unused on macOS.
    #[serde(default)]
    #[cfg_attr(not(target_os = "windows"), allow(dead_code))]
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
    pub(crate) fourcc: Option<String>,
    pub(crate) category: String,
    #[serde(default)]
    pub(crate) au_type: Option<String>,
    #[serde(default)]
    pub(crate) au_subtype: Option<String>,
    #[serde(default)]
    pub(crate) au3_subtype: Option<String>,
    #[serde(default = "default_au_tag")]
    pub(crate) au_tag: String,
    // Per-format display-name overrides. When set, replace
    // `PluginInfo::name` in the host-facing spot of that format
    // (visible in plugin browsers, param-editor title, etc.).
    // Install-time only; the default (`None`) keeps `name`.
    #[serde(default)]
    pub(crate) clap_name: Option<String>,
    #[serde(default)]
    pub(crate) vst3_name: Option<String>,
    #[serde(default)]
    pub(crate) vst2_name: Option<String>,
    #[serde(default)]
    pub(crate) au_name: Option<String>,
    #[serde(default)]
    pub(crate) au3_name: Option<String>,
    #[serde(default)]
    pub(crate) aax_name: Option<String>,
    #[serde(default)]
    pub(crate) lv2_name: Option<String>,
}

impl PluginDef {
    pub(crate) fn resolved_fourcc(&self) -> &str {
        self.fourcc.as_deref()
            .or(self.au_subtype.as_deref())
            .expect("truce.toml: each [[plugin]] requires `fourcc` or `au_subtype`")
    }
    pub(crate) fn resolved_au_type(&self) -> &str {
        // Keep in sync with `truce-derive::plugin_info` +
        // `truce-build::emit_plugin_env`. NoteEffect → `aumi`
        // (Apple's MIDI Processor). `aumi` plugins declare no
        // audio buses per Apple spec — wrappers that can't express
        // that (AAX) synthesize dummy audio I/O internally.
        self.au_type.as_deref().unwrap_or(
            match self.category.as_str() {
                "instrument" => "aumu",
                "midi" | "note_effect" => "aumi",
                _ => "aufx",
            }
        )
    }
    pub(crate) fn au3_sub(&self) -> &str {
        self.au3_subtype.as_deref().unwrap_or(self.resolved_fourcc())
    }

    /// Name used for the AU v3 containing `.app` bundle directory.
    /// When `au3_name` is set in truce.toml it wins (both display
    /// name in host browsers and bundle path stay in sync). Otherwise
    /// we fall back to the historical `"{name} v3"` disambiguator so
    /// projects that haven't opted in are unaffected.
    pub(crate) fn au3_app_name(&self) -> String {
        match self.au3_name.as_deref() {
            Some(n) if !n.is_empty() => n.to_string(),
            _ => format!("{} v3", self.name),
        }
    }
    pub(crate) fn fw_name(&self) -> String {
        let cap = format!("{}{}", self.suffix[..1].to_uppercase(), &self.suffix[1..]);
        format!("Truce{}AU", cap)
    }
    /// Dylib filename stem derived from the crate name (hyphens → underscores).
    pub(crate) fn dylib_stem(&self) -> String {
        self.crate_name.replace('-', "_")
    }
}

fn default_au_tag() -> String {
    "Effects".to_string()
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
pub(crate) fn read_cargo_config_env(key: &str) -> Option<String> {
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
pub(crate) fn deployment_target() -> String {
    std::env::var("MACOSX_DEPLOYMENT_TARGET").unwrap_or_else(|_| "11.0".to_string())
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
