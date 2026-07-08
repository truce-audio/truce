//! Activation-time processing configuration.
//!
//! [`AudioConfig`] is the config struct handed to `reset` when the host
//! (re)prepares the plugin. It carries the sample rate, the maximum block
//! size, and the [`ProcessMode`] the host is driving audio with, so a
//! plugin can size its buffers for an offline render before the first
//! block arrives - allocation has to happen here, off the audio thread.

/// How the host is driving audio through the plugin this activation.
///
/// Delivered two ways that answer two different questions.
/// [`AudioConfig::process_mode`] at `reset` answers "how big should my
/// buffers be for this render?" - the allocation-relevant question, since
/// buffer sizing has to happen off the audio thread. The per-block
/// `ProcessContext::process_mode` answers "may I skip the realtime
/// discipline right now?" - it tracks host toggles that don't warrant a
/// re-prepare (VST3 `kRealtime` <-> `kPrefetch`, an LV2 freewheel port).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum ProcessMode {
    /// Fixed-rate realtime playback. Honor the no-alloc / no-lock rule.
    #[default]
    Realtime,
    /// Real-time-like but processed ahead at irregular intervals to
    /// loosen realtime pressure. Only VST3 (`kPrefetch`) produces this.
    /// Treat it like [`Realtime`](Self::Realtime) unless there is a
    /// specific reason to relax discipline.
    Buffered,
    /// Freewheeling offline render. No wall-clock deadline: a plugin may
    /// allocate, raise oversampling, lengthen lookahead, and trade CPU
    /// for quality.
    Offline,
}

impl ProcessMode {
    /// Whether the host is freewheeling with no realtime deadline. True
    /// only for [`Offline`](Self::Offline) - the one mode where relaxing
    /// the no-alloc / no-lock rule and raising quality is safe.
    #[must_use]
    pub fn is_offline(self) -> bool {
        matches!(self, ProcessMode::Offline)
    }

    /// Discriminant for atomic storage. Wrappers whose offline signal
    /// arrives on one thread (CLAP `render::set`, an AU property) and is
    /// read on the audio thread stash the mode in an `AtomicU8`.
    #[must_use]
    pub fn as_u8(self) -> u8 {
        match self {
            ProcessMode::Realtime => 0,
            ProcessMode::Buffered => 1,
            ProcessMode::Offline => 2,
        }
    }

    /// Inverse of [`Self::as_u8`]. Any unknown value maps to
    /// [`Realtime`](Self::Realtime), the safe default.
    #[must_use]
    pub fn from_u8(v: u8) -> Self {
        match v {
            1 => ProcessMode::Buffered,
            2 => ProcessMode::Offline,
            _ => ProcessMode::Realtime,
        }
    }
}

/// Activation-time configuration handed to `reset`.
///
/// `#[non_exhaustive]` so future prepare-time fields (a bus layout, host
/// latency budget) can be added without breaking the `reset` signature.
/// Construct with [`Self::new`] (defaults to [`ProcessMode::Realtime`])
/// plus [`Self::with_process_mode`]; read the fields directly.
#[derive(Clone, Copy, Debug, PartialEq)]
#[non_exhaustive]
pub struct AudioConfig {
    /// Host sample rate in Hz.
    pub sample_rate: f64,
    /// Largest block the host will hand `process`. Size preallocations
    /// against this - the host never sends a bigger block this activation.
    pub max_block_size: usize,
    /// How the host drives audio this activation. See [`ProcessMode`].
    pub process_mode: ProcessMode,
}

impl AudioConfig {
    /// New config at the default [`ProcessMode::Realtime`]. Chain
    /// [`Self::with_process_mode`] for an offline / buffered activation.
    #[must_use]
    pub fn new(sample_rate: f64, max_block_size: usize) -> Self {
        Self {
            sample_rate,
            max_block_size,
            process_mode: ProcessMode::Realtime,
        }
    }

    /// Set the processing mode for this activation.
    #[must_use]
    pub fn with_process_mode(mut self, mode: ProcessMode) -> Self {
        self.process_mode = mode;
        self
    }
}
