//! Helpers shared across format wrappers (CLAP, VST3, VST2, AU, AAX, LV2).
//!
//! Each wrapper still owns its format-specific descriptor types and
//! callback tables — those don't unify cleanly. What unifies is the
//! "boring" boundary glue: building CStrings from `ParamInfo` fields,
//! picking the default bus layout, and resolving install-time name
//! overrides (see also [`crate::info::resolve_name_override`]).
//!
//! Each helper is a single small function so the wrappers stay
//! greppable — the per-format vtable construction code reads as
//! "for each param, get cstrings, build descriptor" without inlined
//! `CString::new(...).unwrap_or_default()` boilerplate.
//!
//! Adding a new format wrapper? Reach for these first; only fall back
//! to direct `CString::new` etc. when the format genuinely needs
//! something none of the other formats does.

use std::ffi::CString;

use truce_params::ParamInfo;

use crate::bus::BusLayout;
use crate::export::PluginExport;

/// CStrings derived from a single `ParamInfo`. All four conversions
/// follow the same pattern (`unwrap_or_default()` so a `\0` in metadata
/// degrades to an empty C string instead of panicking the host); pulling
/// them into one struct keeps the per-format vtable loops uniform.
pub struct ParamCStrings {
    pub name: CString,
    pub short_name: CString,
    pub unit: CString,
    pub group: CString,
}

impl ParamCStrings {
    /// Build all four CStrings for one parameter.
    pub fn from_info(info: &ParamInfo) -> Self {
        Self {
            name: CString::new(info.name).unwrap_or_default(),
            short_name: CString::new(info.short_name).unwrap_or_default(),
            unit: CString::new(info.unit.as_str()).unwrap_or_default(),
            group: CString::new(info.group).unwrap_or_default(),
        }
    }
}

/// `(input_channels, output_channels)` for the plugin's default bus
/// layout. Falls back to `(0, 2)` (stereo-out, no input — the most
/// useful default for instruments / generators) when the plugin
/// declares no layouts. Used by every format's vtable / descriptor
/// to advertise channel counts at registration time.
///
/// **Note for `aumi` (MIDI processor) plugins:** the convention is
/// `bus_layouts: [BusLayout::new()]`, which has zero input *and* zero
/// output channels. This helper returns `(0, 0)` for that case — which
/// is correct for AU (the AU shim's `channelCapabilities` returns
/// `[0, 0]` and the host treats the plugin as MIDI-only) but **wrong
/// for AAX**, which requires every plugin to advertise at least
/// stereo audio I/O. AAX maps `(0, 0)` → `(2, 2)` (synthesizing a
/// stereo passthrough) inside `truce-aax::register_aax` after this
/// helper returns. Don't push that remap into this helper — only AAX
/// needs it.
pub fn default_io_channels<P: PluginExport>() -> (u32, u32) {
    P::bus_layouts()
        .first()
        .map(|l| (l.total_input_channels(), l.total_output_channels()))
        .unwrap_or((0, 2))
}

/// Pick the plugin's first bus layout, or panic with a clear message.
/// Used by wrappers (AAX, VST2) that need to read the layout *before*
/// host-side bus-config negotiation (so a missing layout is a static
/// plugin-author bug — clearer to fail loudly at registration than
/// silently misreport channel counts).
///
/// For `aumi` plugins the returned layout is typically `BusLayout::new()`
/// (zero in / zero out). AAX synthesizes (2, 2) from that case in
/// `register_aax`; see [`default_io_channels`] for the rationale.
pub fn first_bus_layout<P: PluginExport>() -> BusLayout {
    P::bus_layouts()
        .into_iter()
        .next()
        .expect("plugin must declare at least one bus layout in `Plugin::bus_layouts()`")
}
