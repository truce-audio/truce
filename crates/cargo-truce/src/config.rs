//! Project configuration (read from `truce.toml`).
//!
//! All on-disk types and the resolution logic for signing identities
//! and SDK paths. Identity resolution falls back through env vars and
//! `.cargo/config.toml` so per-developer credentials stay out of the
//! tracked `truce.toml`.

use crate::{BoxErr, project_root};
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
    /// Packaging metadata (welcome HTML, license HTML, etc.). Consumed
    /// by `cmd_package_macos` only — Windows packaging uses
    /// `WindowsConfig::packaging`, Linux has no packaging path.
    #[serde(default)]
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    pub(crate) packaging: PackagingConfig,
    /// Suite installers — repeatable. Each entry produces one
    /// installer per platform that bundles the listed plugins.
    /// Empty = per-plugin output only (today's behaviour).
    #[serde(default, rename = "suite")]
    pub(crate) suites: Vec<SuiteDef>,
}

// Windows-only config fields. Consumed by `packaging_windows.rs`, which
// is `#[cfg(target_os = "windows")] mod packaging_windows`, so on macOS
// and Linux the dead_code lint sees these as unused. The allow keeps
// the structs single-source-of-truth across platforms.
#[derive(Deserialize, Default)]
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
pub(crate) struct WindowsConfig {
    /// Path to the AAX SDK root directory. Falls back to the `AAX_SDK_PATH` env var.
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
    /// Path to a .pfx file. Password via `TRUCE_PFX_PASSWORD` env var.
    pub(crate) pfx_path: Option<String>,
    /// RFC 3161 timestamp URL. Defaults to `DigiCert`.
    pub(crate) timestamp_url: Option<String>,
}

#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
impl WindowsSigningConfig {
    /// True when any credential source is configured.
    pub(crate) fn is_configured(&self) -> bool {
        self.azure_account.is_some() || self.sha1.is_some() || self.pfx_path.is_some()
    }

    pub(crate) fn resolved_timestamp_url(&self) -> &str {
        self.timestamp_url
            .as_deref()
            .unwrap_or("http://timestamp.digicert.com")
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
    /// Defaults to `{vendor_id}.{bundle_id}` when absent.
    pub(crate) app_id: Option<String>,
}

#[derive(Deserialize, Default)]
pub(crate) struct MacosConfig {
    /// Path to the AAX SDK root directory. Falls back to the `AAX_SDK_PATH` env var.
    pub(crate) aax_sdk_path: Option<String>,
    #[serde(default)]
    pub(crate) signing: MacosSigningConfig,
    /// Notarization config — only the `cmd_package_macos` path reads
    /// these fields, so on Windows / Linux they're parsed-and-ignored.
    #[serde(default)]
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
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
    /// `TRUCE_INSTALLER_SIGNING_IDENTITY` env var. macOS-only — only the
    /// `productbuild` step in `cmd_package_macos` consumes this.
    #[cfg(target_os = "macos")]
    pub(crate) fn installer_identity(&self) -> Option<&str> {
        self.signing.installer_identity.as_deref()
    }
}

#[derive(Deserialize, Default)]
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub(crate) struct MacosPackagingConfig {
    #[serde(default)]
    pub(crate) notarize: bool,
    pub(crate) apple_id: Option<String>,
    pub(crate) team_id: Option<String>,
}

#[derive(Deserialize, Default)]
pub(crate) struct PackagingConfig {
    #[serde(default)]
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    pub(crate) formats: Vec<String>,
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    pub(crate) welcome_html: Option<String>,
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    pub(crate) license_html: Option<String>,
    /// Preferred scope for `cargo truce package`: `"user"`,
    /// `"system"`, or `"ask"`. Absent = `"ask"` (the indie-installer
    /// convention where the end user picks at install time). CLI
    /// flags (`--user` / `--system` / `--ask`) override.
    /// Linux has no packaging pipeline, so the field is read only on
    /// macOS / Windows.
    #[cfg_attr(not(any(target_os = "macos", target_os = "windows")), allow(dead_code))]
    pub(crate) preferred_scope: Option<String>,
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
    pub(crate) bundle_id: String,
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
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
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
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    pub(crate) au_name: Option<String>,
    #[serde(default)]
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    pub(crate) au3_name: Option<String>,
    #[serde(default)]
    #[cfg_attr(not(any(target_os = "macos", target_os = "windows")), allow(dead_code))]
    pub(crate) aax_name: Option<String>,
    #[serde(default)]
    pub(crate) lv2_name: Option<String>,
}

impl PluginDef {
    pub(crate) fn resolved_fourcc(&self) -> &str {
        self.fourcc
            .as_deref()
            .or(self.au_subtype.as_deref())
            .expect("truce.toml: each [[plugin]] requires `fourcc` or `au_subtype`")
    }
    pub(crate) fn resolved_au_type(&self) -> &str {
        // Keep in sync with `truce-derive::plugin_info`. NoteEffect →
        // `aumi` (Apple's MIDI Processor). `aumi` plugins declare no
        // audio buses per Apple spec — wrappers that can't express
        // that (AAX) synthesize dummy audio I/O internally.
        self.au_type
            .as_deref()
            .unwrap_or(match self.category.as_str() {
                "instrument" => "aumu",
                "midi" | "note_effect" => "aumi",
                _ => "aufx",
            })
    }
    pub(crate) fn au3_sub(&self) -> &str {
        self.au3_subtype
            .as_deref()
            .unwrap_or(self.resolved_fourcc())
    }

