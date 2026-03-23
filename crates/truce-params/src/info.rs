use crate::range::ParamRange;

/// Metadata for a single parameter, used by format wrappers.
#[derive(Clone, Debug)]
pub struct ParamInfo {
    pub id: u32,
    pub name: &'static str,
    pub short_name: &'static str,
    pub group: &'static str,
    pub range: ParamRange,
    pub default_plain: f64,
    pub flags: ParamFlags,
    pub unit: ParamUnit,
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
