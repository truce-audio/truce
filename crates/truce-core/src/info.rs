/// Static metadata about a plugin.
#[derive(Clone, Debug)]
pub struct PluginInfo {
    pub name: &'static str,
    pub vendor: &'static str,
    pub url: &'static str,
    pub version: &'static str,
    pub category: PluginCategory,

    // Format-specific IDs
    pub vst3_id: &'static str,
    pub clap_id: &'static str,
    pub au_type: [u8; 4],
    pub au_subtype: [u8; 4],
    pub au_manufacturer: [u8; 4],
    pub aax_id: Option<&'static str>,
    /// AAX plugin category string (e.g. "EQ", "Dynamics", "Reverb").
    /// Maps to AAX_ePlugInCategory constants.
    pub aax_category: Option<&'static str>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PluginCategory {
    Effect,
    Instrument,
    Analyzer,
    Tool,
}

/// Convert a category string to [`PluginCategory`] at compile time.
/// Used by the `plugin_info!()` macro.
pub const fn category_from_str(s: &str) -> PluginCategory {
    match s.as_bytes() {
        b"Instrument" => PluginCategory::Instrument,
        b"Analyzer" => PluginCategory::Analyzer,
        b"Tool" => PluginCategory::Tool,
        _ => PluginCategory::Effect,
    }
}

/// Helper to convert a 4-char string literal to `[u8; 4]` at compile time.
/// Panics if the string is not exactly 4 ASCII bytes.
pub const fn fourcc(s: &[u8]) -> [u8; 4] {
    assert!(s.len() == 4, "FourCC must be exactly 4 bytes");
    [s[0], s[1], s[2], s[3]]
}

/// Construct a [`PluginInfo`] from build-time metadata.
///
/// # Zero-arg form (recommended)
///
/// All metadata derived from `truce.toml` + `Cargo.toml`.
/// Requires `truce-build` in `[build-dependencies]` and a `build.rs`:
///
/// ```ignore
/// // build.rs
/// fn main() { truce_build::emit_plugin_env(); }
///
/// // lib.rs
/// fn info() -> PluginInfo { plugin_info!() }
/// ```
///
/// # 6-arg form (explicit)
///
/// ```ignore
/// plugin_info!("My Gain", Effect, "com.myco.gain", "aufx", "MyGn", "MyCo")
/// ```
#[macro_export]
macro_rules! plugin_info {
    // Zero-arg form: everything from env vars set by truce-build + cargo
    () => {
        $crate::PluginInfo {
            name: env!("TRUCE_PLUGIN_NAME"),
            vendor: env!("TRUCE_VENDOR_NAME"),
            url: env!("TRUCE_VENDOR_URL"),
            version: $crate::plugin_info!(@version),
            category: $crate::info::category_from_str(env!("TRUCE_CATEGORY")),
            vst3_id: env!("TRUCE_PLUGIN_ID"),
            clap_id: env!("TRUCE_PLUGIN_ID"),
            au_type: $crate::info::fourcc(env!("TRUCE_AU_TYPE").as_bytes()),
            au_subtype: $crate::info::fourcc(env!("TRUCE_AU_SUBTYPE").as_bytes()),
            au_manufacturer: $crate::info::fourcc(env!("TRUCE_AU_MANUFACTURER").as_bytes()),
            aax_id: None,
            aax_category: option_env!("TRUCE_AAX_CATEGORY"),
        }
    };
    // Version: prefer TRUCE_PLUGIN_VERSION from truce.toml, fall back to CARGO_PKG_VERSION
    (@version) => {
        match option_env!("TRUCE_PLUGIN_VERSION") {
            Some(v) => v,
            None => env!("CARGO_PKG_VERSION"),
        }
    };
    // 6-arg form: explicit, vendor/url/version still from config
    ($name:expr, $cat:ident, $id:expr, $au_type:expr, $au_sub:expr, $au_mfr:expr) => {
        $crate::PluginInfo {
            name: $name,
            vendor: env!("TRUCE_VENDOR_NAME"),
            url: env!("TRUCE_VENDOR_URL"),
            version: $crate::plugin_info!(@version),
            category: $crate::PluginCategory::$cat,
            vst3_id: $id,
            clap_id: $id,
            au_type: $crate::info::fourcc($au_type.as_bytes()),
            au_subtype: $crate::info::fourcc($au_sub.as_bytes()),
            au_manufacturer: $crate::info::fourcc($au_mfr.as_bytes()),
            aax_id: None,
            aax_category: option_env!("TRUCE_AAX_CATEGORY"),
        }
    };
}
