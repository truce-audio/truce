# Audio Testing

Audio tests catch DSP regressions by running your plugin against
scripted input — silence, a constant tone, a buffer you generated,
or MIDI events — for a fixed duration, then asserting on the
captured output, meters, and emitted events. No DAW, no host, no
audio device — the driver instantiates the plugin, calls `process`
in a loop, and hands you a `DriverResult`.

The same engine powers three places:

- **Tests** via `truce-test::driver!` — adds assertion helpers on
  top.
- **Custom `main.rs` bins** — batch CI renders, demo audio, preset
  rendering pipelines.
- **`truce-standalone`'s offline path** —
  `cargo truce run --no-playback --input-file in.wav --output-file out.wav`
  parses CLI flags and feeds them to the driver.

If you need different lifecycles for each consumer, you'll get
silently-different bug surfaces — so they all share one driver.

## Quick start

Add `truce-test` to `[dev-dependencies]`:

```toml
[dev-dependencies]
truce-test = { workspace = true }
```

Drop a test into your `lib.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use truce_test::{InputSource, assertions, driver};

    #[test]
    fn passthrough() {
        let result = driver!(Plugin)
            .duration(Duration::from_millis(100))
            .input(InputSource::Constant(0.5))
            .run();
        assertions::assert_nonzero(&result);
        assertions::assert_no_nans(&result);
        assertions::assert_peak_below(&result, 1.0);
    }
}
```

The `driver!` macro builds a `PluginDriver<Plugin>` with
`manifest_dir` wired to your crate's `CARGO_MANIFEST_DIR` (so
`.state_file("…")` resolves against your crate, not wherever
`cargo test` was launched).

## How it works

A run is a builder followed by `.run()`:

1. `driver!(Plugin)` constructs a `PluginDriver<Plugin>` with
   defaults — 44.1 kHz, 2 channels, 512-sample blocks, 1 s,
   silence input, no script, audio + final-meter capture.
2. Builder methods configure the run: sample rate, duration,
   input, transport, MIDI / param-automation script, state-file
   load, param overrides, setup closure, capture spec.
3. `.run()` does the lifecycle in this order:
    1. `P::create()` → `init()` → `reset(sr, block)` →
       `params().set_sample_rate(sr)` → `params().snap_smoothers()`.
    2. If `state_file` was supplied, `plugin.load_state(&bytes)`.
    3. `set_param` shortcuts apply via
       `params().set_normalized(id, v)`, then `snap_smoothers()`.
    4. The `setup` closure runs (most general escape hatch).
    5. Block loop: pull script events that fall inside
       `[cursor, cursor+block_len)` into an `EventList`,
       allocate per-block channel buffers from
       [`InputSource`], call `plugin.process(buffer, events,
       ctx)`, append the output, advance.
    6. Capture meters / output events / param snapshots
       according to `CaptureSpec`.
4. The `DriverResult` carries the captured data plus the
   post-run plugin instance — read params or custom state from
   `result.plugin` for assertions.

## Inputs

The `InputSource` enum covers the four common shapes:

- `InputSource::Silence` — every channel zero (default).
- `InputSource::Constant(0.5)` — DC on every channel.
- `InputSource::Buffer(channel_major)` — `Vec<Vec<f32>>`,
  `bufs[ch][frame]`. Channel count must match the driver's.
- `InputSource::Generator(Box::new(|frame, sr| …))` — for sweeps,
  noise, derived signals.

Instruments ignore `input` — set their notes via
`.script(|s| …)`.

## Scripted MIDI and automation

`Script` advances a sample-accurate cursor; events land at the
current offset. `wait_ms(0)` is a no-op (events stack). The
script's `set_param` emits a `ParamChange` event the plugin sees
inline — same delivery path CLAP / VST3 / AU automation lanes
use.

```rust
#[test]
fn note_release_decays_to_silence() {
    use std::time::Duration;
    use truce_test::{assertions, driver};

    let result = driver!(MyInstrument)
        .duration(Duration::from_secs(2))
        .script(|s| {
            s.note_on(60, 0.8);
            s.wait_ms(500);
            s.note_off(60);
        })
        .run();

    assertions::assert_nonzero(&result);
    assertions::assert_silence_after(&result, Duration::from_millis(1_500));
}
```

For *automation during a run*, use the script:

```rust
.script(|s| {
    s.set_param(MyParamId::Gain, 0.0);
    s.wait_ms(500);
    s.set_param(MyParamId::Gain, 1.0); // ramps mid-run
})
```

For *one-shot params before the run starts*, use the builder
shortcut:

```rust
.set_param(MyParamId::Gain, 0.7)
```

`.set_param` calls compose; multiple are applied in declaration
order, before the `setup` closure.

## State files

The standalone host's `Cmd+S` / `Ctrl+S` writes a `.pluginstate`
file — your full session state at that moment. Feed it to the
driver via `state_file`:

```rust
#[test]
fn evening_preset_still_audible() {
    use std::time::Duration;
    use truce_test::{InputSource, assertions, driver};

    let result = driver!(MyEffect)
        .state_file("test_states/evening.pluginstate")
        .input(InputSource::Constant(0.3))
        .duration(Duration::from_millis(500))
        .run();

    assertions::assert_nonzero(&result);
}
```

Path is relative to your crate's `Cargo.toml` directory (the
`driver!` macro wires that for you), or absolute. Loaded via
`plugin.load_state(&bytes)` — the same path CLAP / VST3 / AU
hosts use to restore session state.

State load runs *before* `set_param` overrides and the `setup`
closure, so you can use the state as a baseline and tweak from
there:

```rust
driver!(MyEffect)
    .state_file("test_states/evening.pluginstate")
    .set_param(MyParamId::Gain, 1.0)        // override gain only
    .setup(|p, _ctx| p.custom_field = 42)   // tweak custom state
    .run();
```

