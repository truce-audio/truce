//! Project configuration (read from `truce.toml`).
//!
//! `truce.toml` carries **project-level** facts only: vendor info,
//! plugin definitions, suite definitions, and packaging metadata
//! (publisher name, license file, installer icon).
//!
//! **Per-developer credentials and machine-specific paths
//! (signing identities, AAX SDK location, notarization Apple ID /
//! team ID, Authenticode certs) live in `.cargo/config.toml`'s
//! `[env]` table.** Cargo injects those into the environment before
//! invoking `cargo truce`, so the resolvers below just read
//! `std::env::var`. A direct-read fallback (`read_cargo_config_env`)
//! covers the rare case where `cargo-truce` runs outside cargo.
//!
//! There is no truce.toml-side option for any of these - by design.
//! The split keeps secrets out of the tracked file and removes the
//! "which copy wins?" question every time a developer onboards.

use crate::{BoxErr, project_root};
use serde::Deserialize;
use std::fs;
use std::path::PathBuf;

#[derive(Deserialize)]
pub(crate) struct Config {
    #[serde(default)]
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    pub(crate) macos: MacosConfig,
    #[serde(default)]
    #[cfg_attr(not(target_os = "windows"), allow(dead_code))]
    pub(crate) windows: WindowsConfig,
    /// iOS-only workspace config (default minimum OS version, etc.).
    /// Per-plugin iOS metadata (app group, icon set) lives on
    /// [`PluginDef`]. Team ID and signing identities come from
    /// `.cargo/config.toml [env]` (see `ios_team_id()` /
    /// `ios_application_identity()`), keeping per-developer
    /// credentials out of the tracked file.
    #[serde(default)]
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    pub(crate) ios: IosConfig,
    pub(crate) vendor: VendorConfig,
    pub(crate) plugin: Vec<PluginDef>,
    /// Packaging metadata (welcome HTML, license HTML, etc.). Consumed
    /// by `cmd_package_macos` only - Windows packaging uses
    /// `WindowsConfig::packaging`, Linux has no packaging path.
    #[serde(default)]
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    pub(crate) packaging: PackagingConfig,
    /// Suite installers - repeatable. Each entry produces one
    /// installer per platform that bundles the listed plugins.
    /// Empty = per-plugin output only (today's behaviour).
    #[serde(default, rename = "suite")]
    pub(crate) suites: Vec<SuiteDef>,
}

#[derive(Deserialize, Default)]
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub(crate) struct IosConfig {
    /// Default minimum iOS version for all plugins. Per-plugin
    /// `ios_minimum_os_version` overrides. Apple deprecates older
    /// SDK targets aggressively; 16.0 is the lowest officially
    /// supported by the `AUv3` + Swift toolchain we drive.
    #[serde(default)]
    pub(crate) minimum_os_version: Option<String>,
}

#[derive(Deserialize, Default)]
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
pub(crate) struct WindowsConfig {
    #[serde(default)]
    pub(crate) packaging: WindowsPackagingConfig,
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
    /// Notarization config - only the `cmd_package_macos` path reads
    /// these fields, so on Windows / Linux they're parsed-and-ignored.
    #[serde(default)]
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    pub(crate) packaging: MacosPackagingConfig,
}

#[derive(Deserialize, Default)]
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub(crate) struct MacosPackagingConfig {
    /// Whether to run `xcrun notarytool submit` on the produced
    /// `.pkg`. Project-level decision (release vs. dev build);
    /// the credentials it uses come from env vars
    /// (`TRUCE_NOTARY_PROFILE` keychain profile, or
    /// `APPLE_ID` + `TEAM_ID` + `APP_SPECIFIC_PASSWORD`).
    #[serde(default)]
    pub(crate) notarize: bool,
    /// Welcome-page HTML for the productbuild Distribution wizard
    /// (relative to workspace root). macOS-only - Windows has its own
    /// `[windows.packaging] welcome_bmp` slot with a different file
    /// format (164x314 .bmp).
    pub(crate) welcome_html: Option<String>,
    /// License-page HTML for the productbuild Distribution wizard.
    /// macOS-only - Windows uses `[windows.packaging] license_rtf`.
    pub(crate) license_html: Option<String>,
}

