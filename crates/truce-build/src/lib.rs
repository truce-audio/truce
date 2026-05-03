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
    /// Required, matching `cargo-truce::PluginDef`. Not consumed by
    /// build/derive but making the field mandatory keeps the schema in
    /// lockstep — a config missing `bundle_id` fails at the earliest
    /// point (build script / proc-macro expansion) rather than later
    /// at `cargo truce install` time.
    #[allow(dead_code)]
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

/// Reads `truce.toml` from the workspace root, finds the `[[plugin]]`
/// entry matching the current crate (via `CARGO_PKG_NAME`), and emits
/// `cargo:rustc-env` directives for the `plugin_info!()` macro.
///
/// Sets:
/// - `TRUCE_PLUGIN_NAME` — display name
/// - `TRUCE_PLUGIN_ID` — `{vendor.id}.{suffix}` (used as CLAP + VST3 ID)
/// - `TRUCE_FOURCC` — 4-char plugin identifier (e.g., "TGan")
/// - `TRUCE_AU_TYPE` — 4-char AU type (e.g., "aufx")
/// - `TRUCE_AU_MANUFACTURER` — 4-char AU manufacturer (e.g., "Trce")
/// - `TRUCE_CATEGORY` — "Effect" or "Instrument" (derived from au_type)
pub fn emit_plugin_env() {
    let toml_path = find_truce_toml_or_exit();
    println!("cargo:rerun-if-changed={}", toml_path.display());

    // Register every feature name the `truce::plugin!` macro expands into,
    // so plugin crates don't get `unexpected_cfgs` warnings for formats
    // they haven't opted in to. Cargo's auto-allow-list only covers
    // features the crate *declares* — but the macro emits one
    // `#[cfg(feature = "…")]` arm per supported format whether the
    // consumer declared it or not.
    println!(
        "cargo:rustc-check-cfg=cfg(feature, values(\"clap\", \"vst3\", \"vst2\", \"lv2\", \"aax\", \"au\", \"shell\"))"
    );

    let config: Config = load_config(&toml_path).unwrap_or_else(|msg| {
        eprintln!("truce-build: {msg}");
        std::process::exit(1);
    });

    let pkg_name = std::env::var("CARGO_PKG_NAME").unwrap();
    let plugin = config
        .plugin
        .iter()
        .find(|p| p.crate_name == pkg_name)
        .unwrap_or_else(|| {
            panic!(
                "No [[plugin]] entry with crate = \"{pkg_name}\" in {}",
                toml_path.display()
            );
        });

    let category = match plugin.category.as_str() {
        "instrument" => "Instrument",
        "midi" | "note_effect" => "NoteEffect",
        _ => "Effect",
    };
    // Keep in sync with `truce-derive::plugin_info` +
    // `cargo-truce/src/config.rs::resolved_au_type`.
    let au_type = plugin
        .au_type
        .as_deref()
        .unwrap_or(match plugin.category.as_str() {
            "instrument" => "aumu",
            "midi" | "note_effect" => "aumi",
            _ => "aufx",
        });
    let plugin_id = format!(
        "{}.{}",
        config.vendor.id,
        plugin.name.to_lowercase().replace(' ', "")
    );

    // Plugin version: from truce.toml if set, otherwise falls back to CARGO_PKG_VERSION
    if let Some(ref ver) = plugin.version {
        println!("cargo:rustc-env=TRUCE_PLUGIN_VERSION={ver}");
    }

    println!("cargo:rustc-env=TRUCE_PLUGIN_NAME={}", plugin.name);
    println!("cargo:rustc-env=TRUCE_PLUGIN_ID={plugin_id}");
    println!("cargo:rustc-env=TRUCE_VENDOR_NAME={}", config.vendor.name);
    println!("cargo:rustc-env=TRUCE_VENDOR_URL={}", config.vendor.url);
    let resolved_fourcc = plugin
        .fourcc
        .as_ref()
        .or(plugin.au_subtype.as_ref())
        .expect("truce.toml: each [[plugin]] requires `fourcc` or `au_subtype`");
    println!("cargo:rustc-env=TRUCE_FOURCC={resolved_fourcc}");
    println!("cargo:rustc-env=TRUCE_AU_TYPE={au_type}");
    println!(
        "cargo:rustc-env=TRUCE_AU_MANUFACTURER={}",
        config.vendor.au_manufacturer
    );
    println!("cargo:rustc-env=TRUCE_CATEGORY={category}");
    if let Some(ref cat) = plugin.aax_category {
        println!("cargo:rustc-env=TRUCE_AAX_CATEGORY={cat}");
    }

    // Bake the resolved cargo target dir + the logic profile into
    // the binary so the `truce::plugin!` shell-mode arm can find the
    // logic dylib at runtime without reading env in the DAW process
    // (DAWs launched from Finder / Spotlight / Start don't inherit
    // shell env; AU v3 sandboxing strips most env vars). The logic
    // profile defaults to "release" — `cargo truce install --shell
    // --debug` overrides it to "debug" by setting the env var
    // `TRUCE_LOGIC_PROFILE` before the cargo build runs.
    if let Some(target_dir) = resolve_target_dir() {
        println!("cargo:rustc-env=TRUCE_TARGET_DIR={}", target_dir.display());
    }
    let logic_profile =
        std::env::var("TRUCE_LOGIC_PROFILE").unwrap_or_else(|_| "release".to_string());
    println!("cargo:rustc-env=TRUCE_LOGIC_PROFILE={logic_profile}");
    println!("cargo:rerun-if-env-changed=TRUCE_LOGIC_PROFILE");
    // Flipping `CARGO_TARGET_DIR` between runs would otherwise leave the
    // baked `TRUCE_TARGET_DIR` stale (cargo rebuilds the proc-macro /
    // build-script crate but not its consumers), so any change in the
    // target-dir env should re-run this script too.
    println!("cargo:rerun-if-env-changed=CARGO_TARGET_DIR");
}

