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
pub mod presets;
pub use manifest::{BundleEntry, BundleManifest, host_triple};

/// The canonical plugin ID string baked into `PluginInfo::clap_id` /
/// `PluginInfo::vst3_id` by `truce::plugin_info!()`. Shared between
/// `truce-derive` (compile-time expansion) and `cargo-truce` (which
/// needs the same string at install time to stamp state-envelope
/// hashes into emitted preset files). **The derivation is part of the
/// state wire contract** - every saved session and preset embeds
/// `hash_plugin_id(plugin_id(...))`, so changing this invalidates
/// them all.
#[must_use]
pub fn plugin_id(vendor_id: &str, plugin_name: &str) -> String {
    format!(
        "{}.{}",
        vendor_id,
        plugin_name.to_lowercase().replace(' ', "")
    )
}

/// Resolve a plugin's `(accepts_midi_in, emits_midi)` capability pair
/// from its category and the optional `midi_input` / `midi_output`
/// truce.toml overrides. Shared by `truce-derive` (bakes the result
/// onto `PluginInfo`, which every Rust wrapper reads) and the LV2 TTL
/// emitter, so the host-facing port declarations all agree.
///
/// Defaults: instruments and note effects accept MIDI input; only note
/// effects emit MIDI. `Some(_)` overrides the derived value.
#[must_use]
pub fn midi_capabilities(
    category: &str,
    midi_input: Option<bool>,
    midi_output: Option<bool>,
) -> (bool, bool) {
    let is_note_effect = matches!(category, "midi" | "note_effect");
    let is_instrument = category == "instrument";
    let accepts_midi_in = midi_input.unwrap_or(is_instrument || is_note_effect);
    let emits_midi = midi_output.unwrap_or(is_note_effect);
    (accepts_midi_in, emits_midi)
}

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
    /// chunking layer. Absent -> `AutomationConfig::default()`.
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
    /// Override the category-derived "accepts MIDI input" capability.
    /// `None` (the default) keeps the derived value: `true` for
    /// instruments and note effects. Set `midi_input = true` on an
    /// audio effect that reacts to MIDI, or `false` to suppress an
    /// unwanted MIDI input port.
    #[serde(default)]
    pub midi_input: Option<bool>,
    /// Override the category-derived "emits MIDI output" capability.
    /// `None` (the default) keeps the derived value: `true` for note
    /// effects only. Set `midi_output = true` on an instrument or
    /// effect that also emits MIDI to the host.
    #[serde(default)]
    pub midi_output: Option<bool>,
    /// Optional `[plugin.presets]` table - factory-preset opt-in.
    /// When absent, the install pipeline still picks up a `presets/`
    /// directory next to the plugin crate if one exists.
    #[serde(default)]
    pub presets: Option<PresetsConfig>,
}

/// `[plugin.presets]` - factory-preset emission settings.
#[derive(Deserialize, Debug)]
pub struct PresetsConfig {
    /// Directory of authored `.preset` TOML files, relative to the
    /// plugin's crate directory. Defaults to `presets`.
    #[serde(default = "default_presets_dir")]
    pub factory_dir: String,
    /// Optional override for the `truce/<vendor>/<plugin>` subpath
    /// of the user-scope preset root (e.g. `"Acme/MySynth"`).
    /// Relative segments only; `..` is rejected. Resolves to
    /// `~/Library/Audio/Presets/<user_dir>/` on macOS,
    /// `%APPDATA%\<user_dir>\` on Windows, and
    /// `$XDG_DATA_HOME/truce/<user_dir>/` on Linux. Pick once,
    /// before first release: changing it later orphans saved user
    /// presets.
    #[serde(default)]
    pub user_dir: Option<String>,
}

fn default_presets_dir() -> String {
    "presets".to_string()
}

