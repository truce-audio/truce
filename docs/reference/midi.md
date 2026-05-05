# 6. MIDI

MIDI events ride the same `EventList` as parameter automation —
they're just variants of `EventBody`. Reading them is a `match`
on the body; emitting them is `context.output_events.push(...)`.

The framework hands you wire-native integers (7-bit `u8`, 14-bit
`u16`, MIDI 2.0 16/32-bit) so values round-trip exactly with the
host. Float helpers are one function call away when you want
them.

## Declaring a MIDI plugin

Every plugin sees MIDI input that the host sends to it. What
varies is whether the plugin produces audio, MIDI, or both.

| Plugin shape | `truce.toml` `category` | Example |
|---|---|---|
| Audio effect (may also accept MIDI) | `"effect"` | EQ, compressor, synth-style filter |
| Instrument (MIDI in, audio out) | `"instrument"` | Synth, sampler |
| Note effect (MIDI in, MIDI out) | `"midi"` | Arpeggiator, transpose, chord generator |
| Analyzer (no audio out) | `"analyzer"` | Spectrum, level meter |

Set `category = "midi"` for note effects so each format wrapper
opens its MIDI I/O path:

- VST3 / CLAP: registers a MIDI input *and* output bus.
- AU: builds as `aumi` (MIDI FX), routed to Logic's MIDI FX slot.
- AAX: registers `LocalInput` + `LocalOutput` MIDI nodes.
- LV2: emits an `atom:Sequence` output port in addition to the
  input.

Set `category = "instrument"` for synths so wrappers register
MIDI input + audio output (and AU builds as `aumu`).

`category = "effect"` plugins receive MIDI input where the host
provides it (CLAP, VST3, AAX). VST2 effects don't get MIDI input
unless you opt in via the format's `canDo` flag — in practice
declare `"midi"` or `"instrument"` if you need MIDI on every
host.

## The event model

```rust
pub struct Event {
    pub sample_offset: u32,    // 0..num_samples in this block
    pub body: EventBody,
}

pub enum EventBody {
    // MIDI 1.0 channel voice
    NoteOn        { group, channel, note, velocity },           // u8 each
    NoteOff       { group, channel, note, velocity },
    Aftertouch    { group, channel, note, pressure },           // poly key pressure
    ChannelPressure { group, channel, pressure },
    ControlChange { group, channel, cc, value },                // 7-bit
    PitchBend     { group, channel, value: u16 },               // 14-bit, 8192 = center
    ProgramChange { group, channel, program },

    // MIDI 2.0 channel voice (wire-native 16/32-bit)
    NoteOn2 / NoteOff2     { ..., velocity: u16, attribute_type, attribute },
    PolyPressure2          { ..., pressure: u32 },
    PerNoteCC              { ..., cc, value: u32, registered },
    PerNotePitchBend       { ..., value: u32 },                 // 0x8000_0000 = center
    PerNoteManagement      { ..., flags },
    ControlChange2         { ..., cc, value: u32 },
    ChannelPressure2       { ..., pressure: u32 },
    PitchBend2             { ..., value: u32 },
    ProgramChange2         { ..., program, bank: Option<(u8, u8)> },
    RegisteredController   { ..., bank, index, value: u32 },    // RPN
    AssignableController   { ..., bank, index, value: u32 },    // NRPN

    // Plugin/host control (not MIDI)
    ParamChange { id, value },
    ParamMod    { id, note_id, value },                         // CLAP per-voice
    Transport   (TransportInfo),
}
```

`EventBody` is `Copy`, so the audio path never clones an event.
`group` is the UMP group (0–15); legacy MIDI 1.0 wrappers fill
`0`. `channel` is 0–15.

The list is **stable-sorted by `sample_offset`** before your
plugin sees it. Ties stay in the order the host sent them, which
matters when (e.g.) a CC arrives at the same sample as a note-on.

## Reading MIDI input

The plugin sees `&EventList` in `process()`:

```rust
fn process(&mut self, buffer: &mut AudioBuffer, events: &EventList,
           context: &mut ProcessContext) -> ProcessStatus {
    for event in events.iter() {
        match &event.body {
            EventBody::NoteOn  { note, velocity, .. } => self.note_on(*note, *velocity),
            EventBody::NoteOff { note, .. }           => self.note_off(*note),
            EventBody::ControlChange { cc, value, .. } => self.cc(*cc, *value),
            EventBody::PitchBend { value, .. }         => self.pb(*value),
            _ => {}
        }
    }
    // ... DSP ...
    ProcessStatus::Normal
}
```

The `_ => {}` arm catches MIDI 2.0 / per-note variants you don't
care about. Drop it and you'll get a non-exhaustive-match error
that lists everything you missed — useful when you want the
compiler to flag a forgotten case.

For sample-accurate handling (synths, transient shapers),
interleave the event walk with the sample loop instead:

```rust
let mut next = 0;
for i in 0..buffer.num_samples() {
    while let Some(e) = events.get(next) {
        if e.sample_offset as usize > i { break; }
        match &e.body {
            EventBody::NoteOn  { note, velocity, .. } => self.note_on(*note, *velocity),
            EventBody::NoteOff { note, .. }           => self.note_off(*note),
            _ => {}
        }
        next += 1;
    }
    // render sample i...
}
```

## Reading values as floats

MIDI values are integers on the wire. Convert when DSP wants
floats:

