//! Audio bus layouts: the fixed set of layouts a plugin offers its host.
//!
//! A host negotiates a layout from this set before activation (VST3
//! `setBusArrangements`, CLAP's audio-ports configs, AU stream formats);
//! AAX registers one plugin type per layout at scan time and LV2 declares
//! its ports in the bundle's TTL. None of them changes the offered set at
//! runtime, so it's `const` data: `PluginLogic::BUS_LAYOUTS`. Being const
//! is also what lets `truce::plugin!` embed the set in the built binary
//! (see [`encode`]), where a package-time tool can read it without running
//! the plugin.

use std::fmt;
use std::ops::Deref;

use truce_utils::bus_table;

/// Most buses a [`BusLayout`] holds per direction. Inline storage is what
/// lets layouts be built in `const`; 32 covers a main bus plus a sampler's
/// worth of aux outputs.
pub const MAX_BUSES: usize = 32;

/// Describes the audio bus configuration of a plugin.
///
/// By convention, the **first** input bus is the main audio in
/// (effects + analyzers) and any subsequent input buses are sidechain
/// inputs. The first output bus is the main audio out. Format
/// wrappers (CLAP / VST3 / AU / AAX / LV2) rely on this ordering when
/// they translate into format-specific main/aux bus designations, and
/// `BusConfig::kind` lets call-sites that need it ask the bus
/// directly rather than re-deriving the convention.
///
/// Build one with [`Self::new`] / [`Self::mono`] / [`Self::stereo`] and the
/// `with_*` builders, all `const fn`, so a plugin's layouts are a `const`:
///
/// ```ignore
/// const BUS_LAYOUTS: &'static [BusLayout] = &[
///     BusLayout::stereo().with_sidechain_input("Sidechain", ChannelConfig::Stereo),
/// ];
/// ```
#[derive(Clone, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct BusLayout {
    pub inputs: Buses,
    pub outputs: Buses,
}

/// A layout's buses in one direction. Reads as a `&[BusConfig]` (it
/// derefs to one), stored inline so a layout can be built in `const`.
#[derive(Clone, Copy)]
pub struct Buses {
    items: [BusConfig; MAX_BUSES],
    len: usize,
}

impl Buses {
    const EMPTY: Self = Self {
        items: [BusConfig::PLACEHOLDER; MAX_BUSES],
        len: 0,
    };

    const fn push(mut self, bus: BusConfig) -> Self {
        assert!(
            self.len < MAX_BUSES,
            "a BusLayout holds at most MAX_BUSES (32) buses per direction"
        );
        self.items[self.len] = bus;
        self.len += 1;
        self
    }

    /// The buses, as a slice. `const`, unlike the `Deref` that gives the
    /// same slice at runtime.
    #[must_use]
    pub const fn as_slice(&self) -> &[BusConfig] {
        self.items.split_at(self.len).0
    }
}

impl Default for Buses {
    fn default() -> Self {
        Self::EMPTY
    }
}

impl Deref for Buses {
    type Target = [BusConfig];
    fn deref(&self) -> &[BusConfig] {
        self.as_slice()
    }
}

impl<'a> IntoIterator for &'a Buses {
    type Item = &'a BusConfig;
    type IntoIter = std::slice::Iter<'a, BusConfig>;
    fn into_iter(self) -> Self::IntoIter {
        self.iter()
    }
}

impl PartialEq for Buses {
    fn eq(&self, other: &Self) -> bool {
        self.as_slice() == other.as_slice()
    }
}

impl Eq for Buses {}

impl fmt::Debug for Buses {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_list().entries(self.iter()).finish()
    }
}

/// Constructed by [`BusLayout`]'s `with_*` builders. Marked
/// `#[non_exhaustive]` to keep the struct literal as a private
/// detail of the builder methods.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct BusConfig {
    pub name: &'static str,
    pub channels: ChannelConfig,
    pub kind: BusKind,
}

impl BusConfig {
    /// Fills the unused slots of a [`Buses`]; never visible through it.
    const PLACEHOLDER: Self = Self {
        name: "",
        channels: ChannelConfig::Mono,
        kind: BusKind::Main,
    };
}

/// Whether a bus is the plugin's main audio I/O or a secondary
/// sidechain / aux bus. Format wrappers use this to set the
/// per-bus role flag the host expects (`kBusType_Main` /
/// `kBusType_Aux` in VST3, `is_sidechain` in CLAP, etc.).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BusKind {
    Main,
    Sidechain,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChannelConfig {
    Mono,
    Stereo,
    Custom(u32),
}

