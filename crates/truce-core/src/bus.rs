/// Describes the audio bus configuration of a plugin.
#[derive(Clone, Debug)]
pub struct BusLayout {
    pub inputs: Vec<BusConfig>,
    pub outputs: Vec<BusConfig>,
}

#[derive(Clone, Debug)]
pub struct BusConfig {
    pub name: &'static str,
    pub channels: ChannelConfig,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChannelConfig {
    Mono,
    Stereo,
    Custom(u32),
}

impl ChannelConfig {
    pub fn channel_count(&self) -> u32 {
        match self {
            Self::Mono => 1,
            Self::Stereo => 2,
            Self::Custom(n) => *n,
        }
    }
}

impl BusLayout {
    pub fn new() -> Self {
        Self {
            inputs: Vec::new(),
            outputs: Vec::new(),
        }
    }

    pub fn stereo() -> Self {
        Self::new()
            .with_input("Main", ChannelConfig::Stereo)
            .with_output("Main", ChannelConfig::Stereo)
    }

    pub fn with_input(mut self, name: &'static str, channels: ChannelConfig) -> Self {
        self.inputs.push(BusConfig { name, channels });
        self
    }

    pub fn with_output(mut self, name: &'static str, channels: ChannelConfig) -> Self {
        self.outputs.push(BusConfig { name, channels });
        self
    }

    pub fn total_input_channels(&self) -> u32 {
        self.inputs.iter().map(|b| b.channels.channel_count()).sum()
    }

    pub fn total_output_channels(&self) -> u32 {
        self.outputs
            .iter()
            .map(|b| b.channels.channel_count())
            .sum()
    }
}

impl Default for BusLayout {
    fn default() -> Self {
        Self::new()
    }
}