/// Resolve cargo's effective target directory for a given workspace root.
///
/// Asks cargo via `cargo metadata`, which is the only source that knows
/// whether `root` is a standalone crate or a member of a larger
/// workspace (whose target dir sits at the workspace root, not under
/// `root`). It already folds in `CARGO_TARGET_DIR` and every
/// `.cargo/config.toml` merged from `root` up to the home dir, so its
/// answer is authoritative.
///
/// When cargo can't be run (no cargo on `PATH`, offline tooling), falls
/// back to the overrides we can read ourselves and then `<root>/target`:
/// 1. `CARGO_TARGET_DIR` env var (absolute used as-is, relative anchored
///    at `root`).
/// 2. `[build].target-dir` in `<root>/.cargo/config.toml` (same rule).
/// 3. `<root>/target`.
///
/// Used by runtime callers (cargo-truce, truce-test) to anchor artifact
/// paths against cargo's actual target dir instead of a hardcoded
/// `target/`.
#[must_use]
pub fn target_dir(root: &Path) -> PathBuf {
    if let Some(dir) = cargo_metadata_target_dir(root) {
        return dir;
    }
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

/// Cargo's authoritative `target_directory` for the workspace `root`
/// belongs to. `None` when cargo can't be invoked or the manifest is
/// missing, so the caller can fall back to its own heuristics.
fn cargo_metadata_target_dir(root: &Path) -> Option<PathBuf> {
    let manifest = root.join("Cargo.toml");
    if !manifest.exists() {
        return None;
    }
    // Use the `CARGO` cargo exports for subcommands so we hit the same
    // toolchain it chose (rustup proxies, the Windows toolchain under
    // WSL). Falls back to bare `cargo` for direct invocations.
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let out = std::process::Command::new(cargo)
        .args([
            "metadata",
            "--no-deps",
            "--format-version=1",
            "--manifest-path",
        ])
        .arg(&manifest)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    extract_json_string(&text, "target_directory").map(PathBuf::from)
}

/// Pull a top-level JSON string field out of `cargo metadata` output
/// without pulling in a JSON dependency (the workspace deliberately
/// avoids one in this tier). `target_directory` is emitted once at the
/// top level, so the first match is the right one. Unescapes the string
/// body so Windows paths - whose backslashes arrive doubled as `\\` -
/// round-trip correctly.
fn extract_json_string(json: &str, field: &str) -> Option<String> {
    let needle = format!("\"{field}\":\"");
    let start = json.find(&needle)? + needle.len();
    let mut out = String::new();
    let mut chars = json[start..].chars();
    while let Some(c) = chars.next() {
        match c {
            '"' => return Some(out),
            '\\' => match chars.next()? {
                'n' => out.push('\n'),
                't' => out.push('\t'),
                escaped => out.push(escaped),
            },
            _ => out.push(c),
        }
    }
    None
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

#[cfg(test)]
mod tests {
    use super::{extract_json_string, midi_capabilities};

    #[test]
    fn midi_caps_category_defaults() {
        // (accepts_midi_in, emits_midi)
        assert_eq!(midi_capabilities("note_effect", None, None), (true, true));
        assert_eq!(midi_capabilities("midi", None, None), (true, true));
        assert_eq!(midi_capabilities("instrument", None, None), (true, false));
        assert_eq!(midi_capabilities("effect", None, None), (false, false));
        assert_eq!(midi_capabilities("analyzer", None, None), (false, false));
    }

    #[test]
    fn midi_caps_overrides() {
        // Effect opting into MIDI output (the issue-123 instrument/effect case).
        assert_eq!(midi_capabilities("effect", None, Some(true)), (false, true));
        // Instrument opting into MIDI input override off.
        assert_eq!(
            midi_capabilities("instrument", Some(false), None),
            (false, false)
        );
        // Both forced on an effect.
        assert_eq!(
            midi_capabilities("effect", Some(true), Some(true)),
            (true, true)
        );
    }

    #[test]
    fn extracts_unix_target_directory() {
        let json = r#"{"packages":[],"target_directory":"/work/ws/target","version":1}"#;
        assert_eq!(
            extract_json_string(json, "target_directory").as_deref(),
            Some("/work/ws/target")
        );
    }

    #[test]
    fn unescapes_windows_backslashes() {
        // cargo emits Windows paths with doubled backslashes.
        let json = r#"{"target_directory":"C:\\work\\ws\\target","version":1}"#;
        assert_eq!(
            extract_json_string(json, "target_directory").as_deref(),
            Some(r"C:\work\ws\target")
        );
    }

    #[test]
    fn missing_field_is_none() {
        assert_eq!(
            extract_json_string(r#"{"version":1}"#, "target_directory"),
            None
        );
    }
}
