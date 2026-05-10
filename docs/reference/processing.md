# 5. Processing audio

`process()` is called on the audio thread for every block. Same
constraints as any audio plugin — no allocation, no locking, no
I/O, no `println!`. Rust's type system catches a lot of this; the
rest is up to you.

The signature is always:

```rust
fn process(
    &mut self,
    buffer: &mut AudioBuffer,
    events:  &EventList,
    context: &mut ProcessContext,
) -> ProcessStatus;
```

Everything in this chapter is a different shape for that function.

## Buffer model

`AudioBuffer` exposes one `&[f32]` per input channel and one
`&mut [f32]` per output channel, both borrowing host memory. Wrappers
do not copy input into output: read from `buffer.input(ch)` and write
to `buffer.output(ch)`. For instruments, output starts wherever the
host left it (typically zero, but don't assume — write every sample).

## In-place I/O (advanced; opt-in)

Some hosts (Reaper, pluginval) pass the same buffer for both input
and output of a given channel. By default truce handles this for you
— the wrapper detects the alias and copies the input into per-channel
scratch so `buffer.input(ch)` and `buffer.output(ch)` are always
disjoint slices. The cost is one memcpy per aliased channel per block
(a few hundred KB/sec at audio rates) and it never shows up unless
you go looking. **Most plugins should ignore this section.**

If you profile and the wrapper memcpy is meaningful for your DSP,
opt out by setting `SUPPORTS_IN_PLACE = true` on your `Plugin` impl.
The wrapper then skips the copy and you read+write the shared buffer
directly:

```rust
impl Plugin for MyEffect {
    const SUPPORTS_IN_PLACE: bool = true;
    // ...
    fn process(&mut self, buffer: &mut AudioBuffer, _: &EventList,
               _: &mut ProcessContext) -> ProcessStatus {
        for ch in 0..buffer.num_output_channels() {
            if buffer.is_in_place(ch) {
                // Host shares one buffer for in+out; read each
                // sample, then overwrite it.
                let inout = buffer.in_out_mut(ch);
                for s in inout.iter_mut() { *s = self.process_sample(*s); }
            } else {
                let inp = buffer.input(ch);
                let out = buffer.output(ch);
                for i in 0..inp.len() { out[i] = self.process_sample(inp[i]); }
            }
        }
        ProcessStatus::Normal
    }
}
```

The contract:

- With `SUPPORTS_IN_PLACE = true`, `buffer.input(ch)` returns an empty
  slice for in-place channels — the data only exists in the shared
  buffer. You **must** check `buffer.is_in_place(ch)` and use
  `buffer.in_out_mut(ch)` for those channels.
- With `SUPPORTS_IN_PLACE = false` (default), `buffer.input(ch)` and
  `buffer.output(ch)` are always safe and disjoint, even when the
  host requested in-place. `is_in_place` still reflects the host's
  choice — but you can ignore it.

## Per-sample effect

The most common shape — one multiplication per sample per channel:

```rust
fn process(&mut self, buffer: &mut AudioBuffer, _: &EventList,
           _: &mut ProcessContext) -> ProcessStatus {
    for i in 0..buffer.num_samples() {
        let gain = db_to_linear(self.params.gain.smoothed_next());
        for ch in 0..buffer.channels() {
            let (inp, out) = buffer.io(ch);
            out[i] = inp[i] * gain;
        }
    }
    ProcessStatus::Normal
}
```

Pull smoothed param values **per sample** when they need to glide
cleanly (gain, filter cutoff). Pull **per block** for param reads
that are expensive or that don't care about sample-accuracy
(mode switches, enums).

## Per-channel loop with input/output pairs

Useful when you need separate read and write pointers (convolution,
IIR filters) rather than in-place modification:

```rust
for ch in 0..buffer.num_output_channels() {
    let (input, output) = buffer.io_pair(ch, ch);
    for i in 0..buffer.num_samples() {
        output[i] = self.filters[ch].process(input[i]);
    }
}
ProcessStatus::Normal
```

## MIDI and parameter events

`events` is a sorted list of `Event { sample_offset, body }`.
Pattern match the body:

```rust
for event in events.iter() {
    match &event.body {
        EventBody::NoteOn  { note, velocity, .. } => self.note_on(*note, *velocity),
        EventBody::NoteOff { note, .. }           => self.note_off(*note),
        EventBody::ControlChange { cc: 1, value, .. } => {
            self.mod_depth = *value;
        }
        _ => {}
    }
}
```

`EventBody` also carries MIDI 2.0 variants (`NoteOn2`, `PerNoteCC`,
`PerNotePitchBend`, …) and CLAP parameter modulation (`ParamMod`
with a per-voice `note_id`). The `_ => {}` arm means the compiler
can still warn if you forgot a variant that mattered.

For MIDI input *and* output (arpeggiators, transposers, chord
generators), see **[midi.md](midi.md)**.

## Sample-accurate event splitting

If your synth or transient shaper needs events applied at the
exact sample they occur, interleave the event loop with the sample
loop:

```rust
fn process(&mut self, buffer: &mut AudioBuffer, events: &EventList,
           _: &mut ProcessContext) -> ProcessStatus {
    let mut next = 0;

    for i in 0..buffer.num_samples() {
        while let Some(event) = events.get(next) {
            if event.sample_offset as usize > i { break; }
            self.handle_event(&event.body);
            next += 1;
        }
        for ch in 0..buffer.channels() {
            buffer.output(ch)[i] = self.render_sample(ch);
        }
    }
    ProcessStatus::Normal
}
```

For block-rate event handling (effects where param changes don't
need sample accuracy), process the event list once at the top and
then the whole block — simpler and cheaper.

## Host transport

`context.transport` surfaces tempo, play state, beat position, loop
bounds. Use it for tempo-synced LFOs, bar-locked envelopes, looping
delays.

```rust
let t = &context.transport;
if t.playing {
    let beat   = t.position_beats;
    let tempo  = t.tempo;
    let bar    = t.time_sig_num as f64;
    let phase  = (beat * self.sync_rate) % 1.0;
    let in_bar = beat % bar;
    // ...
}
```

Not every host fills every field every block. The
`examples/truce-example-tremolo` example shows the robust pattern:
fall back to a free-running internal clock at 120 BPM when the
host doesn't provide transport.

## Meters (DSP → UI)

Meters push from `process()` via `context.set_meter`, indexed by
typed `ParamId`. The GUI reads the latest value every frame.

```rust
context.set_meter(P::MeterL, buffer.output_peak(0));
context.set_meter(P::MeterR, buffer.output_peak(1));
```

Realtime-safe (atomic). Declaration of the `MeterSlot` fields is
in [chapter 4 → parameters.md § Meters](parameters.md#meters).

## Declaring tail time

Effects with memory — reverbs, delays, self-oscillating filters —
keep producing audio after the input stops. Tell the host how many
samples are left so it doesn't cut you off:

```rust
if self.is_producing_silence() {
    ProcessStatus::Tail(self.remaining_tail_samples())
} else {
    ProcessStatus::Normal
}
```

Return `ProcessStatus::Tail(0)` from a synth when every voice has
released — the host can then elide further `process` calls until
the next note-on.

## Building a synth

A polyphonic synth is a combination of the patterns above:

- **Sample-accurate event loop** so note-ons land at the right
  sample.
- **Per-sample param reads** for filter cutoff / resonance (they
  sound bad when block-rate'd).
- **`ProcessStatus::Tail(0)`** when all voices are done so the host
  can idle.

The full `examples/truce-example-synth/` plugin (in the repo) is
roughly this shape:

```rust
impl PluginLogic for Synth {
    fn reset(&mut self, sample_rate: f64, _: usize) {
        self.sample_rate = sample_rate;
        self.voices.clear();
        self.params.set_sample_rate(sample_rate);
        self.params.snap_smoothers();
    }

    fn process(&mut self, buffer: &mut AudioBuffer, events: &EventList,
               _: &mut ProcessContext) -> ProcessStatus {
        let mut next = 0;

        for i in 0..buffer.num_samples() {
            // 1. Dispatch any events landing at this sample.
            while let Some(e) = events.get(next) {
                if e.sample_offset as usize > i { break; }
                match &e.body {
                    EventBody::NoteOn  { note, velocity, .. } => self.note_on(*note, *velocity),
                    EventBody::NoteOff { note, .. }           => self.note_off(*note),
                    _ => {}
                }
                next += 1;
            }

            // 2. Read per-sample smoothed params.
            let wave    = self.params.waveform.index();
            let cutoff  = self.params.cutoff.smoothed_next_f64();
            let reso    = self.params.resonance.smoothed_next_f64();
            let volume  = db_to_linear(self.params.volume.smoothed_next_f64());

            // 3. Sum the voices.
            let mut sample = 0.0f64;
            for voice in &mut self.voices {
                sample += voice.render(wave, cutoff, reso, self.sample_rate);
            }
            sample *= volume;

            let out = (sample as f32).clamp(-1.0, 1.0);
            buffer.output(0)[i] = out;
            buffer.output(1)[i] = out;
        }

        // 4. Retire finished voices; signal idle when empty.
        self.voices.retain(|v| !v.is_done());
        if self.voices.is_empty() { ProcessStatus::Tail(0) } else { ProcessStatus::Normal }
    }

    fn layout(&self) -> truce_gui::layout::GridLayout { /* ... */ }
}
```

Voice allocation, ADSR, and filter state live in the `Voice` struct
— plain Rust, no framework involvement. Parameters flow in through
`Arc<Params>`; nothing else is shared across threads.

The instrument tells the macro it has no audio inputs:

```rust
truce::plugin! {
    logic: Synth,
    params: SynthParams,
    bus_layouts: [BusLayout::new().with_output("Main", ChannelConfig::Stereo)],
}
```

## What's next

- **[Chapter 6 → midi.md](midi.md)** — emitting MIDI, wire-format
  helpers, MIDI 2.0 surface.
- **[Chapter 7 → gui.md](gui.md)** — widgets, layout, meters in
  the UI.
- **[Chapter 8 → hot-reload.md](hot-reload.md)** — keep your DAW
  open while you iterate on this code.
- **`examples/truce-example-tremolo`** in the repo — host transport
  + egui UI in a small, real plugin.
