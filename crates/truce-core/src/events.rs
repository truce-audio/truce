/// A timestamped event within a process block.
#[derive(Clone, Debug)]
pub struct Event {
    /// Sample offset within the block (0..num_samples).
    pub sample_offset: u32,
    pub body: EventBody,
}

#[derive(Clone, Debug)]
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

#[derive(Clone, Debug, Default)]
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

/// Ordered list of events within a process block.
#[derive(Clone, Debug, Default)]
pub struct EventList {
    events: Vec<Event>,
}

impl EventList {
    pub fn new() -> Self {
        Self { events: Vec::new() }
    }

    pub fn push(&mut self, event: Event) {
        self.events.push(event);
    }

    pub fn clear(&mut self) {
        self.events.clear();
    }

    pub fn sort(&mut self) {
        self.events.sort_by_key(|e| e.sample_offset);
    }

    pub fn iter(&self) -> impl Iterator<Item = &Event> {
        self.events.iter()
    }

    pub fn get(&self, index: usize) -> Option<&Event> {
        self.events.get(index)
    }

    pub fn len(&self) -> usize {
        self.events.len()
    }

    pub fn is_empty(&self) -> bool {
        self.events.is_empty()
    }
}
