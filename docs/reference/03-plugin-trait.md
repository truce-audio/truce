## Understanding the PluginLogic trait

`PluginLogic` is the trait you implement for your plugin. It covers
the runtime behavior: reset, process, layout, state, and tail/latency.
Only `reset()` and `process()` are required.

Note that `new()` is **not** part of the trait — it is an inherent
method on your plugin struct that takes `Arc<Params>`. The
`truce::plugin!` macro wires up construction automatically.

Static metadata (`info()`, `bus_layouts()`) is provided by the
`truce::plugin!` macro via `plugin_info!()` (reads `truce.toml` +
`Cargo.toml`) and a default stereo layout. Override bus layouts via
the `plugin!` macro's `bus_layouts:` field (not on `PluginLogic`).

```rust
pub trait PluginLogic: Send + 'static {
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

    /// Render the GUI into the backend.
    /// Default: no-op. Override only for custom visuals.
    fn render(&self, backend: &mut dyn RenderBackend) {}

    /// Whether this plugin uses a custom render() implementation.
    /// If false (default), the shell uses BuiltinEditor with
    /// standard widget drawing from layout().
    fn uses_custom_render(&self) -> bool { false }

    /// Return the widget layout for the built-in GUI.
    fn layout(&self) -> truce_gui::layout::GridLayout { ... }

    /// Hit test: which widget (if any) is at (x, y)?
    fn hit_test(&self, widgets: &[WidgetRegion], x: f32, y: f32) -> Option<usize> { ... }

    /// Processing latency in samples (for delay compensation).
    /// Return 0 if the plugin adds no latency.
    fn latency(&self) -> u32 { 0 }

    /// Tail time in samples (reverb/delay tails).
    /// Return 0 for no tail. Return u32::MAX for infinite tail.
    fn tail(&self) -> u32 { 0 }

    /// Save extra state beyond parameter values.
    fn save_state(&self) -> Vec<u8> { Vec::new() }

    /// Restore extra state.
    fn load_state(&mut self, data: &[u8]) {}

    /// Custom GUI editor. Return None to use the built-in layout.
    fn custom_editor(&self) -> Option<Box<dyn Editor>> { None }
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
Plugin::init()                <-- one-time setup (non-realtime, on Plugin trait)
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
