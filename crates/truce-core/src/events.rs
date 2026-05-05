//! Event types crossing the host → plugin boundary.
//!
//! `EventBody` carries MIDI 1.0 and MIDI 2.0 channel-voice messages
//! in their **wire-native integer** shapes (7-bit `u8`, 14-bit
//! `u16`, 16-bit `u16`, 32-bit `u32`) so the framework's
//! representation round-trips exactly with the host's wire format.
//! Plugin code that wants float values reaches for the helpers in
//! [`truce_utils::midi`] (`norm_7bit`, `norm_pitch_bend`,
//! `norm_16bit`, `norm_32bit`, `norm_pitch_bend_32`, etc.).
//!
//! Every MIDI variant carries a `group: u8` field (0..=15) that
//! UMP (Universal MIDI Packet) hosts use to address one of 16
//! groups × 16 channels = 256 logical channels. Format wrappers
//! that don't expose the group field (legacy MIDI 1.0 byte streams)
//! emit `0`.

/// A timestamped event within a process block.
///
/// `Copy` because every [`EventBody`] variant is POD — lets the
/// audio path move events without per-event clones.
#[derive(Clone, Copy, Debug)]
pub struct Event {
    /// Sample offset within the block (`0..num_samples`).
    pub sample_offset: u32,
    pub body: EventBody,
}

#[derive(Clone, Copy, Debug)]
pub enum EventBody {
    // -- MIDI 1.0 channel voice (wire-native 7-bit / 14-bit) --
    /// Note on. MIDI 1.0 quirk: a `NoteOn` with `velocity == 0` is
    /// a `NoteOff`. Format wrappers normalize that at parse time so
    /// plugin code can match `NoteOn` without checking velocity.
    NoteOn {
        group: u8,
        channel: u8,
        note: u8,
        velocity: u8,
    },
    NoteOff {
        group: u8,
        channel: u8,
        note: u8,
        velocity: u8,
    },
    /// Polyphonic key pressure (per-note aftertouch).
    Aftertouch {
        group: u8,
        channel: u8,
        note: u8,
        pressure: u8,
    },
    ChannelPressure {
        group: u8,
        channel: u8,
        pressure: u8,
    },
    ControlChange {
        group: u8,
        channel: u8,
        cc: u8,
        value: u8,
    },
    /// 14-bit pitch bend, raw code `0..=16383`. `8192` is center.
    /// See `truce_utils::midi::norm_pitch_bend` for the
    /// asymmetric-range conversion helper.
    PitchBend {
        group: u8,
        channel: u8,
        value: u16,
    },
    ProgramChange {
        group: u8,
        channel: u8,
        program: u8,
    },

    // -- MIDI 2.0 channel voice (wire-native 16/32-bit) --
    /// MIDI 2.0 `NoteOn`. `velocity` is `0..=65535`; unlike MIDI 1.0,
    /// a zero velocity is a genuine zero (`NoteOff` is its own
    /// dedicated message). `attribute_type` indicates how
    /// `attribute` should be interpreted: 0 = no attribute, 1 =
    /// manufacturer-specific, 2 = profile-specific, 3 = Pitch 7.9.
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
    /// MIDI 2.0 polyphonic key pressure (`pressure: u32`).
    PolyPressure2 {
        group: u8,
        channel: u8,
        note: u8,
        pressure: u32,
    },
    /// MIDI 2.0 per-note controller. `registered = true` for
    /// Registered Per-Note (RPN-like indexed list); `false` for
    /// Assignable Per-Note (free-form per-controller mapping).
    PerNoteCC {
        group: u8,
        channel: u8,
        note: u8,
        cc: u8,
        value: u32,
        registered: bool,
    },
    /// MIDI 2.0 per-note pitch bend (`value: u32`). `0x8000_0000`
    /// is center.
    PerNotePitchBend {
        group: u8,
        channel: u8,
        note: u8,
        value: u32,
    },
    /// MIDI 2.0 per-note management flags. Bit 0 = detach
    /// per-note controllers from active note; bit 1 = reset
    /// (set) per-note controllers to default values.
    PerNoteManagement {
        group: u8,
        channel: u8,
        note: u8,
        flags: u8,
    },
    /// MIDI 2.0 channel-wide control change (32-bit).
    ControlChange2 {
        group: u8,
        channel: u8,
        cc: u8,
        value: u32,
    },
    /// MIDI 2.0 channel pressure (32-bit aftertouch on the whole
    /// channel).
    ChannelPressure2 {
        group: u8,
        channel: u8,
        pressure: u32,
    },
    /// MIDI 2.0 channel pitch bend (32-bit). `0x8000_0000` is
    /// center.
    PitchBend2 {
        group: u8,
        channel: u8,
        value: u32,
    },
    /// MIDI 2.0 program change. Optional bank pair (MSB, LSB);
    /// MIDI 2.0's "B" flag is encoded as `Some` / `None`. When
    /// `None`, the host hasn't selected a bank and the program
    /// applies in the current bank.
    ProgramChange2 {
        group: u8,
        channel: u8,
        program: u8,
        bank: Option<(u8, u8)>,
    },
    /// MIDI 2.0 Registered Controller (the spec's RPN replacement,
    /// 32-bit). `bank` and `index` are the two 7-bit identifiers
    /// the spec reserves for Registered Parameter Numbers.
    RegisteredController {
        group: u8,
        channel: u8,
        bank: u8,
        index: u8,
        value: u32,
    },
    /// MIDI 2.0 Assignable Controller (the spec's NRPN
    /// replacement, 32-bit). `bank` and `index` are
    /// manufacturer-defined.
    AssignableController {
        group: u8,
        channel: u8,
        bank: u8,
        index: u8,
        value: u32,
    },

    // -- truce-internal automation --
    ParamChange {
        id: u32,
        value: f64,
    },
    /// Parameter modulation offset (CLAP-specific, zero on other
    /// formats). Effective value is `base + value`. The base value
    /// is unchanged.
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
    /// Construct an `EventList` with backing capacity already reserved.
    ///
    /// Format wrappers build their per-instance event lists at
    /// construction time and reuse them across blocks via `clear()`.
    /// Without this, the first `push` after `EventList::default()` hits
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