    /// Name used for the AU v3 containing `.app` bundle directory.
    /// When `au3_name` is set in truce.toml it wins (both display
    /// name in host browsers and bundle path stay in sync). Otherwise
    /// we fall back to the historical `"{name} v3"` disambiguator so
    /// projects that haven't opted in are unaffected. macOS-only —
    /// AU v3 only installs to `/Applications/` on macOS.
    #[cfg(target_os = "macos")]
    pub(crate) fn au3_app_name(&self) -> String {
        match self.au3_name.as_deref() {
            Some(n) if !n.is_empty() => n.to_string(),
            _ => format!("{} v3", self.name),
        }
    }
    #[cfg(target_os = "macos")]
    pub(crate) fn fw_name(&self) -> String {
        let cap = format!(
            "{}{}",
            self.bundle_id[..1].to_uppercase(),
            &self.bundle_id[1..]
        );
        format!("Truce{cap}AU")
    }
    /// Dylib filename stem derived from the crate name (hyphens → underscores).
    pub(crate) fn dylib_stem(&self) -> String {
        self.crate_name.replace('-', "_")
    }
}

fn default_au_tag() -> String {
    "Effects".to_string()
}

/// One `[[suite]]` entry from `truce.toml`. Bundles a subset of the
/// workspace's plugins into a single installer per platform.
///
/// Defaults: `plugins` omitted → all workspace plugins; `formats`
/// omitted → union of every included plugin's enabled formats;
/// `version` omitted → workspace version. `plugins` and
/// `exclude_plugins` are mutually exclusive — supplying both is a
/// hard error caught at validation time.
#[derive(Deserialize, Debug)]
pub(crate) struct SuiteDef {
    pub(crate) name: String,
    pub(crate) bundle_id: String,
    /// Explicit plugin list. Names match `[[plugin]].crate` (or
    /// `[[plugin]].bundle_id` — both accepted). Omit for "all".
    #[serde(default)]
    pub(crate) plugins: Option<Vec<String>>,
    /// Plugins to exclude from the otherwise-implicit "all". Mutually
    /// exclusive with `plugins`.
    #[serde(default)]
    pub(crate) exclude_plugins: Option<Vec<String>>,
    /// Per-suite format restriction. Intersected with each included
    /// plugin's enabled formats. Omit for the union.
    #[serde(default)]
    pub(crate) formats: Option<Vec<String>>,
    /// Suite-level version. Falls back to `[workspace.package].version`.
    #[serde(default)]
    pub(crate) version: Option<String>,
    /// Display blurb in the installer welcome page (where supported).
    #[serde(default)]
    #[cfg_attr(not(any(target_os = "macos", target_os = "windows")), allow(dead_code))]
    pub(crate) description: Option<String>,
}

impl SuiteDef {
    /// Validate the suite against the workspace config. Returns the
    /// resolved plugin set + format set the suite should ship.
    ///
    /// `workspace_plugins` is the full `Config::plugin` slice. Plugin
    /// names in the suite's `plugins` / `exclude_plugins` fields can
    /// be either the cargo crate name (`[[plugin]].crate`) or the
    /// `bundle_id`; both forms resolve here.
    pub(crate) fn resolve<'a>(
        &'a self,
        workspace_plugins: &'a [PluginDef],
    ) -> Result<ResolvedSuite<'a>, BoxErr> {
        if self.plugins.is_some() && self.exclude_plugins.is_some() {
            return Err(format!(
                "[[suite]] '{}' sets both `plugins` and `exclude_plugins` — \
                 these are mutually exclusive",
                self.name,
            )
            .into());
        }

