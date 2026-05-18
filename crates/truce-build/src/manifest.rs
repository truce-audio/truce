//! Build manifest - the contract between `cargo truce build` and
//! `cargo truce package`.
//!
//! `cargo truce build` writes `target/bundles/manifest.toml` recording
//! every bundle it produced (plugin crate, format, filename) plus the
//! build context (host triple, profile). `cargo truce package` reads
//! this manifest to know what to stage; absence of the manifest, or
//! mismatch with the package host, is a hard error.
//!
//! The manifest replaces an earlier implicit contract where the
//! packager probed `target/bundles/<Plugin>.<format>` directly. That
//! probe couldn't tell a missing build apart from a partial build, and
//! produced empty-payload tarballs that passed every downstream check.
//! The manifest makes the contract explicit: build declares what it
//! produced, package validates against that declaration.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Bumped on any incompatible schema change. Consumers reject manifests
/// whose `schema_version` doesn't match this constant.
///
/// v2 (current): `host_triple` renamed to `target_triple`; the manifest
///              now lives at `target/bundles/<triple>/manifest.toml`
///              instead of `target/bundles/manifest.toml`, so
///              cross-target builds can coexist on disk.
/// v1: top-level `host_triple` + flat `target/bundles/manifest.toml`.
pub const SCHEMA_VERSION: u32 = 2;

const FILENAME: &str = "manifest.toml";

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct BundleManifest {
    pub schema_version: u32,
    /// Cargo target triple the bundles were compiled for. The
    /// containing directory's name should match this - the field is
    /// authoritative; the path is convention.
    pub target_triple: String,
    /// Cargo profile name: `"release"`, `"debug"`, or `"shell"`.
    pub profile: String,
    #[serde(default, rename = "bundle")]
    pub bundles: Vec<BundleEntry>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct BundleEntry {
    pub plugin_crate: String,
    pub plugin_name: String,
    pub plugin_bundle_id: String,
    /// Format slug: `"clap"`, `"vst3"`, `"vst2"`, `"lv2"`, `"au2"`,
    /// `"au3"`, or `"aax"`. Stable across the manifest schema.
    pub format: String,
    /// Bundle filename relative to the manifest's enclosing dir
    /// (i.e. `target/bundles/`). May be a directory bundle
    /// (`Foo.clap/`) or a bare file (`Foo.so` for Linux VST2).
    pub filename: String,
}

impl BundleManifest {
    #[must_use]
    pub fn new(target_triple: impl Into<String>, profile: impl Into<String>) -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            target_triple: target_triple.into(),
            profile: profile.into(),
            bundles: Vec::new(),
        }
    }

    /// Path to a manifest within a per-target bundles directory.
    /// Callers pass the directory that already names the target triple
    /// (e.g. `target/bundles/x86_64-unknown-linux-gnu/`).
    #[must_use]
    pub fn manifest_path(bundles_dir: &Path) -> PathBuf {
        bundles_dir.join(FILENAME)
    }

    /// Load the manifest, returning a clear "run cargo truce build"
    /// message when it's missing.
    ///
    /// # Errors
    /// Returns `Err` when the file is missing, unreadable, fails TOML
    /// parsing, or has an unsupported `schema_version`.
    pub fn load(bundles_dir: &Path) -> Result<Self, String> {
        let path = Self::manifest_path(bundles_dir);
        let content = std::fs::read_to_string(&path).map_err(|e| {
            format!(
                "no build manifest at {}: {e}\n\
                 Run `cargo truce build` first to produce bundles.",
                path.display()
            )
        })?;
        let manifest: Self = toml::from_str(&content)
            .map_err(|e| format!("failed to parse build manifest at {}: {e}", path.display()))?;
        if manifest.schema_version != SCHEMA_VERSION {
            return Err(format!(
                "build manifest at {} has schema_version {} (expected {SCHEMA_VERSION}). \
                 Re-run `cargo truce build`.",
                path.display(),
                manifest.schema_version,
            ));
        }
        Ok(manifest)
    }

    /// `Ok(None)` when the manifest doesn't exist; `Err` only on
    /// read/parse failure of an existing manifest.
    ///
    /// # Errors
    /// Returns `Err` if the manifest exists but cannot be read or parsed.
    pub fn load_if_present(bundles_dir: &Path) -> Result<Option<Self>, String> {
        let path = Self::manifest_path(bundles_dir);
        if !path.exists() {
            return Ok(None);
        }
        Self::load(bundles_dir).map(Some)
    }

    /// # Errors
    /// Returns `Err` if serialization fails or the file cannot be written.
    pub fn save(&self, bundles_dir: &Path) -> Result<(), String> {
        let path = Self::manifest_path(bundles_dir);
        let body = toml::to_string_pretty(self)
            .map_err(|e| format!("failed to serialize build manifest: {e}"))?;
        std::fs::write(&path, body)
            .map_err(|e| format!("failed to write build manifest at {}: {e}", path.display()))
    }

    /// Merge `incoming` into `self`. If `target_triple` or `profile`
    /// differs, `incoming` replaces `self` wholesale - bundles built
    /// for a different target/profile aren't usable alongside the new
    /// ones. Otherwise, entries with matching `(plugin_crate, format)`
    /// are replaced and new entries are appended.
    pub fn merge(&mut self, incoming: BundleManifest) {
        if self.target_triple != incoming.target_triple || self.profile != incoming.profile {
            *self = incoming;
            return;
        }
        for entry in incoming.bundles {
            self.bundles
                .retain(|e| !(e.plugin_crate == entry.plugin_crate && e.format == entry.format));
            self.bundles.push(entry);
        }
    }

    pub fn bundles_for_plugin<'a>(
        &'a self,
        plugin_crate: &'a str,
    ) -> impl Iterator<Item = &'a BundleEntry> + 'a {
        self.bundles
            .iter()
            .filter(move |e| e.plugin_crate == plugin_crate)
    }
}