#[derive(Deserialize, Default)]
pub(crate) struct PackagingConfig {
    /// Default format list for `cargo truce package` when no
    /// `--formats` flag is passed. Cross-platform - both the macOS
    /// `.pkg` and Windows Inno Setup paths read it. Linux's tarball
    /// pipeline ignores it (Linux ships every default-feature format
    /// the plugin built, no opt-in).
    #[serde(default)]
    #[cfg_attr(not(any(target_os = "macos", target_os = "windows")), allow(dead_code))]
    pub(crate) formats: Vec<String>,
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
    /// Reverse-DNS vendor identifier (e.g. `com.acme`). Used by macOS
    /// `CFBundleIdentifier` plists and Windows Inno Setup paths;
    /// Linux VST3 bundles don't include a plist, so the field looks
    /// dead there. Keep cfg-gated to silence the lint without changing
    /// the schema.
    #[cfg_attr(not(any(target_os = "macos", target_os = "windows")), allow(dead_code))]
    pub(crate) id: String,
    /// Vendor website URL. Used by the Windows Inno Setup installer's
    /// "Publisher URL" field; unused on macOS.
    #[serde(default)]
    #[cfg_attr(not(target_os = "windows"), allow(dead_code))]
    pub(crate) url: Option<String>,
    pub(crate) au_manufacturer: String,
}

/// Install-time view of a `[[plugin]]` entry.
///
/// Wraps the shared `truce_build::PluginDef` schema (consumed by the
/// proc macros) and adds install-only fields (`au3_subtype`,
/// `au_tag`). `Deref` exposes the shared fields so call sites read
/// `p.name` / `p.bundle_id` directly without going through `p.shared`.
#[derive(Deserialize)]
pub(crate) struct PluginDef {
    #[serde(flatten)]
    pub(crate) shared: truce_build::PluginDef,
    #[serde(default)]
    pub(crate) au3_subtype: Option<String>,
    #[serde(default = "default_au_tag")]
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    pub(crate) au_tag: String,
    /// Per-plugin Windows app icon (`.ico`, path relative to workspace
    /// root). Embedded as `RT_GROUP_ICON` in the standalone `.exe`.
    /// Distinct from `[windows.packaging] installer_icon` (Inno-wizard
    /// chrome): a vendor with one installer-window logo can still ship
    /// different per-product app icons.
    #[serde(default)]
    #[cfg_attr(not(target_os = "windows"), allow(dead_code))]
    pub(crate) windows_icon: Option<String>,
    /// Per-plugin macOS app icon (`.icns`, path relative to workspace
    /// root). Copied into the standalone `.app`'s `Contents/Resources/`
    /// and referenced by `CFBundleIconFile` so Finder, the Dock,
    /// Launchpad, and Spotlight pick it up. Linux uses `.desktop` +
    /// freedesktop icons - file formats don't survive a single
    /// cross-OS slot. macOS has no installer-chrome icon equivalent;
    /// `.pkg` files inherit Installer.app's icon by design.
    #[serde(default)]
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    pub(crate) macos_icon: Option<String>,
    /// App Group identifier for cross-process preset / state sharing
    /// between the container `.app` and the `.appex`. When present,
    /// adds `com.apple.security.application-groups` to both
    /// entitlements files so `fullState` blobs round-trip and
    /// `FileManager.containerURL(forSecurityApplicationGroupIdentifier:)`
    /// resolves. Convention: `group.{vendor.id}.{bundle_id}`.
    #[serde(default)]
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    pub(crate) ios_app_group: Option<String>,
    /// Per-plugin iOS app icon (`.appiconset` directory, path relative
    /// to workspace root). Copied into the container app's resources
    /// at install / package time. Absent → the container ships with
    /// the system default icon (fine for simulator-only smoke testing,
    /// rejected by App Store review for distribution builds).
    #[serde(default)]
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    pub(crate) ios_icon_set: Option<String>,
    /// Per-plugin iOS minimum OS version override. Falls back to
    /// `[ios].minimum_os_version`, which itself defaults to "16.0".
    #[serde(default)]
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    pub(crate) ios_minimum_os_version: Option<String>,
    /// Per-plugin URL the iOS container's "About" sheet links to
    /// (the link-out icon in the top-right opens this in Safari).
    /// Falls back to `[vendor].url`, then to <https://truce.audio/>.
    /// Useful when a plug-in has its own product page distinct from
    /// the vendor's homepage - common in suites where individual
    /// plug-ins ship with separate marketing pages.
    #[serde(default)]
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    pub(crate) ios_url: Option<String>,
    /// Per-plugin allowed interface orientations for the iOS
    /// container app. Accepted values: `"portrait"`,
    /// `"portrait-upside-down"`, `"landscape-left"`,
    /// `"landscape-right"`. The first entry becomes the launch
    /// orientation. Absent → defaults to
    /// `["portrait", "landscape-left", "landscape-right"]`
    /// (preserves the historical behaviour). Empty array is
    /// rejected at install time.
    #[serde(default)]
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    pub(crate) ios_orientations: Option<Vec<String>>,
    /// Scale the embedded editor uniformly to fit the
    /// container's hero region while preserving aspect ratio.
    /// Never up-scales above 1.0. Default `true` - desktop-sized
    /// editors are the common case and overflow the iPhone screen
    /// without it. Opt out (`false`) for plug-ins whose editor is
    /// already iPhone-sized or that ship multiple per-orientation
    /// layouts and want verbatim natural-pixel rendering.
    #[serde(default = "default_true")]
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    pub(crate) ios_scale_editor_to_fit: bool,
}

