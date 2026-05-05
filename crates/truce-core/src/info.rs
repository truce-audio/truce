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
    pub fourcc: [u8; 4],
    pub au_type: [u8; 4],
    pub au_manufacturer: [u8; 4],
    pub aax_id: Option<&'static str>,
    /// AAX plugin category string (e.g. "EQ", "Dynamics", "Reverb").
    /// Maps to `AAX_ePlugInCategory` constants.
    pub aax_category: Option<&'static str>,
}

/// Resolve a format's install-time display-name override. Each wrapper
/// reads its own `TRUCE_{FORMAT}_NAME_OVERRIDE` via `option_env!` and
/// passes the result here along with the `PluginInfo::name` fallback.
/// Empty overrides (unset or set to `""`) fall through to `fallback`.
#[must_use]
pub fn resolve_name_override(
    override_value: Option<&'static str>,
    fallback: &'static str,
) -> &'static str {
    match override_value {
        Some(s) if !s.is_empty() => s,
        _ => fallback,
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PluginCategory {
    Effect,
    Instrument,
    /// MIDI note effect (e.g., transpose, arpeggiator). Processes MIDI events.
    NoteEffect,
    Analyzer,
    Tool,
}

/// Convert a category string to [`PluginCategory`] at compile time.
/// Used by the `plugin_info!()` macro.
#[must_use]
pub const fn category_from_str(s: &str) -> PluginCategory {
    match s.as_bytes() {
        b"Instrument" => PluginCategory::Instrument,
        b"NoteEffect" => PluginCategory::NoteEffect,
        b"Analyzer" => PluginCategory::Analyzer,
        b"Tool" => PluginCategory::Tool,
        _ => PluginCategory::Effect,
    }
}

/// Helper to convert a 4-char string literal to `[u8; 4]` at compile time.
/// Panics if the string is not exactly 4 ASCII bytes.
///
/// # Panics
///
/// Panics at compile time when used in a `const` context (preferred)
/// or at runtime if `s.len() != 4`. ASCII-ness isn't checked here —
/// callers that need it should validate separately.
#[must_use]
pub const fn fourcc(s: &[u8]) -> [u8; 4] {
    assert!(s.len() == 4, "FourCC must be exactly 4 bytes");
    [s[0], s[1], s[2], s[3]]
}

// The historical `truce_core::plugin_info!` macro_rules form lived
// here and was driven by `TRUCE_*` env vars emitted by
// `truce-build::emit_plugin_env()`. The proc-macro version
// (`truce_derive::plugin_info`, re-exported through the prelude as
// `truce::plugin_info!`) reads `truce.toml` directly at compile time
// — no build.rs, no env-var hop — and is what every plugin and every
// in-tree example uses. The macro_rules form had no remaining
// callers and its env-var schema was about to be deleted by the
// "drop build.rs from the scaffold" change, so it's gone.
