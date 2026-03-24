## Understanding the PluginLogic trait

`PluginLogic` is the trait you implement for your plugin. It covers
the runtime behavior: reset, process, layout, state, and tail/latency.
Only `reset()` and `process()` are required.

Note that `new()` is **not** part of the trait — it is an inherent
method on your plugin struct that takes `Arc<Params>`. The
`truce::plugin!` macro wires up construction automatically.

Static metadata (`info()`, `bus_layouts()`) is provided by the
`truce::plugin!` macro via `plugin_info!()` (reads `truce.toml` +
`Cargo.toml`) and a default stereo layout. Override `bus_layouts()`
in `PluginLogic` if you need a custom layout.

```rust
pub trait PluginLogic: Send + 'static {
    /// Called once after construction. Not real-time safe.
    fn init(&mut self) {}

    /// Called when sample rate or max block size changes.
    /// Reset filters, clear delay lines. Not real-time safe.
    fn reset(&mut self, sample_rate: f64, max_block_size: usize);

    /// Real-time audio processing.
    /// NEVER allocate, lock, or do I/O in here.
    fn process(
        &mut self,
        buffer: &mut AudioBuffer,
        events: &EventList,
        context: &mut ProcessContext,
    ) -> ProcessStatus;

    /// Supported channel layouts. The host picks one.
    /// Default: stereo in/out.
    fn bus_layouts() -> Vec<BusLayout> where Self: Sized {
        vec![BusLayout::stereo()]
    }

    /// Processing latency in samples (for delay compensation).
    /// Return 0 if the plugin adds no latency.
    fn latency(&self) -> u32 { 0 }

    /// Tail time in samples (reverb/delay tails).
    /// Return 0 for no tail. Return u32::MAX for infinite tail.
    fn tail(&self) -> u32 { 0 }

    /// Save extra state beyond parameter values.
    fn save_state(&self) -> Option<Vec<u8>> { None }

    /// Restore extra state.
    fn load_state(&mut self, data: &[u8]) {}

    /// GUI layout for the built-in renderer.
    fn layout(&self) -> truce_gui::layout::PluginLayout { ... }

    /// Custom GUI editor. Return None to use the built-in layout.
    fn custom_editor(&mut self) -> Option<Box<dyn Editor>> { None }
}
```

The plugin struct holds `Arc<Params>` (shared with the format
shell). The shell owns the `Arc` and passes a clone to `new()`.
GUI closures capture clones of the same `Arc` — no raw pointers.

### Lifecycle

Understanding when each method is called:

```
Host loads plugin binary
    |
    +-- truce::plugin! reads PluginInfo (from truce.toml + Cargo.toml)
    +-- Shell creates Arc<Params> and shares it
    |
    v
YourPlugin::new(params)       <-- your struct is constructed (inherent method)
    |
    v
PluginLogic::init()           <-- one-time setup (non-realtime)
    |
    v
PluginLogic::reset(44100, 512) <-- sample rate and block size known
    |
    |   +----------------------------------------------+
    |   |          Main playback loop                  |
    |   |                                              |
    +-->|  PluginLogic::process(buffer, events, ctx)   |
    |   |  PluginLogic::process(buffer, events, ctx)   |
    |   |  PluginLogic::process(buffer, events, ctx)   |
    |   |  ...                                         |
    |   +----------------------------------------------+
    |
    |   (sample rate changes -> reset() called again)
    |
    |   +----------------------------------------------+
    |   |  Host saves session                          |
    |   |  -> Framework serializes all param values    |
    |   |  -> PluginLogic::save_state() for extra data |
    |   +----------------------------------------------+
    |
    v
Plugin dropped                <-- host unloads the plugin
```

---


---

[← Previous](02-first-plugin.md) | [Next →](04-parameters.md) | [Index](README.md)