fn default_true() -> bool {
    true
}

impl std::ops::Deref for PluginDef {
    type Target = truce_build::PluginDef;
    fn deref(&self) -> &Self::Target {
        &self.shared
    }
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
        // audio buses per Apple spec - wrappers that can't express
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

    /// Resolved iOS minimum OS version: per-plugin override → workspace
    /// `[ios].minimum_os_version` → `"16.0"`. The 16.0 floor matches
    /// what the Swift + `AUv3` toolchain we drive supports without
    /// deprecation warnings.
    #[cfg(target_os = "macos")]
    pub(crate) fn resolved_ios_minimum_os_version(&self, ios: &IosConfig) -> String {
        self.ios_minimum_os_version
            .clone()
            .or_else(|| ios.minimum_os_version.clone())
            .unwrap_or_else(|| "16.0".to_string())
    }

    /// Resolved iOS App Group identifier. `None` → no group entitlement
    /// is added. `Some(s)` → both container + appex get the
    /// `com.apple.security.application-groups` entitlement.
    #[cfg(target_os = "macos")]
    pub(crate) fn resolved_ios_app_group(&self) -> Option<&str> {
        self.ios_app_group.as_deref()
    }

    /// Name used for the AU v3 containing `.app` bundle directory.
    /// When `au3_name` is set in truce.toml it wins (both display
    /// name in host browsers and bundle path stay in sync). Otherwise
    /// we fall back to the historical `"{name} v3"` disambiguator so
    /// projects that haven't opted in are unaffected. macOS-only -
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
/// Defaults: `plugins` omitted → all workspace plugins;
/// `version` omitted → workspace version. `plugins` and
/// `exclude_plugins` are mutually exclusive - supplying both is a
/// hard error caught at validation time.
#[derive(Deserialize, Debug)]
pub(crate) struct SuiteDef {
    pub(crate) name: String,
    pub(crate) bundle_id: String,
    /// Explicit plugin list. Names match `[[plugin]].crate` (or
    /// `[[plugin]].bundle_id` - both accepted). Omit for "all".
    #[serde(default)]
    pub(crate) plugins: Option<Vec<String>>,
    /// Plugins to exclude from the otherwise-implicit "all". Mutually
    /// exclusive with `plugins`.
    #[serde(default)]
    pub(crate) exclude_plugins: Option<Vec<String>>,
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
                "[[suite]] '{}' sets both `plugins` and `exclude_plugins` - \
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

