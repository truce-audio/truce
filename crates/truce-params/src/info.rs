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
        const AUTOMATABLE = 0b0001;
        const HIDDEN      = 0b0010;
        const READONLY    = 0b0100;
        const IS_BYPASS   = 0b1000;
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
            Self::Pan | Self::None => "",
        }
    }
}
