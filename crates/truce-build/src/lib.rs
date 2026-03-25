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
use std::path::PathBuf;

#[derive(Deserialize)]
struct Config {
    vendor: VendorConfig,
    plugin: Vec<PluginDef>,
}

#[derive(Deserialize)]
struct VendorConfig {
    name: String,
    id: String,
    #[serde(default)]
    url: String,
    au_manufacturer: String,
}

#[derive(Deserialize)]
struct PluginDef {
    name: String,
    #[serde(rename = "crate")]
    crate_name: String,
    #[serde(default)]
    version: Option<String>,
    #[serde(default)]
    fourcc: Option<String>,
    category: String,
    #[serde(default)]
    au_type: Option<String>,
    #[serde(default)]
    au_subtype: Option<String>,
    #[serde(default)]
    aax_category: Option<String>,
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
    let toml_path = find_truce_toml();
    println!("cargo:rerun-if-changed={}", toml_path.display());

    let content = std::fs::read_to_string(&toml_path).unwrap_or_else(|e| {
        panic!("Failed to read {}: {e}", toml_path.display());
    });
    let config: Config = toml::from_str(&content).unwrap_or_else(|e| {
        panic!("Failed to parse {}: {e}", toml_path.display());
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
        _ => "Effect",
    };
    let au_type = plugin.au_type.as_deref().unwrap_or(
        match plugin.category.as_str() {
            "instrument" => "aumu",
            _ => "aufx",
        }
    );
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
    let resolved_fourcc = plugin.fourcc.as_ref()
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
}

fn find_truce_toml() -> PathBuf {
    // Walk up from CARGO_MANIFEST_DIR to find truce.toml
    let manifest_dir = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    let mut dir = manifest_dir.as_path();
    loop {
        let candidate = dir.join("truce.toml");
        if candidate.exists() {
            return candidate;
        }
        dir = match dir.parent() {
            Some(parent) => parent,
            None => panic!("truce.toml not found. Copy truce.toml.example to get started."),
        };
    }
}