```rust
use truce_core::midi::{norm_7bit, norm_pitch_bend};

EventBody::ControlChange { cc: 1, value, .. } => {
    self.mod_depth = norm_7bit(*value);              // 0..=127 → [0.0, 1.0]
}
EventBody::PitchBend { value, .. } => {
    self.bend_semitones = norm_pitch_bend(*value) * 2.0;  // [-1.0, 1.0)
}
```

Available helpers (`truce_core::midi::*`, re-exported from
`truce_utils::midi`):

- `norm_7bit(u8) -> f32` and `denorm_7bit(f32) -> u8` — velocity,
  CC, channel pressure, aftertouch, program change.
- `norm_pitch_bend(u16) -> f32` and `denorm_pitch_bend(f32) -> u16`
  — 14-bit pitch bend. Asymmetric: `0` decodes to `-1.0`,
  `8192` to `0.0`, `16383` to `~0.99987`.
- `pitch_bend_to_bytes(u16) -> (u8, u8)` and
  `pitch_bend_from_bytes(u8, u8) -> u16` — split / combine LSB +
  MSB. Format wrappers use these internally; plugins rarely
  need them.

## Emitting MIDI output

Push events onto `context.output_events`:

```rust
context.output_events.push(Event {
    sample_offset: e.sample_offset,
    body: EventBody::NoteOn {
        group: 0, channel: 0,
        note: 60, velocity: 100,
    },
});
```

Sample offsets must fall within the current block
(`0..num_samples`). The framework forwards each event to the
host's MIDI output as a MIDI 1.0 byte stream. Variants that don't
fit MIDI 1.0 (every MIDI 2.0 variant, `ParamChange`,
`Transport`) are silently dropped at the wrapper. Use the MIDI
1.0 variants for portable note effects.

The arpeggiator example in `examples/truce-example-arpeggio/`
walks held-note tracking + step scheduling:

```rust
EventBody::NoteOn  { note, .. } => self.held.push(*note),
EventBody::NoteOff { note, .. } => self.held.retain(|n| n != note),
// ...later, on each step boundary:
context.output_events.push(Event {
    sample_offset: step_offset,
    body: EventBody::NoteOn {
        group: 0, channel: 0,
        note: chosen_note, velocity: 96,
    },
});
```

## Format coverage

| Format | MIDI 1.0 in | MIDI 1.0 out | MIDI 2.0 in | Notes |
|---|---|---|---|---|
| CLAP | ✅ | ✅ | partial† | Per-note expression mapped to `PerNoteCC` / `PerNotePitchBend` |
| VST3 | ✅ | ✅ | partial† | Per-note expression (volume, pan, tuning, vibrato, expression, brightness) → MIDI 2.0 events |
| VST2 | ✅ | ✅ | — | MIDI 1.0 only; opt-in per VST2's `canDo("receiveVstMidiEvent")` |
| AU v2 | ✅ | ✅ | — | MIDI 2.0 events landing on the input bus are silently dropped |
| AU v3 | ✅ | ✅ | — | Same as AU v2 |
| AAX | ✅ | ✅ | — | Pro Tools' MIDI tracks; see `docs/formats/aax.md` |
| LV2 | ✅ | ✅ | — | Hosts deliver `atom:Sequence`; emits one in turn for note effects |

† MIDI 2.0 *channel-voice* messages (`NoteOn2`, `ControlChange2`,
etc.) are not currently demuxed by any wrapper. If you receive a
MIDI 2.0 host event your plugin sees it as a MIDI 1.0
`NoteOn` / `ControlChange` after the host's own downconvert.
Emitting MIDI 2.0 variants from a plugin is also not wired
end-to-end yet.

## Testing MIDI plugins

`truce_test::driver!` scripts MIDI events sample-accurately —
same delivery path the format wrappers use, no host required.

```rust
use std::time::Duration;
use truce_test::{assertions, driver};

#[test]
fn arp_emits_step_per_quarter_at_120bpm() {
    let result = driver!(MyArp)
        .duration(Duration::from_secs(1))
        .capture_output_events(true)
        .script(|s| {
            s.note_on(60, 0.8);   // velocity is normalized [0, 1]
            s.note_on(64, 0.8);
            s.note_on(67, 0.8);
        })
        .run();

    let notes = result.output_events.iter()
        .filter(|e| matches!(e.body, EventBody::NoteOn { .. }))
        .count();
    assert_eq!(notes, 4);   // four quarter-note steps in one second at 120 BPM
}
```

The `Script` builder exposes one method per common MIDI 1.0
message — `note_on`, `note_off`, `cc`, `pitch_bend`,
`channel_pressure`, plus `set_param` for automation. Need
something else? `Script::push(EventBody)` takes anything.

The arpeggiator example's tests (`examples/truce-example-arpeggio/
src/lib.rs`) cover the full MIDI-in / MIDI-out shape end to end.

## What's next

- **[Chapter 7 → gui.md](gui.md)** — visualise note state,
  expose CC mappings as parameters.
- **[Chapter 8 → hot-reload.md](hot-reload.md)** — iterate on
  arp logic without restarting the DAW.
- **`examples/truce-example-arpeggio/`** in the repo — full
  MIDI in → MIDI out plugin with state, transport, and tests.
- **`examples/truce-example-synth/`** — MIDI in → audio out
  with sample-accurate event handling.