/// Resolve the cargo target directory in a layout-agnostic way.
///
/// Cargo's documented contract for `OUT_DIR` is "a directory under
/// `<target>/<profile>/build/<crate-hash>/`", which the previous
/// `ancestors().nth(4)` walk hard-coded. That breaks under
/// `[unstable.target-dir-per-package]`, custom `[profile.<name>]`
/// names, and Bazel-style split target directories where the relative
/// nesting differs.
///
/// The robust strategy is two-step:
/// 1. Prefer `CARGO_TARGET_DIR` if set — when the user explicitly
///    routed cargo to a custom target dir, that env var is the
///    authoritative answer regardless of how `OUT_DIR` looks.
/// 2. Otherwise, walk up `OUT_DIR`'s ancestors looking for an entry
///    literally named `target`. Falls back to `None` (so we skip
///    emitting the env var) rather than baking a wrong path.
fn resolve_target_dir() -> Option<PathBuf> {
    if let Ok(d) = std::env::var("CARGO_TARGET_DIR") {
        let p = PathBuf::from(d);
        if !p.as_os_str().is_empty() {
            return Some(p);
        }
    }
    let out_dir = std::env::var("OUT_DIR").ok()?;
    std::path::Path::new(&out_dir)
        .ancestors()
        .find(|a| a.file_name().is_some_and(|n| n == "target"))
        .map(PathBuf::from)
}

/// Walk up from `CARGO_MANIFEST_DIR` looking for `truce.toml`.
///
/// Returns `Err(message)` rather than panicking so callers in
/// proc-macro contexts can route the message into a `compile_error!`
/// with a span — panicking from a proc macro produces no span and a
/// noisy multi-line error frame. Build scripts call
/// [`find_truce_toml_or_exit`] for the convenient cleanly-exit form.
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

/// Build-script convenience wrapper around [`find_truce_toml`].
///
/// On miss we exit cleanly with a one-line message instead of
/// `panic!`-ing — panicking from a build script dumps a multi-line
/// `RUST_BACKTRACE` stack trace through cargo's "warning:" framing and
/// buries the actually-useful "copy truce.toml.example" hint at the
/// bottom.
fn find_truce_toml_or_exit() -> PathBuf {
    find_truce_toml().unwrap_or_else(|msg| {
        eprintln!("truce-build: {msg}");
        std::process::exit(1);
    })
}

/// Read a `truce.toml` from `path` and parse it.
///
/// Returns `Err(message)` on read or parse failure. Like
/// [`find_truce_toml`], the error form is what makes this safe to call
/// from a proc-macro context.
pub fn load_config(path: &Path) -> Result<Config, String> {
    let content = std::fs::read_to_string(path)
        .map_err(|e| format!("Failed to read {}: {e}", path.display()))?;
    toml::from_str(&content).map_err(|e| format!("Failed to parse {}: {e}", path.display()))
}