The second arg is a [`SetupContext`] carrying the resolved channel
count, sample rate, and block size — useful when the closure needs
to size per-channel scratch:

```rust
driver!(MyEffect)
    .channels(4)
    .setup(|p, ctx| {
        p.scratch = vec![0.0; ctx.block_size * ctx.channels];
        assert_eq!(ctx.channels, 4);
    })
    .run();
```

## Captured meters and output events

By default the driver captures audio + a final meter snapshot
(per-meter readings at end-of-run). Opt into per-block snapshots
or output-event capture as needed:

```rust
let result = driver!(Plugin)
    .duration(Duration::from_secs(1))
    .capture_meters(MeterCapture::PerBlock)
    .capture_output_events(true)
    .run();

assertions::assert_meter_above(&result, MyParamId::Output as u32, 0.1);
assertions::assert_output_event_count(&result, 4);
```

Meters keyed by their parameter ID; the driver pulls
`plugin.params().meter_ids()` and `plugin.get_meter(id)` in the
order the params were declared.

## Assertions

`truce_test::assertions::*` covers the standard claims. They all
take `&DriverResult<P>`:

| Whole-run | What it checks |
|---|---|
| `assert_nonzero` | At least one sample > audible threshold |
| `assert_silence` | Every sample < audible threshold |
| `assert_no_nans` | No NaN / Inf — DSP didn't go divergent |
| `assert_peak_below(t)` | No sample exceeds `t` (clipping guard) |

| Time-windowed | What it checks |
|---|---|
| `assert_silence_after(t)` | Tail after `t` is silent (decay tests) |
| `assert_nonzero_after(t)` | At least one sample after `t` |
| `assert_silence_between(s, e)` | `[s, e)` is silent (gate tests) |
| `assert_nonzero_between(s, e)` | At least one sample in `[s, e)` |

| Meter / events | What it checks |
|---|---|
| `assert_meter_above(id, t)` | Meter `id` final reading > `t` |
| `assert_meter_below(id, t)` | Meter `id` final reading < `t` |
| `assert_output_event_count(n)` | Plugin emitted exactly `n` events |

For anything custom, walk `result.output[ch][frame]` directly —
the captured audio is plain `Vec<Vec<f32>>`.

## Reading post-run state

`DriverResult::plugin` holds the post-run plugin instance, so
assertions can inspect param values, meters, or custom fields
the framework doesn't know about:

```rust
let result = driver!(MyEffect)
    .duration(Duration::from_secs(1))
    .input(InputSource::Constant(0.5))
    .run();

let final_gain = result.plugin.params().get_plain(MyParamId::Gain as u32).unwrap();
assert!((final_gain - 0.7).abs() < 1e-3);
```

## Writing captured audio to disk

For preset rendering or visual debugging, the driver can write
the captured audio out as a 32-bit float WAV. Enable the `wav`
feature on `truce-driver` (already on whenever `truce-standalone`
has `playback`):

```rust
let result = driver!(MyEffect)
    .duration(Duration::from_secs(4))
    .input(InputSource::Buffer(decode_wav("in.wav")))
    .run();
result.write_wav("out.wav")?;
```

Same engine the standalone host's offline path uses — your test
output and your CI render matrix produce byte-identical results.

## API surface

```rust
truce_test::driver!($plugin:ty)
// → truce_test::PluginDriver::<$plugin>::new()
//       .manifest_dir(env!("CARGO_MANIFEST_DIR"))
```

```rust
impl<P: PluginExport> PluginDriver<P> {
    // Run shape
    pub fn sample_rate(self, sr: f64) -> Self;
    pub fn channels(self, n: usize) -> Self;
    pub fn block_size(self, n: usize) -> Self;
    pub fn duration(self, d: Duration) -> Self;
    pub fn transport(self, t: TransportSpec) -> Self;
    pub fn bpm(self, bpm: f64) -> Self;
    pub fn playing(self, playing: bool) -> Self;

    // Input
    pub fn input(self, source: InputSource) -> Self;

    // Pre-run mutations (apply in order: state_file → set_param → setup)
    pub fn state_file(self, path: impl Into<PathBuf>) -> Self;
    pub fn set_param(self, id: impl Into<u32>, normalized: f32) -> Self;
    pub fn setup<F: FnOnce(&mut P) + 'static>(self, f: F) -> Self;

    // In-run automation / events
    pub fn script(self, f: impl FnOnce(&mut Script)) -> Self;

    // What to capture
    pub fn capture_audio(self, on: bool) -> Self;
    pub fn capture_meters(self, m: MeterCapture) -> Self;
    pub fn capture_output_events(self, on: bool) -> Self;
    pub fn capture_block_snapshots(self, on: bool) -> Self;

    pub fn run(self) -> DriverResult<P>;
}
```

```rust
pub struct DriverResult<P> {
    pub output: Vec<Vec<f32>>,         // channel-major
    pub sample_rate: f64,
    pub block_size: usize,
    pub total_frames: usize,
    pub meters: MeterReadings,
    pub output_events: Vec<Event>,
    pub block_snapshots: Vec<Vec<(u32, f64)>>,
    pub plugin: P,                     // post-run instance
}
```

```rust
pub enum InputSource {
    Silence,
    Constant(f32),
    Buffer(Vec<Vec<f32>>),
    Generator(Box<dyn FnMut(usize, f64) -> f32>),
}
```

For the screenshot equivalent (GUI regression tests), see
[`gui/screenshot-testing.md`](gui/screenshot-testing.md). Both
builders share the same `state_file` / `set_param` / `setup`
ordering so the same vocabulary works for audio + GUI.