/// Cargo target triple of the host running this binary. Recorded in
/// the manifest so consumers can detect cross-host mismatches.
#[must_use]
pub fn host_triple() -> &'static str {
    if cfg!(all(target_os = "linux", target_arch = "x86_64")) {
        "x86_64-unknown-linux-gnu"
    } else if cfg!(all(target_os = "linux", target_arch = "aarch64")) {
        "aarch64-unknown-linux-gnu"
    } else if cfg!(all(target_os = "macos", target_arch = "x86_64")) {
        "x86_64-apple-darwin"
    } else if cfg!(all(target_os = "macos", target_arch = "aarch64")) {
        "aarch64-apple-darwin"
    } else if cfg!(all(target_os = "windows", target_arch = "x86_64")) {
        "x86_64-pc-windows-msvc"
    } else if cfg!(all(target_os = "windows", target_arch = "aarch64")) {
        "aarch64-pc-windows-msvc"
    } else {
        "unknown"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(plugin: &str, format: &str, filename: &str) -> BundleEntry {
        BundleEntry {
            plugin_crate: plugin.into(),
            plugin_name: plugin.into(),
            plugin_bundle_id: format!("com.example.{plugin}"),
            format: format.into(),
            filename: filename.into(),
        }
    }

    #[test]
    fn merge_replaces_matching_plugin_format_keys() {
        let mut m = BundleManifest::new("x86_64-unknown-linux-gnu", "release");
        m.bundles.push(entry("a", "clap", "A.clap"));
        m.bundles.push(entry("b", "vst3", "B.vst3"));

        let mut incoming = BundleManifest::new("x86_64-unknown-linux-gnu", "release");
        incoming.bundles.push(entry("a", "clap", "A-new.clap"));
        incoming.bundles.push(entry("a", "vst3", "A.vst3"));

        m.merge(incoming);

        assert_eq!(m.bundles.len(), 3);
        assert!(m.bundles.iter().any(|e| e.filename == "A-new.clap"));
        assert!(m.bundles.iter().any(|e| e.filename == "A.vst3"));
        assert!(m.bundles.iter().any(|e| e.filename == "B.vst3"));
    }

    #[test]
    fn merge_wipes_when_target_or_profile_differs() {
        let mut m = BundleManifest::new("x86_64-apple-darwin", "release");
        m.bundles.push(entry("a", "clap", "A.clap"));

        let mut incoming = BundleManifest::new("x86_64-unknown-linux-gnu", "release");
        incoming.bundles.push(entry("b", "vst3", "B.vst3"));

        m.merge(incoming);

        assert_eq!(m.target_triple, "x86_64-unknown-linux-gnu");
        assert_eq!(m.bundles.len(), 1);
        assert_eq!(m.bundles[0].filename, "B.vst3");
    }

    #[test]
    fn merge_wipes_when_profile_differs() {
        let mut m = BundleManifest::new("x86_64-unknown-linux-gnu", "release");
        m.bundles.push(entry("a", "clap", "A.clap"));

        let mut incoming = BundleManifest::new("x86_64-unknown-linux-gnu", "debug");
        incoming.bundles.push(entry("a", "clap", "A-debug.clap"));

        m.merge(incoming);

        assert_eq!(m.profile, "debug");
        assert_eq!(m.bundles.len(), 1);
        assert_eq!(m.bundles[0].filename, "A-debug.clap");
    }

    #[test]
    fn round_trip_via_toml() {
        let mut m = BundleManifest::new("x86_64-unknown-linux-gnu", "release");
        m.bundles
            .push(entry("truce-gain", "clap", "Truce Gain.clap"));
        m.bundles
            .push(entry("truce-gain", "vst3", "Truce Gain.vst3"));

        let s = toml::to_string_pretty(&m).unwrap();
        let parsed: BundleManifest = toml::from_str(&s).unwrap();
        assert_eq!(parsed.target_triple, m.target_triple);
        assert_eq!(parsed.bundles, m.bundles);
    }
}
