# 3. Plugin anatomy

How the pieces of a truce plugin fit together: the `PluginLogic`
trait, the `truce::plugin!` macro, bus layouts, and state
persistence.

If you've walked through [first-plugin.md](first-plugin.md) this
chapter explains **why** the code you just wrote is shaped the way
it is.

## The moving parts

```
                                    ┌─ truce.toml (metadata)
                                    ↓
                              truce::plugin! macro
                              ────────────────────
                              generates:
                                ↓
  YourParams ◄──Arc───── Shell (one of CLAP / VST3 / AU / …)
  (atomic params)         │
                          ├─ calls YourPlugin::new(params)    (inherent)
                          ├─ calls PluginLogic::reset(sr, bs) (your code)
                          ├─ calls PluginLogic::process(…)    (audio thread)
                          ├─ calls PluginLogic::layout(…)     (main thread)
                          ├─ calls PluginLogic::save_state(…) (main thread)
                          └─ drops when unloaded
```

Three things you write:

1. A **params struct** with `#[derive(Params)]`.
2. A **plugin struct** with an inherent `new(params: Arc<P>)` and
   an `impl PluginLogic`.
3. A **single `truce::plugin!` macro call** that wires those into
   every plugin format.

Everything else — parameter hosting, GUI event dispatch, state
envelope, format-specific lifecycle, hot-reload shell — is
generated.

## The `PluginLogic` trait

Only `reset` and `process` are required. Everything else has a
default. Override what you need.

```rust
pub trait PluginLogic: Send + 'static {
    fn reset(&mut self, sample_rate: f64, max_block_size: usize);

    fn process(
        &mut self,
        buffer: &mut AudioBuffer,
        events: &EventList,
        context: &mut ProcessContext,
    ) -> ProcessStatus;

    fn layout(&self) -> truce_gui::layout::GridLayout { ... }
    fn render(&self, backend: &mut dyn RenderBackend) {}   // custom visuals
    fn uses_custom_render(&self) -> bool { false }
    fn hit_test(&self, widgets: &[WidgetRegion], x: f32, y: f32) -> Option<usize> { ... }

    fn save_state(&self) -> Vec<u8> { Vec::new() }
    fn load_state(&mut self, data: &[u8]) {}

    fn latency(&self) -> u32 { 0 }
    fn tail(&self) -> u32 { 0 }

    fn custom_editor(&self) -> Option<Box<dyn Editor>> { None }
}
```

### What each method is for

| Method | When called | Real-time? | Notes |
|--------|-------------|------------|-------|
| `reset` | Sample rate or block size changes; before the first `process` | no | Clear delay lines, reset filter state, call `params.set_sample_rate` + `snap_smoothers`. |
| `process` | Every audio block | **yes** — no alloc / lock / I/O | The audio thread. See [processing.md](processing.md). |
| `layout` | Built-in GUI rebuild | no | Returns a `GridLayout` description of widgets. See [gui.md](gui.md). |
| `save_state` / `load_state` | Host saves/loads a session, recalls a preset, or copies the plugin | no | **Extra** state only — params are serialized automatically. |
| `latency` | Host bus reconfiguration | no | Samples of processing delay, for PDC. |
| `tail` | Host transport stop | no | Samples of audio produced after input stops (reverb, delay). |
| `render`, `uses_custom_render`, `hit_test`, `custom_editor` | Built-in GUI, when overridden | no | Escape hatches for custom visuals / editors. See [gui.md](gui.md). |

### Construction is not on the trait

`new()` is a plain inherent method on your plugin struct:

```rust
pub struct MyPlugin {
    params: Arc<MyParams>,
    extra_dsp_state: SomeFilter,
}

impl MyPlugin {
    pub fn new(params: Arc<MyParams>) -> Self {
        Self {
            params,
            extra_dsp_state: SomeFilter::default(),
        }
    }
}
```

The `truce::plugin!` macro calls `YourPlugin::new(arc_clone_of_params)`
once per plugin instance. It's plain Rust construction — not a
trait method — because the shell needs to hand you the shared
`Arc<Params>` at construction time, and trait methods can't do
that.

The same `Arc<Params>` lives on the shell too, and can be cloned
into GUI closures. One source of truth, no synchronization.

## Lifecycle

```
Host loads plugin binary
    │
    │   truce::plugin! has already:
    │     - read truce.toml via truce-build
    │     - emitted format entry points
    │     - wrapped MyPlugin into a format-specific shell
    │
    ▼
Shell creates Arc<MyParams>, clones it into:
    ├── the host-visible parameter tree
    └── MyPlugin::new(arc_clone)
    │
    ▼
PluginLogic::reset(sr, max_block)      ◄── sample rate and block size known
    │
    │   ┌──────────── playback loop ────────────┐
    │   │  process(buffer, events, ctx)  (audio thread)
    │   │  process(buffer, events, ctx)  (audio thread)
    │   │  layout() / render()           (main thread)
    │   │  host writes automation        (atomics)
    │   │  …                                    │
    │   └─────────────────────────────────────────┘
    │
    │   (user changes sample rate → reset called again)
    │   (host saves session → params auto-serialized + save_state called)
    │   (host loads session → load_state called, then reset, then process resumes)
    │
    ▼
MyPlugin dropped
```

