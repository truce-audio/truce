## Understanding the Plugin trait

Here is every method you can implement. Only `info()`, `reset()`,
and `process()` are required.

```rust
pub trait Plugin: Send + 'static {
    /// Plugin identity -- returned as a struct literal.
    fn info() -> PluginInfo where Self: Sized;

    /// Supported channel layouts. The host picks one.
    /// Default: stereo in/out.
    fn bus_layouts() -> Vec<BusLayout> where Self: Sized {
        vec![BusLayout::stereo()]
    }

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
        context: &ProcessContext,
    ) -> ProcessStatus;

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

    /// GUI editor. Return None for headless plugins.
    fn editor(&mut self) -> Option<Box<dyn Editor>> { None }
}
```

Note that the `Plugin` trait does **not** have an associated
`type Params`. Parameters are wired up through the `PluginExport`
trait, which declares `type Params: Params`. Format shells store
params as `Arc<P>` — GUI `EditorContext` closures capture clones
of the `Arc` instead of raw pointers, and `PluginExport` provides
a `params_arc()` method to access the shared `Arc<P>`.

### Lifecycle

Understanding when each method is called:

```
Host loads plugin binary
    |
    +-- Format wrapper reads PluginInfo (your IDs, name, etc.)
    +-- Format wrapper accesses params via PluginExport
    |
    v
PluginExport::create()        <-- your struct is constructed
    |
    v
Plugin::init()                <-- one-time setup (non-realtime)
    |
    v
Plugin::reset(44100, 512)     <-- sample rate and block size known
    |
    |   +----------------------------------------------+
    |   |          Main playback loop                  |
    |   |                                              |
    +-->|  Plugin::process(buffer, events, ctx)        |
    |   |  Plugin::process(buffer, events, ctx)        |
    |   |  Plugin::process(buffer, events, ctx)        |
    |   |  ...                                         |
    |   +----------------------------------------------+
    |
    |   (sample rate changes -> reset() called again)
    |
    |   +----------------------------------------------+
    |   |  Host saves session                          |
    |   |  -> Framework serializes all param values    |
    |   |  -> Plugin::save_state() for extra data      |
    |   +----------------------------------------------+
    |
    v
Plugin dropped                <-- host unloads the plugin
```

---


---

[← Previous](02-first-plugin.md) | [Next →](04-parameters.md) | [Index](README.md)
