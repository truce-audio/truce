use crate::range::ParamRange;

/// Metadata for a single parameter, used by format wrappers.
///
/// `Copy` because every field is POD (`&'static str`, scalars,
/// bitflags, the [`ParamRange`] / [`ParamUnit`] enums). Lets the
/// audio path pass `param_infos[i]` by value without `clone()` noise.
#[derive(Clone, Copy, Debug)]
pub struct ParamInfo {
    pub id: u32,
    pub name: &'static str,
    pub short_name: &'static str,
    pub group: &'static str,
    pub range: ParamRange,
    pub default_plain: f64,
    pub flags: ParamFlags,
    pub unit: ParamUnit,
    /// Which `*Param` type backs this entry. Drives display rounding
    /// (`IntParam` skips fractional digits) and `value_text` parsing,
    /// independently of [`ParamRange`] - a `FloatParam` declared with
    /// `range = "discrete(...)"` should still format as a float, so
    /// inferring kind from range alone is wrong.
    pub kind: ParamValueKind,
    /// Default host MIDI-learn binding: the MIDI message that should
    /// drive this parameter. `None` (the common case) means the host
    /// maps it itself with no plugin hint. Set by `#[param(midi_cc =
    /// …)]` / `#[param(midi_source = …)]`; read by the VST3
    /// `IMidiMapping`, AU parameter-MIDI-mapping, and LV2 `midi:binding`
    /// paths. Ignored by CLAP / VST2 / AAX (the host owns the mapping).
    pub midi_map: Option<MidiSource>,
    /// Optional channel scope for [`Self::midi_map`], as the wire
    /// channel `0..=15`. `None` matches any channel.
    pub midi_channel: Option<u8>,
}

/// The MIDI message a parameter binds to for host MIDI-learn. CCs cover
/// the common case; the non-CC per-channel messages each map onto a
/// VST3 `ControllerNumbers` value, an AU status byte, and an LV2
/// `midi:binding` class.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MidiSource {
    /// Control change, `0..=127`.
    Cc(u8),
    /// Pitch bend.
    PitchBend,
    /// Channel pressure (mono aftertouch).
    ChannelPressure,
    /// Program change.
    ProgramChange,
}

/// Resolve which parameter a MIDI `source` on `channel` is bound to,
/// from a param-info list. The first param whose binding matches the
/// source and whose channel scope is unset (any) or equal wins;
/// `#[derive(Params)]` rejects ambiguous overlaps at compile time, so
/// at most one matches. Used by the format wrappers' mapping paths.
#[must_use]
pub fn map_source_to_param(infos: &[ParamInfo], channel: u8, source: MidiSource) -> Option<u32> {
    infos
        .iter()
        .find(|p| p.midi_map == Some(source) && p.midi_channel.is_none_or(|ch| ch == channel))
        .map(|p| p.id)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info(id: u32, map: Option<MidiSource>, channel: Option<u8>) -> ParamInfo {
        ParamInfo {
            id,
            name: "p",
            short_name: "p",
            group: "",
            range: ParamRange::Linear { min: 0.0, max: 1.0 },
            default_plain: 0.0,
            flags: ParamFlags::AUTOMATABLE,
            unit: ParamUnit::None,
            kind: ParamValueKind::Float,
            midi_map: map,
            midi_channel: channel,
        }
    }

    #[test]
    fn resolves_cc_any_and_scoped_channel() {
        let infos = [
            info(1, Some(MidiSource::Cc(74)), None),    // any channel
            info(2, Some(MidiSource::Cc(71)), Some(0)), // channel 1 (wire 0)
            info(3, Some(MidiSource::PitchBend), None),
        ];
        // Any-channel CC matches whatever channel.
        assert_eq!(map_source_to_param(&infos, 5, MidiSource::Cc(74)), Some(1));
        // Scoped CC matches only its channel.
        assert_eq!(map_source_to_param(&infos, 0, MidiSource::Cc(71)), Some(2));
        assert_eq!(map_source_to_param(&infos, 1, MidiSource::Cc(71)), None);
        // Non-CC source resolves by kind.
        assert_eq!(
            map_source_to_param(&infos, 9, MidiSource::PitchBend),
            Some(3)
        );
        // Unmapped source/CC returns None.
        assert_eq!(map_source_to_param(&infos, 0, MidiSource::Cc(7)), None);
    }
}

/// Which strongly-typed `*Param` constructor produced this
/// [`ParamInfo`]. The `#[derive(Params)]` macro sets it from the
/// field type so format-side code can branch on the original
/// typing without re-deriving it from `range` / `unit`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ParamValueKind {
    Float,
    Int,
    Bool,
    Enum,
}

bitflags::bitflags! {
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub struct ParamFlags: u32 {
        const AUTOMATABLE = 0b0_0001;
        const HIDDEN      = 0b0_0010;
        const READONLY    = 0b0_0100;
        const IS_BYPASS   = 0b0_1000;
        /// This parameter participates in sample-accurate sub-block
        /// chunking: a `ParamChange` event targeting it splits the
        /// audio block at its `sample_offset`. Defaults on; cleared
        /// by `#[param(chunk = false)]` on expensive-to-retarget
        /// params (FFT sizes, lookahead, etc.) where the per-event
        /// fixed cost of subdividing the block outweighs the
        /// sample-accuracy win. Read by
        /// `truce_core::chunked_process::is_split_event`.
        const CHUNKED     = 0b1_0000;
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ParamUnit {
    None,
    Db,
    Hz,
    Milliseconds,
    Seconds,
    Percent,
    Semitones,
    Pan,
    Degrees,
}

impl ParamUnit {
    /// Format-agnostic unit string for host display.
    #[must_use]
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Db => "dB",
            Self::Hz => "Hz",
            Self::Milliseconds => "ms",
            Self::Seconds => "s",
            Self::Percent => "%",
            Self::Semitones => "st",
            Self::Degrees => "°",
            Self::Pan | Self::None => "",
        }
    }
}
