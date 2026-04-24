//! Proc macros for truce plugins.
//!
//! Provides `plugin_info!()` which reads `truce.toml` at compile time,
//! eliminating the need for a build.rs in every plugin crate.

use proc_macro::TokenStream;
use quote::quote;
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
    #[serde(default)]
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

fn find_truce_toml() -> PathBuf {
    // CARGO_MANIFEST_DIR is available to proc macros at expansion time.
    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR")
        .expect("CARGO_MANIFEST_DIR not set");
    let mut dir = PathBuf::from(&manifest_dir);
    loop {
        let candidate = dir.join("truce.toml");
        if candidate.exists() {
            return candidate;
        }
        if !dir.pop() {
            panic!(
                "truce.toml not found (searched up from {manifest_dir}). \
                 Copy truce.toml.example to get started."
            );
        }
    }
}

fn load_config() -> Config {
    let path = find_truce_toml();
    let content = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("Failed to read {}: {e}", path.display()));
    toml::from_str(&content)
        .unwrap_or_else(|e| panic!("Failed to parse {}: {e}", path.display()))
}

fn find_plugin(config: &Config) -> &PluginDef {
    let pkg_name = std::env::var("CARGO_PKG_NAME")
        .expect("CARGO_PKG_NAME not set");
    config.plugin.iter()
        .find(|p| p.crate_name == pkg_name)
        .unwrap_or_else(|| {
            let available: Vec<_> = config.plugin.iter()
                .map(|p| p.crate_name.as_str())
                .collect();
            panic!(
                "No [[plugin]] entry with crate = \"{pkg_name}\" in truce.toml. \
                 Available: {}", available.join(", ")
            );
        })
}

/// Generate a `PluginInfo` struct literal from `truce.toml`.
///
/// Reads the `[[plugin]]` entry matching the current crate's package name
/// and the `[vendor]` section. No build.rs needed.
///
/// ```ignore
/// fn info() -> PluginInfo {
///     truce::plugin_info!()
/// }
/// ```
#[proc_macro]
pub fn plugin_info(_input: TokenStream) -> TokenStream {
    let config = load_config();
    let plugin = find_plugin(&config);

    let name = &plugin.name;
    let vendor = &config.vendor.name;
    let url = &config.vendor.url;
    let pkg_version = std::env::var("CARGO_PKG_VERSION").unwrap_or_else(|_| "0.1.0".into());
    let version = plugin.version.as_deref().unwrap_or(&pkg_version).to_string();

    // Keep these mappings in sync with `truce-build::emit_plugin_env` and
    // `truce_core::info::category_from_str`. Historically this match only
    // knew about "instrument" and fell everything else through to
    // `Effect` — which silently broke LV2 MIDI for every note-effect
    // plugin because `truce-lv2::derive_port_layout` reads the category
    // to decide whether to open the MIDI input decode path.
    let category = match plugin.category.as_str() {
        "instrument" => quote! { ::truce::core::PluginCategory::Instrument },
        "midi" | "note_effect" => quote! { ::truce::core::PluginCategory::NoteEffect },
        "analyzer" => quote! { ::truce::core::PluginCategory::Analyzer },
        "tool" => quote! { ::truce::core::PluginCategory::Tool },
        _ => quote! { ::truce::core::PluginCategory::Effect },
    };
    // NoteEffect plugins → `aumi` (Apple's MIDI Processor type).
    // Pairs with empty `bus_layouts` at the plugin level: aumi
    // plugins must not expose audio I/O. Logic routes `aumi` to the
    // MIDI FX slot, which is where arpeggiators / transposers /
    // note-shapers belong. Must stay in sync with
    // `truce-xtask/src/config.rs::resolved_au_type` and
    // `truce-build::emit_plugin_env` — a mismatch causes auval
    // "Class Data fields … do not match component description".
    let au_type = plugin.au_type.as_deref().unwrap_or(
        match plugin.category.as_str() {
            "instrument" => "aumu",
            "midi" | "note_effect" => "aumi",
            _ => "aufx",
        }
    );

    let plugin_id = format!(
        "{}.{}",
        config.vendor.id,
        plugin.name.to_lowercase().replace(' ', "")
    );

    let resolved_fourcc = plugin.fourcc.as_ref()
        .or(plugin.au_subtype.as_ref())
        .expect("truce.toml: each [[plugin]] requires `fourcc` or `au_subtype`");
    let au_manufacturer = &config.vendor.au_manufacturer;

    let aax_category = match &plugin.aax_category {
        Some(cat) => quote! { Some(#cat) },
        None => quote! { None },
    };

    let expanded = quote! {
        ::truce::core::PluginInfo {
            name: #name,
            vendor: #vendor,
            url: #url,
            version: #version,
            category: #category,
            vst3_id: #plugin_id,
            clap_id: #plugin_id,
            fourcc: ::truce::core::info::fourcc(#resolved_fourcc.as_bytes()),
            au_type: ::truce::core::info::fourcc(#au_type.as_bytes()),
            au_manufacturer: ::truce::core::info::fourcc(#au_manufacturer.as_bytes()),
            aax_id: None,
            aax_category: #aax_category,
        }
    };

    expanded.into()
}
