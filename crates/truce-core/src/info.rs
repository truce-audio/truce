/// Wire dialect a MIDI port speaks. `Midi1` is the default and covers
/// every format today; `Midi2` opts a port into MIDI 2.0 / UMP so the
/// plugin receives the native 16/32-bit + per-note + group-addressed
/// variants of [`crate::events::EventBody`] instead of the MIDI 1.0
/// down-conversion. Formats with a UMP transport (CLAP, AU v3) honor
/// `Midi2` both ways; VST3 carries the per-note subset via note
/// expression; VST2 / AU v2 / AAX / LV2 clamp to MIDI 1.0.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Default)]
pub enum MidiDialect {
    #[default]
    Midi1,
    Midi2,
}

/// Static metadata about a plugin.
#[derive(Clone, Debug)]
pub struct PluginInfo {
    pub name: &'static str,
    pub vendor: &'static str,
    pub url: &'static str,
    pub version: &'static str,
    pub category: PluginCategory,

    /// Whether the host should route MIDI / note events *into* this
    /// plugin. Defaults to `true` for instruments and note effects;
    /// `truce.toml`'s `midi_input` overrides the derived value (e.g.
    /// an audio effect that reacts to CC). Every format wrapper gates
    /// its MIDI input port / bus / capability on this one flag.
    pub accepts_midi_in: bool,

    /// Whether this plugin emits MIDI / note events *to* the host.
    /// Defaults to `true` for note effects only; `truce.toml`'s
    /// `midi_output` overrides the derived value (e.g. an instrument
    /// or effect that also emits MIDI). Every format wrapper gates its
    /// MIDI output port / bus / capability on this one flag, so the
    /// host actually reads what `process()` pushes to `output_events`.
    pub emits_midi: bool,

    /// Dialect the (single) MIDI input port speaks. Defaults to
    /// [`MidiDialect::Midi1`]; a plugin opts into MIDI 2.0 with the
    /// `midi2` key in `truce.toml`. Honored by the UMP-transport formats
    /// (CLAP, AU v3); VST3 maps the per-note subset to note expression;
    /// the rest deliver MIDI 1.0 regardless.
    pub midi_input_dialect: MidiDialect,

    /// Dialect the (single) MIDI output port speaks. See
    /// [`Self::midi_input_dialect`].
    pub midi_output_dialect: MidiDialect,

    /// Number of MIDI input ports the plugin exposes. `0` when it
    /// accepts no MIDI, `1` for the ordinary single-port case, `>1` for
    /// a multi-port plugin (e.g. a merger). Derived from the MIDI-input
    /// capability by default; `midi_input_ports` in `truce.toml` raises
    /// it. Formats without a multi-port MIDI transport clamp to `1` and
    /// route everything to [`crate::Event::port`] `0`. Always `>= 1` when
    /// [`Self::accepts_midi_in`] is set, and `0` otherwise.
    pub midi_input_ports: u8,

    /// Number of MIDI output ports the plugin exposes. See
    /// [`Self::midi_input_ports`]; mirrors [`Self::emits_midi`].
    pub midi_output_ports: u8,

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
    /// VST3 "Plugin Type Categories" secondary token. The wrapper
    /// emits this after the primary token (`Fx|<sub>`,
    /// `Instrument|<sub>`) so hosts like Cubase route to the right
    /// submenu. Values from the VST3 SDK list: `Delay`, `Distortion`,
    /// `Dynamics`, `EQ`, `Filter`, `Mastering`, `Modulation`,
    /// `Pitch Shift`, `Restoration`, `Reverb`, `Analyzer`, `Tools`,
    /// `Surround`. Optional; when `None` only the primary token is
    /// emitted and Cubase will fall back to "Other".
    pub vst3_subcategory: Option<&'static str>,

    /// Per-format display-name overrides, populated by
    /// `truce::plugin_info!()` from the matching `truce.toml` keys.
    /// Format wrappers fall back to `name` when the override is `None`.
    /// Baked at compile time so back-to-back plugin builds with
    /// different overrides don't invalidate the format wrapper's
    /// build fingerprint.
    ///
    /// `au3_name` is exposed for parity with the other formats and
    /// for user introspection, but `truce-au`'s `resolved_plugin_name`
    /// reads `au_name` for both v2 and v3 builds - the v3 host's
    /// displayed label comes from the appex `Info.plist`'s `AUNAME`
    /// (which `cargo truce install --au3` populates from `au3_name`),
    /// not from `g_descriptor->name`.
    /// `[plugin.presets]` `user_dir` from `truce.toml`: replaces the
    /// `truce/<vendor>/<plugin>` subpath of the user-scope preset
    /// root. `truce_utils::presets::user_preset_root` documents
    /// where the path resolves on each OS. Effectively permanent
    /// once a plugin ships - changing it orphans previously saved
    /// user presets and packs.
    pub preset_user_dir: Option<&'static str>,

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

    /// Sample-accurate automation chunking tunables. Read by the
    /// `chunked_process::process_chunked` helper that every format
    /// wrapper routes `process()` through. Populated by
    /// `truce::plugin_info!()` from `truce.toml`'s `[automation]`
    /// table; defaults to [`AutomationConfig::DEFAULT`] when the
    /// table is absent.
    pub automation: AutomationConfig,

    /// AU `ClassInfo` dictionary keys a pre-truce build stored its
    /// state under. Probed by `truce-au` when truce's own data key is
    /// absent; a hit feeds the plugin's `migrate_state` hook. From
    /// `truce.toml`'s `[plugin.legacy_state]` `au_keys`; empty when
    /// undeclared (no probing).
    pub legacy_au_keys: &'static [&'static str],
    /// LV2 state property URIs a pre-truce build stored its state
    /// under. See [`Self::legacy_au_keys`].
    pub legacy_lv2_uris: &'static [&'static str],
    /// AAX chunk fourccs a pre-truce build stored its state under.
    /// See [`Self::legacy_au_keys`].
    pub legacy_aax_chunk_ids: &'static [&'static str],
}

/// Sample-accurate chunking tunables baked into [`PluginInfo`] at
/// compile time. Mirrors `truce_build::AutomationConfig` (the
/// derive-time view of the same TOML key) but lives in `truce-core`
/// so wrappers can read it without a `truce-build` dep.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AutomationConfig {
    /// Smallest sub-block size in samples. The chunker only splits
    /// the audio block at split-eligible events whose `sample_offset`
    /// is at least `block_start + min_subblock_samples` past the
    /// current sub-block start; closer events are coalesced. Default
    /// 32 (set via [`AutomationConfig::DEFAULT`]).
    pub min_subblock_samples: u32,
}

impl AutomationConfig {
    /// Default used when `truce.toml` omits the `[automation]` table.
    pub const DEFAULT: Self = Self {
        min_subblock_samples: 32,
    };
}

impl Default for AutomationConfig {
    fn default() -> Self {
        Self::DEFAULT
    }
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
