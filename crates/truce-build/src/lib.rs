#![forbid(unsafe_code)]

//! Build-time schema + target-dir helpers shared by `truce-derive`
//! (proc macros) and `cargo-truce` (install / build pipeline).
//!
//! Plugin crates do not need a `build.rs` - `truce::plugin_info!()`
//! reads `truce.toml` directly at compile time and tracks it via
//! `include_bytes!`.

use serde::Deserialize;
use std::path::{Path, PathBuf};

pub mod lv2;
pub mod manifest;
pub use manifest::{BundleEntry, BundleManifest, host_triple};

/// Derive-time view of `truce.toml`.
///
/// `truce-derive` (proc macros) reads this to expand
/// `plugin_info!()` at compile time. `cargo-truce` has its own richer
/// `PluginDef` that adds install-time fields (per-format display
/// names, AU3 subtype, packaging, …); the install path needs that
/// detail, but derive doesn't, so cargo-truce's shape is intentionally
/// a superset rather than a duplicate of this one.
#[derive(Deserialize, Debug)]
pub struct Config {
    pub vendor: VendorConfig,
    pub plugin: Vec<PluginDef>,
    /// Optional `[automation]` table - tunes the sample-accurate
    /// chunking layer. Absent -> `AutomationConfig::default()`. See
    /// `truce-docs/docs/internal/parameter-dependent-chunking.md`.
    #[serde(default)]
    pub automation: AutomationConfig,
}

/// Sample-accurate automation chunking tunables.
///
/// Read by every format wrapper at instantiate time and passed to
/// `truce_core::chunked_process::process_chunked` to drive the
/// sub-block splitting decision.
#[derive(Deserialize, Debug, Clone, Copy)]
#[serde(default)]
pub struct AutomationConfig {
    /// Smallest sub-block size in samples. Sub-blocks shorter than
    /// this are coalesced with the next event (the smoother target
    /// is set at `block_start` instead of at the event sample).
    /// Default 32 fits typical SIMD widths and avoids paying per-block
    /// fixed costs on dense automation. Set to 1 for "true" sample
    /// accuracy. Set above the host's max block size to disable
    /// splitting entirely (the chunker still runs, but never finds a
    /// split point and falls back to one `process()` call per block,
    /// equivalent to the pre-chunking behavior).
    pub min_subblock_samples: u32,
}

impl Default for AutomationConfig {
    fn default() -> Self {
        Self {
            min_subblock_samples: 32,
        }
    }
}

#[derive(Deserialize, Debug)]
pub struct VendorConfig {
    pub name: String,
    pub id: String,
    #[serde(default)]
    pub url: String,
    pub au_manufacturer: String,
}

/// Shared TOML schema for a `[[plugin]]` entry.
///
/// Lives in `truce-build` so both the `#[derive(Params)]` /
/// `plugin_info!()` proc macros and `cargo-truce`'s install-time
/// logic read the same definition. Install-time tooling extends this
/// with extra fields like `au3_subtype` / `au_tag` via
/// `#[serde(flatten)]`.
#[derive(Deserialize, Debug)]
pub struct PluginDef {
    pub name: String,
    /// Required, matching `cargo-truce::PluginDef`. Deliberately unread
    /// after deserialization: presence makes serde fail early on a
    /// config missing `bundle_id` (at proc-macro expansion time)
    /// instead of later at `cargo truce install`.
    #[allow(dead_code, reason = "schema-validity check at deserialize time")]
    pub bundle_id: String,
    #[serde(rename = "crate")]
    pub crate_name: String,
    #[serde(default)]
    pub version: Option<String>,
    /// User-facing one-paragraph description shown in distribution
    /// surfaces - the iOS container app's "About" pane, App Store
    /// description, generated docs. Optional; absent → callers
    /// generate a category-aware default ("A truce effect", "A
    /// truce instrument", …).
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub fourcc: Option<String>,
    pub category: String,
    #[serde(default)]
    pub au_type: Option<String>,
    #[serde(default)]
    pub au_subtype: Option<String>,
    #[serde(default)]
    pub aax_category: Option<String>,
    /// VST3 "Plugin Type Categories" secondary token. The wrapper
    /// emits this after the primary token (`Fx|<sub>`,
    /// `Instrument|<sub>`) so hosts like Cubase can route the
    /// plugin into the right submenu instead of falling back to
    /// "Other". Values from the VST3 SDK's published list — common
    /// effects: `Delay`, `Distortion`, `Dynamics`, `EQ`, `Filter`,
    /// `Mastering`, `Modulation`, `Pitch Shift`, `Restoration`,
    /// `Reverb`, `Analyzer`, `Tools`, `Surround`. Optional; when
    /// omitted the wrapper ships just the primary token (and Cubase
    /// will bucket the plugin under "Other").
    #[serde(default)]
    pub vst3_subcategory: Option<String>,
    // Per-format display-name overrides. Surface a different plugin
    // name in a specific host's plugin browser without changing
    // `name` (which other formats and the bundle path continue to
    // use). Embedded into `PluginInfo` at proc-macro expansion time
    // so the value is part of the rlib.
    #[serde(default)]
    pub vst3_name: Option<String>,
    #[serde(default)]
    pub clap_name: Option<String>,
    #[serde(default)]
    pub vst2_name: Option<String>,
    #[serde(default)]
    pub au_name: Option<String>,
    #[serde(default)]
    pub au3_name: Option<String>,
    #[serde(default)]
    pub aax_name: Option<String>,
    #[serde(default)]
    pub lv2_name: Option<String>,
    /// Silence the audio output in *preview* hosts (truce-standalone,
    /// the iOS `AUv3` container app). `process()` keeps running on the
    /// usual cadence so plug-ins whose editor visualises an input
    /// signal (analyzers, tuners, spectrum displays) still update -
    /// but the signal never reaches the speakers, so a mic-fed analyzer
    /// doesn't form a feedback loop with its own loopback. Real DAW
    /// hosts ignore this flag; they own their own output graph.
    #[serde(default)]
    pub mute_preview_output: bool,
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
/// hardcoded `target/`.
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
/// with a span - panicking from a proc macro produces no span and a
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