impl ChannelConfig {
    #[must_use]
    pub const fn channel_count(&self) -> u32 {
        match self {
            Self::Mono => 1,
            Self::Stereo => 2,
            Self::Custom(n) => *n,
        }
    }
}

impl BusLayout {
    /// The default audio-effect layout set: stereo and mono (stereo
    /// first, so it's the default width). Use it as `BUS_LAYOUTS` for an
    /// ordinary in/out effect and the host offers it on both stereo
    /// and mono tracks, instead of a stereo-only effect that's hidden on
    /// mono ones. The effect's `process` must handle either width - loop
    /// over `buffer.channels()` rather than assuming two.
    pub const STEREO_AND_MONO: &'static [Self] = &[Self::stereo(), Self::mono()];

    /// The output-only counterpart of [`Self::STEREO_AND_MONO`]: stereo and
    /// mono output buses with no input, for an instrument that produces
    /// audio from MIDI. Offered on both stereo and mono tracks. The
    /// instrument's `process` must handle either output width - guard any
    /// write past the first channel with `buffer.num_output_channels()`.
    pub const STEREO_AND_MONO_OUTPUT: &'static [Self] = &[
        Self::new().with_output("Main", ChannelConfig::Stereo),
        Self::new().with_output("Main", ChannelConfig::Mono),
    ];

    #[must_use]
    pub const fn new() -> Self {
        Self {
            inputs: Buses::EMPTY,
            outputs: Buses::EMPTY,
        }
    }

    #[must_use]
    pub const fn mono() -> Self {
        Self::new()
            .with_input("Main", ChannelConfig::Mono)
            .with_output("Main", ChannelConfig::Mono)
    }

    #[must_use]
    pub const fn stereo() -> Self {
        Self::new()
            .with_input("Main", ChannelConfig::Stereo)
            .with_output("Main", ChannelConfig::Stereo)
    }

    /// Append a main audio input bus. First call → main audio in;
    /// subsequent calls → sidechain inputs (use [`Self::with_sidechain_input`]
    /// if you prefer to be explicit).
    #[must_use]
    pub const fn with_input(mut self, name: &'static str, channels: ChannelConfig) -> Self {
        let kind = if self.inputs.len == 0 {
            BusKind::Main
        } else {
            BusKind::Sidechain
        };
        self.inputs = self.inputs.push(BusConfig {
            name,
            channels,
            kind,
        });
        self
    }

    /// Append a sidechain input bus. Equivalent to [`Self::with_input`]
    /// after the first input has already been added, but lets call
    /// sites express intent.
    #[must_use]
    pub const fn with_sidechain_input(
        mut self,
        name: &'static str,
        channels: ChannelConfig,
    ) -> Self {
        self.inputs = self.inputs.push(BusConfig {
            name,
            channels,
            kind: BusKind::Sidechain,
        });
        self
    }

    #[must_use]
    pub const fn with_output(mut self, name: &'static str, channels: ChannelConfig) -> Self {
        self.outputs = self.outputs.push(BusConfig {
            name,
            channels,
            kind: BusKind::Main,
        });
        self
    }

    /// Return the indices of all sidechain input buses.
    pub fn sidechain_input_indices(&self) -> impl Iterator<Item = usize> + '_ {
        self.inputs
            .iter()
            .enumerate()
            .filter(|(_, b)| b.kind == BusKind::Sidechain)
            .map(|(i, _)| i)
    }

    #[must_use]
    pub fn total_input_channels(&self) -> u32 {
        self.inputs.iter().map(|b| b.channels.channel_count()).sum()
    }

    #[must_use]
    pub fn total_output_channels(&self) -> u32 {
        self.outputs
            .iter()
            .map(|b| b.channels.channel_count())
            .sum()
    }
}

// ---------------------------------------------------------------------------
// The embedded table (`truce_utils::bus_table`'s wire format)
// ---------------------------------------------------------------------------

/// Bytes [`encode`] writes for `layouts`: the array length of the static
/// `truce::plugin!` embeds.
#[must_use]
pub const fn encoded_len(layouts: &[BusLayout]) -> usize {
    let mut n = bus_table::MAGIC.len() + 2;
    let mut i = 0;
    while i < layouts.len() {
        n += 2 + buses_len(layouts[i].inputs.as_slice()) + buses_len(layouts[i].outputs.as_slice());
        i += 1;
    }
    n
}

const fn buses_len(buses: &[BusConfig]) -> usize {
    let mut n = 0;
    let mut i = 0;
    while i < buses.len() {
        n += 1 + 4 + 1 + buses[i].name.len();
        i += 1;
    }
    n
}