## Bus layouts

Supported audio bus configurations go on the `truce::plugin!`
macro, not on the `PluginLogic` trait. The host picks one; the
others are rejected at bus-config time before `process` is ever
called.

### Default (stereo in, stereo out)

If you don't pass `bus_layouts:`, the macro defaults to stereo
effect routing:

```rust
truce::plugin! {
    logic: MyGain,
    params: MyGainParams,
    // bus_layouts omitted → [BusLayout::stereo()]
}
```

### Instrument (no audio input)

```rust
truce::plugin! {
    logic: MySynth,
    params: MySynthParams,
    bus_layouts: [BusLayout::new().with_output("Main", ChannelConfig::Stereo)],
}
```

### Multiple layouts (host picks)

```rust
truce::plugin! {
    logic: Widener,
    params: WidenerParams,
    bus_layouts: [
        BusLayout::new()
            .with_input("Main",  ChannelConfig::Mono)
            .with_output("Main", ChannelConfig::Stereo),
        BusLayout::stereo(),
    ],
}
```

### Sidechain

```rust
truce::plugin! {
    logic: SidechainComp,
    params: CompParams,
    bus_layouts: [
        BusLayout::new()
            .with_input("Main",      ChannelConfig::Stereo)
            .with_input("Sidechain", ChannelConfig::Stereo)
            .with_output("Main",     ChannelConfig::Stereo),
        BusLayout::stereo(),                  // fallback when no sidechain
    ],
}
```

Inside `process`, channels are flat-indexed across buses: with the
above layout, `buffer.input(0)` / `(1)` is main L/R and `(2)` /
`(3)` is sidechain L/R. Use `buffer.num_input_channels()` to detect
which layout the host selected.

## State persistence

**Parameter values are saved and restored automatically** by the
format wrappers. The only time you override `save_state` /
`load_state` is when you have state that isn't a parameter —
loaded sample paths, custom curves, view mode, selection,
anything else the user can change.

### Option A: `#[derive(State)]` — recommended

Define a state struct, derive binary serialization, and wire it
into `PluginLogic`:

```rust
#[derive(State, Default)]
pub struct MyExtraState {
    pub ir_file_path: String,
    pub view_mode: u8,
    pub selected_ids: Vec<u32>,
}

pub struct MyPlugin {
    params: Arc<MyParams>,
    extra: MyExtraState,
}

impl PluginLogic for MyPlugin {
    fn save_state(&self) -> Vec<u8> { self.extra.serialize() }

    fn load_state(&mut self, data: &[u8]) {
        if let Some(s) = MyExtraState::deserialize(data) {
            self.extra = s;
        }
    }
    // ... reset, process, layout ...
}
```

Supported field types: `u8`..`u64`, `i8`..`i64`, `f32`, `f64`,
`bool`, `String`, `Vec<T>`, `Option<T>`, and nested `State`
structs. **Forward-compatible**: adding fields later means old
state blobs still deserialize, with defaults for new fields.

### Option B: bring your own serializer

If you need a specific format — JSON for human-readable presets,
`bincode` for structs with third-party types — you can do the
bytes yourself:

```rust
fn save_state(&self) -> Vec<u8> {
    bincode::serialize(&self.extra).unwrap()
}

fn load_state(&mut self, data: &[u8]) {
    if let Ok(s) = bincode::deserialize::<MyExtraState>(data) {
        self.extra = s;
    }
}
```

### How it works

The framework wraps whatever you return from `save_state()` in a
binary envelope with a plugin-ID hash, a version field, and the
list of `(param_id, f64)` parameter values. On load, the envelope
is validated (rejects state saved by a different plugin) and
params are restored **before** `load_state()` is called. You only
ever see your extra blob.

If your plugin has no extra state — only `#[param]` fields and
meters — don't override `save_state` / `load_state` at all. The
defaults (`Vec::new()` / no-op) are fine.

### Editor state

If your editor reads extra state (e.g. a loaded IR path to draw a
waveform), it needs to know when state changes — preset recall,
undo, session load. Use `StateBinding<T>`:

```rust
struct MyEditor {
    state: StateBinding<MyExtraState>,
}

impl Editor for MyEditor {
    fn open(&mut self, parent: RawWindowHandle, context: EditorContext) {
        self.state = StateBinding::new(&context);
    }

    fn state_changed(&mut self) {
        self.state.sync();            // re-read from plugin
    }
}

// Reading:
let path = &self.state.get().ir_file_path;

// Writing (user renamed the instance):
self.state.update(|s| s.ir_file_path = new_path);
```

If your plugin is parameter-only (no custom editor, no extra
state), skip `StateBinding` — the built-in GUI polls parameters
every frame for free.

## What's next

- **[Chapter 4 → parameters.md](parameters.md)** — every attribute
  the derive macro accepts, plus meters and parameter groups.
- **[Chapter 5 → processing.md](processing.md)** — the shapes
  `process()` takes for effects, MIDI processors, and synths.
- **[Chapter 6 → gui.md](gui.md)** — the built-in widget set and
  when to reach for a framework backend.