        Ok(ResolvedSuite { def: self, plugins })
    }
}

/// Result of [`SuiteDef::resolve`]. Borrows from the original
/// workspace config so we don't clone every plugin per suite.
pub(crate) struct ResolvedSuite<'a> {
    pub(crate) def: &'a SuiteDef,
    pub(crate) plugins: Vec<&'a PluginDef>,
}

/// Read a per-developer build env var. Cargo injects values from
/// `.cargo/config.toml`'s `[env]` table into the environment of any
/// subcommand it spawns, so `std::env::var(key)` is the normal path.
/// As a fallback for the rare `cargo-truce` invocation that doesn't
/// go through cargo (e.g. running `target/release/cargo-truce`
/// directly), parse `.cargo/config.toml` ourselves.
///
/// Returns `None` for missing or empty values (an empty string from
/// either source is treated as unset). Cargo's `force = true`
/// override on a `[env]` entry is handled transparently because
/// cargo has already applied it to the process environment by the
/// time we read.
pub(crate) fn read_build_env(key: &str) -> Option<String> {
    if let Ok(v) = std::env::var(key)
        && !v.is_empty()
    {
        return Some(v);
    }
    let root = project_root();
    let path = root.join(".cargo/config.toml");
    let content = fs::read_to_string(&path).ok()?;
    let doc: toml::Table = content.parse().ok()?;
    let env = doc.get("env")?.as_table()?;
    // Supports both `KEY = "value"` and `KEY = { value = "...", force = true }`.
    let raw = match env.get(key)? {
        toml::Value::String(s) => s.clone(),
        toml::Value::Table(t) => t.get("value")?.as_str()?.to_string(),
        _ => return None,
    };
    if raw.is_empty() { None } else { Some(raw) }
}

/// Resolved application signing identity. `"-"` means ad-hoc /
/// unsigned (the default). Read from the `TRUCE_SIGNING_IDENTITY`
/// build env. The accessor stays in this module so all callers have
/// one path to follow when they need to know "where does this come
/// from?"
pub(crate) fn application_identity() -> String {
    read_build_env("TRUCE_SIGNING_IDENTITY").unwrap_or_else(|| "-".to_string())
}

/// Resolved installer signing identity. `None` means the installer
/// won't be signed. Read from the `TRUCE_INSTALLER_SIGNING_IDENTITY`
/// build env. macOS-only - only the `productbuild` step in
/// `cmd_package_macos` consumes this.
#[cfg(target_os = "macos")]
pub(crate) fn installer_identity() -> Option<String> {
    read_build_env("TRUCE_INSTALLER_SIGNING_IDENTITY")
}

/// Read `MACOSX_DEPLOYMENT_TARGET` from the build env, defaulting
/// to "11.0".
pub(crate) fn deployment_target() -> String {
    read_build_env("MACOSX_DEPLOYMENT_TARGET").unwrap_or_else(|| "11.0".to_string())
}

/// Apple Developer team ID for iOS device / distribution builds.
/// Returned `None` means simulator-only ad-hoc signing is the only
/// viable install path. Source: `TRUCE_IOS_TEAM_ID` build env.
#[cfg(target_os = "macos")]
pub(crate) fn ios_team_id() -> Option<String> {
    read_build_env("TRUCE_IOS_TEAM_ID")
}

