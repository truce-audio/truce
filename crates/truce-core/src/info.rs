/// Static metadata about a plugin.
#[derive(Clone, Debug)]
pub struct PluginInfo {
    pub name: &'static str,
    pub vendor: &'static str,
    pub url: &'static str,
    pub version: &'static str,
    pub category: PluginCategory,

    /// Short identifier (`bundle_id` in `truce.toml`). Used to derive
    /// the LV2 plugin URI (`{vendor.url}/lv2/{bundle_id}`); also a
    /// stable, vendor-agnostic key for "this plugin" that doesn't
    /// drift with display-name changes the way `clap_id` does.
    pub bundle_id: &'static str,

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

    /// Per-format display-name overrides - populated by
    /// `truce::plugin_info!()` from the matching `truce.toml` keys.
    /// Format wrappers fall back to `name` when the override is `None`.
    /// Baked at compile time so cargo-truce no longer needs to pass
    /// `TRUCE_<FORMAT>_NAME_OVERRIDE` env vars (which used to
    /// invalidate the format wrapper's fingerprint between back-to-back
    /// plugin builds with different overrides).
    ///
    /// `au3_name` is exposed for parity with the other formats and
    /// for user introspection, but `truce-au`'s `resolved_plugin_name`
    /// reads `au_name` for both v2 and v3 builds - the v3 host's
    /// displayed label comes from the appex `Info.plist`'s `AUNAME`
    /// (which `cargo truce install --au3` populates from `au3_name`),
    /// not from `g_descriptor->name`.
    pub vst3_name: Option<&'static str>,
    pub clap_name: Option<&'static str>,
    pub vst2_name: Option<&'static str>,
    pub au_name: Option<&'static str>,
    pub au3_name: Option<&'static str>,
    pub aax_name: Option<&'static str>,
    pub lv2_name: Option<&'static str>,

    /// Standalone-only. Format wrappers MUST NOT read this - it
    /// exists for preview hosts (truce-standalone, the iOS `AUv3`
    /// container app) that need a TOML-driven way to mute the
    /// plug-in's audio output while keeping `process()` ticking, so
    /// editors that visualise an input signal (analyzers, tuners,
    /// spectrum displays) update from mic / file input without
    /// closing a mic → speakers feedback loop. Set from
    /// `mute_preview_output` in `truce.toml`. Real DAW hosts own
    /// their own output graph; consulting this flag from a wrapper
    /// would let plug-in authors silence the DAW's mix bus, which
    /// is never what they want.
    #[doc(hidden)]
    pub mute_preview_output: bool,
}

/// Resolve a format's display-name override. Each wrapper picks its
/// own `<format>_name` field off `PluginInfo` and passes the result
/// here along with the `PluginInfo::name` fallback. Empty overrides
/// (unset or set to `""`) fall through to `fallback`.
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
/// or at runtime if `s.len() != 4`. ASCII-ness isn't checked here -
/// callers that need it should validate separately.
#[must_use]
pub const fn fourcc(s: &[u8]) -> [u8; 4] {
    assert!(s.len() == 4, "FourCC must be exactly 4 bytes");
    [s[0], s[1], s[2], s[3]]
}
