#![forbid(unsafe_code)]

//! Build-time helper for truce plugins.
//!
//! Reads `truce.toml` and sets `cargo:rustc-env` variables so the
//! `plugin_info!()` macro can derive all metadata at compile time.
//!
//! # Usage in build.rs
//!
//! ```ignore
//! fn main() {
//!     truce_build::emit_plugin_env();
//! }
//! ```

use serde::Deserialize;
use std::path::{Path, PathBuf};

/// Build/derive-time view of `truce.toml`.
///
/// This is the **shared schema** between `truce-build` (build scripts)
/// and `truce-derive` (proc macros) — both crates read the same TOML
/// to emit metadata, so they consume the same struct definition.
/// `cargo-truce` has its own [richer
/// PluginDef](https://github.com/) that adds install-time fields
/// (per-format display names, AU3 subtype, packaging, …); the install
/// path needs that detail, but build/derive don't, so cargo-truce's
/// shape is intentionally a superset rather than a duplicate of this
/// one.
#[derive(Deserialize, Debug)]
pub struct Config {
    pub vendor: VendorConfig,
    pub plugin: Vec<PluginDef>,
}

#[derive(Deserialize, Debug)]
pub struct VendorConfig {
    pub name: String,
    pub id: String,
    #[serde(default)]
    pub url: String,
    pub au_manufacturer: String,
}

#[derive(Deserialize, Debug)]
pub struct PluginDef {
    pub name: String,
    /// Required, matching `cargo-truce::PluginDef`. Deliberately unread
    /// after deserialization: presence makes serde fail early on a
    /// config missing `bundle_id` (at build script / proc-macro
    /// expansion time) instead of later at `cargo truce install`.
    #[allow(dead_code, reason = "schema-validity check at deserialize time")]
    pub bundle_id: String,
    #[serde(rename = "crate")]
    pub crate_name: String,
    #[serde(default)]
    pub version: Option<String>,
    #[serde(default)]
    pub fourcc: Option<String>,
    pub category: String,
    #[serde(default)]
    pub au_type: Option<String>,
    #[serde(default)]
    pub au_subtype: Option<String>,
    #[serde(default)]
    pub aax_category: Option<String>,
}

/// Compatibility shim for plugin crates that still ship a `build.rs`
/// from a pre-0.33 scaffold. The current scaffold doesn't generate
/// `build.rs` — `truce::plugin_info!()` reads `truce.toml` directly
/// (and uses `include_bytes!` to register it as a build dep) and
/// `cargo truce install --shell` writes a sidecar at install time
/// instead of baking env vars at compile time.
///
/// What this function does today: emit `cargo:rerun-if-changed=truce.toml`
/// as belt-and-braces alongside the proc-macro's `include_bytes!`
/// tracking. Nothing else — the historical `TRUCE_*` env vars had no
/// remaining consumers in the workspace.
///
/// Will be marked `#[deprecated]` in a future release once any
/// out-of-tree plugins still on a pre-0.33 scaffold have had time to
/// drop their `build.rs`.
pub fn emit_plugin_env() {
    if let Ok(path) = find_truce_toml() {
        println!("cargo:rerun-if-changed={}", path.display());
    }
}

/// Resolve cargo's effective target directory for a given workspace root.
///
/// Honoured in priority order:
/// 1. `CARGO_TARGET_DIR` env var (cargo's documented override; absolute
///    paths used as-is, relative paths anchored at `root`).
/// 2. `[build].target-dir` in `<root>/.cargo/config.toml` (the
///    per-workspace equivalent of the env var; same anchoring rule).
/// 3. Fall back to `<root>/target`.
///
/// Used by runtime callers (cargo-truce, truce-test) to anchor
/// artifact paths against cargo's actual target dir instead of a
/// hardcoded `target/`. Build-script callers that already have
/// `OUT_DIR` should keep using the private `resolve_target_dir()`
/// fallback chain — it can detect the dir from `OUT_DIR` itself
/// without needing a workspace-root anchor.
#[must_use]
pub fn target_dir(root: &Path) -> PathBuf {
    if let Ok(d) = std::env::var("CARGO_TARGET_DIR")
        && !d.is_empty()
    {
        let p = PathBuf::from(&d);
        return if p.is_absolute() { p } else { root.join(p) };
    }
    if let Some(custom) = read_cargo_config_target_dir(root) {
        return if custom.is_absolute() {
            custom
        } else {
            root.join(custom)
        };
    }
    root.join("target")
}

/// Look for `[build].target-dir = "..."` in `<root>/.cargo/config.toml`.
/// Goes through the `toml` crate (already a dep) so dotted-key forms
/// (`build.target-dir = "build"`), inline tables, and commented-out
/// keys are all handled correctly.
fn read_cargo_config_target_dir(root: &Path) -> Option<PathBuf> {
    let cfg = root.join(".cargo").join("config.toml");
    let contents = std::fs::read_to_string(&cfg).ok()?;
    let doc: toml::Table = contents.parse().ok()?;
    let v = doc.get("build")?.get("target-dir")?.as_str()?;
    if v.is_empty() {
        return None;
    }
    Some(PathBuf::from(v))
}

/// Walk up from `CARGO_MANIFEST_DIR` looking for `truce.toml`.
///
/// Returns `Err(message)` rather than panicking so callers in
/// proc-macro contexts can route the message into a `compile_error!`
/// with a span — panicking from a proc macro produces no span and a
/// noisy multi-line error frame.
///
/// # Errors
///
/// Returns `Err` if `CARGO_MANIFEST_DIR` is not set, or if no
/// `truce.toml` is found walking from the manifest dir up to `/`.
pub fn find_truce_toml() -> Result<PathBuf, String> {
    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR")
        .map_err(|_| "CARGO_MANIFEST_DIR not set".to_string())?;
    let manifest_dir = PathBuf::from(manifest_dir);
    let mut dir = manifest_dir.as_path();
    loop {
        let candidate = dir.join("truce.toml");
        if candidate.exists() {
            return Ok(candidate);
        }
        match dir.parent() {
            Some(parent) => dir = parent,
            None => {
                return Err(format!(
                    "truce.toml not found in any ancestor of {}. \
                     Copy truce.toml.example to your workspace root to get started.",
                    manifest_dir.display()
                ));
            }
        }
    }
}

/// Read a `truce.toml` from `path` and parse it.
///
/// Returns `Err(message)` on read or parse failure. Like
/// [`find_truce_toml`], the error form is what makes this safe to call
/// from a proc-macro context.
///
/// # Errors
///
/// Returns `Err(String)` if the file cannot be read or fails to
/// parse as TOML. Both messages include `path` for context.
pub fn load_config(path: &Path) -> Result<Config, String> {
    let content = std::fs::read_to_string(path)
        .map_err(|e| format!("Failed to read {}: {e}", path.display()))?;
    toml::from_str(&content).map_err(|e| format!("Failed to parse {}: {e}", path.display()))
}
