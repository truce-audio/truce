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
    au_type: String,
    au_subtype: String,
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

    let category = match plugin.au_type.as_str() {
        "aumu" => quote! { ::truce::core::PluginCategory::Instrument },
        _ => quote! { ::truce::core::PluginCategory::Effect },
    };

    let plugin_id = format!(
        "{}.{}",
        config.vendor.id,
        plugin.name.to_lowercase().replace(' ', "")
    );

    let au_type = &plugin.au_type;
    let au_subtype = &plugin.au_subtype;
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
            au_type: ::truce::core::info::fourcc(#au_type.as_bytes()),
            au_subtype: ::truce::core::info::fourcc(#au_subtype.as_bytes()),
            au_manufacturer: ::truce::core::info::fourcc(#au_manufacturer.as_bytes()),
            aax_id: None,
            aax_category: #aax_category,
        }
    };

    expanded.into()
}