        let resolve_one = |needle: &str| -> Result<&'a PluginDef, BoxErr> {
            workspace_plugins
                .iter()
                .find(|p| p.crate_name == needle || p.bundle_id == needle)
                .ok_or_else(|| {
                    format!(
                        "[[suite]] '{}': plugin '{}' is not in the workspace. \
                         Available: {}",
                        self.name,
                        needle,
                        workspace_plugins
                            .iter()
                            .map(|p| p.crate_name.as_str())
                            .collect::<Vec<_>>()
                            .join(", "),
                    )
                    .into()
                })
        };

        let plugins: Vec<&PluginDef> = if let Some(list) = &self.plugins {
            list.iter()
                .map(|s| resolve_one(s))
                .collect::<Result<Vec<_>, _>>()?
        } else if let Some(excl) = &self.exclude_plugins {
            let exclude_set: Vec<&PluginDef> = excl
                .iter()
                .map(|s| resolve_one(s))
                .collect::<Result<Vec<_>, _>>()?;
            workspace_plugins
                .iter()
                .filter(|p| !exclude_set.iter().any(|e| std::ptr::eq(*e, *p)))
                .collect()
        } else {
            workspace_plugins.iter().collect()
        };

        if plugins.is_empty() {
            return Err(format!(
                "[[suite]] '{}' resolves to zero plugins after \
                 plugins/exclude_plugins resolution",
                self.name,
            )
            .into());
        }

        Ok(ResolvedSuite {
            def: self,
            plugins,
            formats: self.formats.as_deref(),
        })
    }
}

/// Result of [`SuiteDef::resolve`]. Borrows from the original
/// workspace config so we don't clone every plugin per suite.
pub(crate) struct ResolvedSuite<'a> {
    pub(crate) def: &'a SuiteDef,
    pub(crate) plugins: Vec<&'a PluginDef>,
    /// Caller intersects this with each plugin's enabled formats.
    /// `None` = no per-suite restriction (use union of plugin defaults).
    #[allow(dead_code)]
    pub(crate) formats: Option<&'a [String]>,
}

/// Resolve the application signing identity:
/// `[macos.signing].application_identity` → `TRUCE_SIGNING_IDENTITY` env →
/// `.cargo/config.toml` `[env].TRUCE_SIGNING_IDENTITY` → ad-hoc.
fn resolve_signing_identity(config: &Config) -> String {
    // 1. truce.toml explicit value
    if let Some(id) = &config.macos.signing.application_identity
        && !id.is_empty()
        && id != "-"
    {
        return id.clone();
    }
    // 2. Environment variable
    if let Ok(id) = std::env::var("TRUCE_SIGNING_IDENTITY")
        && !id.is_empty()
    {
        return id;
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
        toml::Value::Table(t) => t
            .get("value")?
            .as_str()
            .map(std::string::ToString::to_string),
        _ => None,
    }
}

/// Resolve the installer signing identity:
/// `[macos.signing].installer_identity` → `TRUCE_INSTALLER_SIGNING_IDENTITY`
/// env → `.cargo/config.toml` → None.
fn resolve_installer_identity(config: &Config) -> Option<String> {
    if let Some(ref id) = config.macos.signing.installer_identity
        && !id.is_empty()
    {
        return Some(id.clone());
    }
    if let Ok(id) = std::env::var("TRUCE_INSTALLER_SIGNING_IDENTITY")
        && !id.is_empty()
    {
        return Some(id);
    }
    if let Some(id) = read_cargo_config_env("TRUCE_INSTALLER_SIGNING_IDENTITY") {
        return Some(id);
    }
    None
}