/// Serialize `layouts` into `truce_utils::bus_table`'s wire format, at
/// compile time. `N` must be [`encoded_len`]`(layouts)`; `truce::plugin!`
/// sizes the static it embeds that way.
///
/// # Panics
///
/// At compile time, when `N` is wrong, or there are more than 255 layouts
/// or buses in one direction (the format's counts are bytes), or a bus name
/// is longer than 255 bytes.
#[must_use]
pub const fn encode<const N: usize>(layouts: &[BusLayout]) -> [u8; N] {
    assert!(
        N == encoded_len(layouts),
        "encode::<N>: N must be encoded_len(layouts)"
    );
    assert!(
        layouts.len() <= 255,
        "a plugin offers at most 255 bus layouts"
    );
    let mut out = [0u8; N];
    let mut at = 0;
    let mut m = 0;
    while m < bus_table::MAGIC.len() {
        out[at] = bus_table::MAGIC[m];
        at += 1;
        m += 1;
    }
    out[at] = bus_table::VERSION;
    #[allow(clippy::cast_possible_truncation)]
    {
        out[at + 1] = layouts.len() as u8;
    }
    at += 2;
    let mut i = 0;
    while i < layouts.len() {
        let inputs = layouts[i].inputs.as_slice();
        let outputs = layouts[i].outputs.as_slice();
        #[allow(clippy::cast_possible_truncation)]
        {
            out[at] = inputs.len() as u8;
            out[at + 1] = outputs.len() as u8;
        }
        at += 2;
        at = encode_buses(&mut out, at, inputs);
        at = encode_buses(&mut out, at, outputs);
        i += 1;
    }
    out
}

const fn encode_buses(out: &mut [u8], mut at: usize, buses: &[BusConfig]) -> usize {
    let mut i = 0;
    while i < buses.len() {
        let bus = &buses[i];
        out[at] = match bus.kind {
            BusKind::Main => bus_table::KIND_MAIN,
            BusKind::Sidechain => bus_table::KIND_SIDECHAIN,
        };
        let channels = bus.channels.channel_count().to_le_bytes();
        let mut c = 0;
        while c < 4 {
            out[at + 1 + c] = channels[c];
            c += 1;
        }
        let name = bus.name.as_bytes();
        assert!(name.len() <= 255, "a bus name is at most 255 bytes");
        #[allow(clippy::cast_possible_truncation)]
        {
            out[at + 5] = name.len() as u8;
        }
        at += 6;
        let mut b = 0;
        while b < name.len() {
            out[at] = name[b];
            at += 1;
            b += 1;
        }
        i += 1;
    }
    at
}

#[cfg(test)]
mod tests {
    use super::{BusKind, BusLayout, ChannelConfig, encode, encoded_len};
    use truce_utils::bus_table::{BusRecord, LayoutRecord, decode};

    const LAYOUTS: &[BusLayout] = &[
        BusLayout::stereo().with_sidechain_input("Key", ChannelConfig::Mono),
        BusLayout::new()
            .with_output("Main", ChannelConfig::Stereo)
            .with_output("Aux", ChannelConfig::Custom(6)),
    ];
    const LEN: usize = encoded_len(LAYOUTS);
    static TABLE: [u8; LEN] = encode(LAYOUTS);

    fn bus(name: &str, channels: u32, sidechain: bool) -> BusRecord {
        BusRecord {
            name: name.into(),
            channels,
            sidechain,
        }
    }

    /// What `truce::plugin!` embeds decodes back to the layouts it came
    /// from - the encoder (here, `const`) and decoder (`truce-utils`) agree.
    #[test]
    fn the_embedded_table_round_trips() {
        assert_eq!(
            decode(&TABLE).unwrap(),
            vec![
                LayoutRecord {
                    inputs: vec![bus("Main", 2, false), bus("Key", 1, true)],
                    outputs: vec![bus("Main", 2, false)],
                },
                LayoutRecord {
                    inputs: vec![],
                    outputs: vec![bus("Main", 2, false), bus("Aux", 6, false)],
                },
            ]
        );
    }

    #[test]
    fn const_builders_match_the_runtime_view() {
        let l = &LAYOUTS[0];
        assert_eq!(l.inputs.len(), 2);
        assert_eq!(l.inputs[1].kind, BusKind::Sidechain);
        assert_eq!(l.total_input_channels(), 3);
        assert_eq!(l.sidechain_input_indices().collect::<Vec<_>>(), vec![1]);
        assert_eq!(BusLayout::STEREO_AND_MONO[1], BusLayout::mono());
    }
}