/// iOS-specific signing identity (e.g. `"Apple Development: …"` for
/// device builds, `"Apple Distribution: …"` for .ipa releases).
/// Falls back to [`application_identity`] so users without an
/// iOS-specific override get the macOS identity (which is wrong for
/// device installs but right for simulator + ad-hoc). Source:
/// `TRUCE_IOS_SIGNING_IDENTITY`.
#[cfg(target_os = "macos")]
pub(crate) fn ios_application_identity() -> String {
    read_build_env("TRUCE_IOS_SIGNING_IDENTITY").unwrap_or_else(application_identity)
}

/// Path to a `.mobileprovision` provisioning profile for the
/// container `.app`. Required for device installs and `.ipa`
/// packaging - simulator builds proceed without one. Source:
/// `TRUCE_IOS_PROVISIONING_PROFILE`.
#[cfg(target_os = "macos")]
pub(crate) fn ios_provisioning_profile() -> Option<PathBuf> {
    resolve_profile_env("TRUCE_IOS_PROVISIONING_PROFILE")
}

/// Optional path to a `.mobileprovision` for the `.appex` extension
/// when a separate profile is needed (i.e. when
/// `TRUCE_IOS_PROVISIONING_PROFILE` is bound to the container's
/// exact bundle ID, not a wildcard covering both). Source:
/// `TRUCE_IOS_APPEX_PROVISIONING_PROFILE`. Returns `None` when
/// unset - callers fall back to the container app's profile, which
/// works for wildcard profiles that match both IDs.
#[cfg(target_os = "macos")]
pub(crate) fn ios_appex_provisioning_profile() -> Option<PathBuf> {
    resolve_profile_env("TRUCE_IOS_APPEX_PROVISIONING_PROFILE")
}

#[cfg(target_os = "macos")]
fn resolve_profile_env(key: &str) -> Option<PathBuf> {
    let raw = read_build_env(key)?;
    let path = PathBuf::from(&raw);
    if path.exists() {
        return Some(path);
    }
    eprintln!(
        "warning: {key}={raw} (from .cargo/config.toml [env] or shell env) but file does not exist"
    );
    None
}

/// Resolve the AAX SDK path from the `AAX_SDK_PATH` build env. The
/// path must point at an extant directory; a stale value emits a
/// warning and resolves to `None` so callers can degrade gracefully.
pub(crate) fn resolve_aax_sdk_path() -> Option<PathBuf> {
    let raw = read_build_env("AAX_SDK_PATH")?;
    let path = PathBuf::from(&raw);
    if path.exists() {
        return Some(path);
    }
    eprintln!(
        "warning: AAX_SDK_PATH={raw} (from .cargo/config.toml [env] or shell env) but \
         directory does not exist"
    );
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
    let config: Config = toml::from_str(&content)?;
    if config.plugin.is_empty() {
        return Err("No [[plugin]] entries in truce.toml".into());
    }
    Ok(config)
}

#[cfg(test)]
mod suite_tests {
    use super::*;

    fn plugin(crate_name: &str, bundle_id: &str) -> PluginDef {
        PluginDef {
            shared: truce_build::PluginDef {
                name: crate_name.into(),
                bundle_id: bundle_id.into(),
                crate_name: crate_name.into(),
                version: None,
                description: None,
                fourcc: None,
                category: "effect".into(),
                au_type: None,
                au_subtype: None,
                aax_category: None,
                vst3_name: None,
                clap_name: None,
                vst2_name: None,
                au_name: None,
                au3_name: None,
                aax_name: None,
                lv2_name: None,
                mute_preview_output: false,
            },
            au3_subtype: None,
            au_tag: default_au_tag(),
            windows_icon: None,
            macos_icon: None,
            ios_app_group: None,
            ios_icon_set: None,
            ios_minimum_os_version: None,
            ios_url: None,
            ios_orientations: None,
            ios_scale_editor_to_fit: true,
        }
    }

    fn suite(name: &str) -> SuiteDef {
        SuiteDef {
            name: name.into(),
            bundle_id: name.to_lowercase(),
            plugins: None,
            exclude_plugins: None,
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
