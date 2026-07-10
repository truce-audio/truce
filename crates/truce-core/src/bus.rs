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
/// Construct via [`Self::new`] / [`Self::mono`] / [`Self::stereo`] + the `with_*`
/// builders rather than struct literal - `#[non_exhaustive]` so
/// pre-1.0 future fields don't break downstream.
#[derive(Clone, Debug, Default)]
#[non_exhaustive]
pub struct BusLayout {
    pub inputs: Vec<BusConfig>,
    pub outputs: Vec<BusConfig>,
}

/// Constructed by [`BusLayout`]'s `with_*` builders. Marked
/// `#[non_exhaustive]` to keep the struct literal as a private
/// detail of the builder methods.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct BusConfig {
    pub name: &'static str,
    pub channels: ChannelConfig,
    pub kind: BusKind,
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
    pub fn channel_count(&self) -> u32 {
        match self {
            Self::Mono => 1,
            Self::Stereo => 2,
            Self::Custom(n) => *n,
        }
    }
}

impl BusLayout {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    #[must_use]
    pub fn mono() -> Self {
        Self::new()
            .with_input("Main", ChannelConfig::Mono)
            .with_output("Main", ChannelConfig::Mono)
    }

    #[must_use]
    pub fn stereo() -> Self {
        Self::new()
            .with_input("Main", ChannelConfig::Stereo)
            .with_output("Main", ChannelConfig::Stereo)
    }

    /// The default audio-effect layout set: stereo and mono (stereo
    /// first, so it's the default width). Return this from `bus_layouts()`
    /// for an ordinary in/out effect and the host offers it on both stereo
    /// and mono tracks, instead of a stereo-only effect that's hidden on
    /// mono ones. The effect's `process` must handle either width - loop
    /// over `buffer.channels()` rather than assuming two.
    #[must_use]
    pub fn stereo_and_mono() -> Vec<Self> {
        vec![Self::stereo(), Self::mono()]
    }

    /// The output-only counterpart of [`Self::stereo_and_mono`]: stereo and
    /// mono output buses with no input, for an instrument that produces
    /// audio from MIDI. Offered on both stereo and mono tracks. The
    /// instrument's `process` must handle either output width - guard any
    /// write past the first channel with `buffer.num_output_channels()`.
    #[must_use]
    pub fn stereo_and_mono_output() -> Vec<Self> {
        vec![
            Self::new().with_output("Main", ChannelConfig::Stereo),
            Self::new().with_output("Main", ChannelConfig::Mono),
        ]
    }

    /// Append a main audio input bus. First call → main audio in;
    /// subsequent calls → sidechain inputs (use [`Self::with_sidechain_input`]
    /// if you prefer to be explicit).
    #[must_use]
    pub fn with_input(mut self, name: &'static str, channels: ChannelConfig) -> Self {
        let kind = if self.inputs.is_empty() {
            BusKind::Main
        } else {
            BusKind::Sidechain
        };
        self.inputs.push(BusConfig {
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
    pub fn with_sidechain_input(mut self, name: &'static str, channels: ChannelConfig) -> Self {
        self.inputs.push(BusConfig {
            name,
            channels,
            kind: BusKind::Sidechain,
        });
        self
    }

    #[must_use]
    pub fn with_output(mut self, name: &'static str, channels: ChannelConfig) -> Self {
        self.outputs.push(BusConfig {
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