/// Read `MACOSX_DEPLOYMENT_TARGET` from the environment, defaulting to "11.0".
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
    if let Some(p) = toml_path.0 {
        let path = PathBuf::from(p);
        if path.exists() {
            return Some(path);
        }
        eprintln!(
            "warning: {} = {:?} in truce.toml but directory not found",
            toml_path.1, p
        );
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

#[cfg(test)]
mod suite_tests {
    use super::*;

    fn plugin(crate_name: &str, bundle_id: &str) -> PluginDef {
        PluginDef {
            name: crate_name.into(),
            bundle_id: bundle_id.into(),
            crate_name: crate_name.into(),
            fourcc: None,
            category: "effect".into(),
            au_type: None,
            au_subtype: None,
            au3_subtype: None,
            au_tag: default_au_tag(),
            clap_name: None,
            vst3_name: None,
            vst2_name: None,
            au_name: None,
            au3_name: None,
            aax_name: None,
            lv2_name: None,
        }
    }

    fn suite(name: &str) -> SuiteDef {
        SuiteDef {
            name: name.into(),
            bundle_id: name.to_lowercase(),
            plugins: None,
            exclude_plugins: None,
            formats: None,
            version: None,
            description: None,
        }
    }

    #[test]
    fn default_resolves_to_all_workspace_plugins() {
        let plugins = vec![plugin("a", "a"), plugin("b", "b"), plugin("c", "c")];
        let s = suite("Studio");
        let r = match s.resolve(&plugins) {
            Ok(r) => r,
            Err(e) => panic!("resolve failed: {e}"),
        };
        assert_eq!(r.plugins.len(), 3);
    }

    #[test]
    fn explicit_plugin_list_narrows() {
        let plugins = vec![plugin("a", "a"), plugin("b", "b"), plugin("c", "c")];
        let mut s = suite("Studio");
        s.plugins = Some(vec!["a".into(), "c".into()]);
        let r = match s.resolve(&plugins) {
            Ok(r) => r,
            Err(e) => panic!("resolve failed: {e}"),
        };
        let names: Vec<_> = r.plugins.iter().map(|p| p.crate_name.as_str()).collect();
        assert_eq!(names, vec!["a", "c"]);
    }

    #[test]
    fn exclude_plugins_inverts() {
        let plugins = vec![plugin("a", "a"), plugin("b", "b"), plugin("c", "c")];
        let mut s = suite("Studio");
        s.exclude_plugins = Some(vec!["b".into()]);
        let r = match s.resolve(&plugins) {
            Ok(r) => r,
            Err(e) => panic!("resolve failed: {e}"),
        };
        let names: Vec<_> = r.plugins.iter().map(|p| p.crate_name.as_str()).collect();
        assert_eq!(names, vec!["a", "c"]);
    }

    #[test]
    fn bundle_id_resolves_alongside_crate_name() {
        let plugins = vec![plugin("acme-gain", "gain")];
        let mut s = suite("Studio");
        // Reference by bundle_id rather than crate name.
        s.plugins = Some(vec!["gain".into()]);
        let r = match s.resolve(&plugins) {
            Ok(r) => r,
            Err(e) => panic!("resolve failed: {e}"),
        };
        assert_eq!(r.plugins.len(), 1);
    }

    #[test]
    fn unknown_plugin_errors() {
        let plugins = vec![plugin("a", "a")];
        let mut s = suite("Studio");
        s.plugins = Some(vec!["does-not-exist".into()]);
        let err = match s.resolve(&plugins) {
            Err(e) => e.to_string(),
            Ok(_) => panic!("expected resolve to error"),
        };
        assert!(err.contains("does-not-exist"), "got: {err}");
        assert!(err.contains("Studio"), "got: {err}");
    }

    #[test]
    fn plugins_and_exclude_plugins_both_set_errors() {
        let plugins = vec![plugin("a", "a")];
        let mut s = suite("Studio");
        s.plugins = Some(vec!["a".into()]);
        s.exclude_plugins = Some(vec!["a".into()]);
        let err = match s.resolve(&plugins) {
            Err(e) => e.to_string(),
            Ok(_) => panic!("expected resolve to error"),
        };
        assert!(err.contains("mutually exclusive"));
    }

    #[test]
    fn empty_resolution_errors() {
        // Three plugins, exclude all three → zero remaining.
        let plugins = vec![plugin("a", "a"), plugin("b", "b")];
        let mut s = suite("Studio");
        s.exclude_plugins = Some(vec!["a".into(), "b".into()]);
        let err = match s.resolve(&plugins) {
            Err(e) => e.to_string(),
            Ok(_) => panic!("expected resolve to error"),
        };
        assert!(err.contains("zero plugins"), "got: {err}");
    }
}
