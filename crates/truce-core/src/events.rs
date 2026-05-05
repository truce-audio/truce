/// A timestamped event within a process block.
///
/// `Copy` because every [`EventBody`] variant is POD — lets the audio
/// path move events without per-event clones.
#[derive(Clone, Copy, Debug)]
pub struct Event {
    /// Sample offset within the block (`0..num_samples`).
    pub sample_offset: u32,
    pub body: EventBody,
}

#[derive(Clone, Copy, Debug)]
pub enum EventBody {
    // -- MIDI 1.0 (normalized float values) --
    NoteOn {
        channel: u8,
        note: u8,
        velocity: f32,
    },
    NoteOff {
        channel: u8,
        note: u8,
        velocity: f32,
    },
    Aftertouch {
        channel: u8,
        note: u8,
        pressure: f32,
    },
    ChannelPressure {
        channel: u8,
        pressure: f32,
    },
    ControlChange {
        channel: u8,
        cc: u8,
        value: f32,
    },
    PitchBend {
        channel: u8,
        value: f32,
    },
    ProgramChange {
        channel: u8,
        program: u8,
    },

    // -- MIDI 2.0 (high-resolution) --
    NoteOn2 {
        group: u8,
        channel: u8,
        note: u8,
        velocity: u16,
        attribute_type: u8,
        attribute: u16,
    },
    NoteOff2 {
        group: u8,
        channel: u8,
        note: u8,
        velocity: u16,
        attribute_type: u8,
        attribute: u16,
    },
    PerNoteCC {
        channel: u8,
        note: u8,
        cc: u8,
        value: u32,
    },
    PerNotePitchBend {
        channel: u8,
        note: u8,
        value: u32,
    },
    PerNoteManagement {
        channel: u8,
        note: u8,
        flags: u8,
    },
    PolyPressure2 {
        channel: u8,
        note: u8,
        pressure: u32,
    },

    // -- Automation --
    ParamChange {
        id: u32,
        value: f64,
    },

    /// Parameter modulation offset (CLAP-specific, zero on other formats).
    /// The effective value is base + mod. The base value is unchanged.
    ParamMod {
        id: u32,
        note_id: i32,
        value: f64,
    },

    // -- Transport --
    Transport(TransportInfo),
}

#[derive(Clone, Copy, Debug, Default)]
pub struct TransportInfo {
    pub playing: bool,
    pub recording: bool,
    pub tempo: f64,
    pub time_sig_num: u8,
    pub time_sig_den: u8,
    pub position_samples: i64,
    pub position_seconds: f64,
    pub position_beats: f64,
    pub bar_start_beats: f64,
    pub loop_active: bool,
    pub loop_start_beats: f64,
    pub loop_end_beats: f64,
}

impl TransportInfo {
    /// Synthetic transport for snapshot tests — playing at 120 BPM,
    /// 4/4, position 4.0 beats. Used as the default by every snapshot
    /// helper (`truce-egui`, `truce-slint`, `truce-iced`,
    /// `truce-test`) so that transport-aware widgets render a
    /// populated readout in marketing screenshots instead of a
    /// `(no host transport)` placeholder.
    #[must_use]
    pub fn for_screenshot() -> Self {
        Self {
            playing: true,
            tempo: 120.0,
            time_sig_num: 4,
            time_sig_den: 4,
            position_beats: 4.0,
            ..Self::default()
        }
    }
}

/// Default reserved capacity for per-instance `EventList`s held by
/// format wrappers. Sized to cover a heavy MIDI block (note bursts +
/// per-block automation changes) without growing past steady state.
/// Each `Event` is roughly 40 bytes, so this reservation is ~10 KB
/// per list — two lists (input + output) per plugin instance.
///
/// Plugins can construct a smaller or larger list explicitly via
/// [`EventList::with_capacity`]; this const exists so the format
/// wrappers don't each pick their own magic number.
pub const EVENT_LIST_PREALLOC: usize = 256;

/// Ordered list of events within a process block.
#[derive(Clone, Debug, Default)]
pub struct EventList {
    events: Vec<Event>,
}

impl EventList {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Construct an `EventList` with backing capacity already reserved.
    ///
    /// Format wrappers build their per-instance event lists at
    /// construction time and reuse them across blocks via `clear()`.
    /// Without this, the first `push` after `EventList::new()` hits
    /// the global allocator on the audio thread; pre-allocating with
    /// the max event count an audio block is likely to carry keeps
    /// the first block alloc-free.
    #[must_use]
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            events: Vec::with_capacity(capacity),
        }
    }

    /// Append an event. Note: `sample_offset` is **not** bounds-checked
    /// against any block size — callers that build event lists per
    /// block must validate `sample_offset < num_samples` themselves
    /// (the audio thread can't recover from an out-of-range offset, so
    /// we treat that as a contract violation rather than panicking).
    pub fn push(&mut self, event: Event) {
        self.events.push(event);
    }

    pub fn clear(&mut self) {
        self.events.clear();
    }

    /// Stable sort by `sample_offset`. **Stability matters:** events
    /// with identical sample offsets stay in the order they were
    /// pushed, which is what plugins assume when they iterate (e.g.
    /// "MIDI on this sample then a CC on the same sample" stays in
    /// that order). Don't replace with `sort_unstable_by_key` — the
    /// stability guarantee is load-bearing.
    pub fn sort(&mut self) {
        self.events.sort_by_key(|e| e.sample_offset);
    }

    pub fn iter(&self) -> impl Iterator<Item = &Event> {
        self.events.iter()
    }

    #[must_use]
    pub fn get(&self, index: usize) -> Option<&Event> {
        self.events.get(index)
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.events.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.events.is_empty()
    }
}
