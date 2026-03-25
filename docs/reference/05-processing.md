## Processing audio

**In-place processing**: For effects, all format wrappers copy the
host's input audio into the output buffers before calling `process()`.
This means you can read and write `buffer.output(ch)` directly --
the output buffer already contains the input audio. You don't need
to manually copy from `buffer.input()` to `buffer.output()`.

For instruments, the output buffers start at zero (no input audio).

### Basic effect (per-sample)

```rust
fn process(
    &mut self,
    buffer: &mut AudioBuffer,
    _events: &EventList,
    _ctx: &mut ProcessContext,
) -> ProcessStatus {
    for ch in 0..buffer.num_output_channels() {
        let output = buffer.output(ch);
        for i in 0..buffer.num_samples() {
            output[i] = self.process_sample(ch, output[i]);
        }
    }
    ProcessStatus::Normal
}
```

### Using input/output pairs

```rust
fn process(
    &mut self,
    buffer: &mut AudioBuffer,
    _events: &EventList,
    _ctx: &mut ProcessContext,
) -> ProcessStatus {
    for ch in 0..buffer.num_output_channels() {
        let (input, output) = buffer.io_pair(ch, ch);
        for i in 0..buffer.num_samples() {
            output[i] = input[i] * self.gain_linear;
        }
    }
    ProcessStatus::Normal
}
```

### Handling MIDI events

```rust
fn process(
    &mut self,
    buffer: &mut AudioBuffer,
    events: &EventList,
    _ctx: &mut ProcessContext,
) -> ProcessStatus {
    for event in events.iter() {
        match &event.body {
            EventBody::NoteOn { note, velocity, .. } => {
                self.trigger_envelope(*note, *velocity);
            }
            EventBody::NoteOff { note, .. } => {
                self.release_envelope(*note);
            }
            EventBody::ControlChange { cc, value, .. } => {
                if *cc == 1 {  // mod wheel
                    self.mod_depth = *value;
                }
            }
            _ => {}
        }
    }

    // Process audio with the updated state
    for ch in 0..buffer.num_output_channels() {
        let output = buffer.output(ch);
        for i in 0..buffer.num_samples() {
            output[i] *= self.envelope_value();
            self.advance_envelope();
        }
    }

    ProcessStatus::Normal
}
```

### Sample-accurate event processing

When you need events applied at the exact sample they occur
(important for tight synth timing, transient shapers, etc.):

```rust
fn process(
    &mut self,
    buffer: &mut AudioBuffer,
    events: &EventList,
    _ctx: &mut ProcessContext,
) -> ProcessStatus {
    let mut next_event_idx = 0;

    for i in 0..buffer.num_samples() {
        // Process all events at this sample offset
        while let Some(event) = events.get(next_event_idx) {
            if event.sample_offset as usize > i {
                break;
            }
            self.handle_event(&event.body);
            next_event_idx += 1;
        }

        // Process this sample
        for ch in 0..buffer.num_output_channels() {
            buffer.output(ch)[i] = self.render_sample(ch);
        }
    }

    ProcessStatus::Normal
}
```

### Using transport information

```rust
fn process(
    &mut self,
    buffer: &mut AudioBuffer,
    _events: &EventList,
    ctx: &mut ProcessContext,
) -> ProcessStatus {
    let transport = ctx.transport;

    if transport.playing {
        let beat_position = transport.position_beats;
        let tempo = transport.tempo;

        // Sync an LFO to the beat
        let lfo_phase = (beat_position * self.sync_rate) % 1.0;

        // Detect bar boundaries for rhythmic effects
        let bar_length_beats = transport.time_sig_num as f64;
        let pos_in_bar = beat_position % bar_length_beats;

        // ...
    }

    ProcessStatus::Normal
}
```

### Declaring tail time

Effects like reverb and delay produce output after the input stops.
Tell the host how long your tail is so it doesn't cut you off:

```rust
fn process(..) -> ProcessStatus {
    // ... process audio ...

    if self.is_producing_silence() {
        // Tell the host: I have N samples of tail left,
        // then you can stop calling me
        ProcessStatus::Tail(self.remaining_tail_samples())
    } else {
        ProcessStatus::Normal
    }
}
```

---


---

[← Previous](04-parameters.md) | [Next →](06-channels.md) | [Index](README.md)
