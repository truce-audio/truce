use crate::buffer::AudioBuffer;
use crate::bus::BusLayout;
use crate::config::AudioConfig;
use crate::events::EventList;
use crate::info::PluginInfo;
use crate::process::{ProcessContext, ProcessStatus};
use truce_params::sample::Sample;

/// The format-facing plugin runtime trait. **Plugin authors do NOT
/// implement this directly.**
///
/// `PluginRuntime` is the surface every format wrapper (CLAP, VST3,
/// VST2, LV2, AU, AAX) consumes. The `truce::plugin!` macro generates
/// an `impl PluginRuntime for __HotShellWrapper` from the user's
/// `truce_plugin::PluginLogic` impl, bridging the user-facing trait
/// into this GUI-free format-wrapper surface so `truce-core` doesn't
/// pull in `truce-gui` types.
///
/// What plugin authors implement instead:
///
/// ```ignore
/// impl truce::prelude::PluginLogic for MyPlugin {
///     type Params = MyPluginParams;
///     fn reset(&mut self, config: &AudioConfig) { /* ... */ }
///     fn process(&mut self, /* ... */) -> ProcessStatus { /* ... */ }
///     fn editor(params: Arc<MyPluginParams>) -> Box<dyn Editor> { /* ... */ }
/// }
///
/// truce::plugin! { logic: MyPlugin, params: MyPluginParams }
/// ```
///
/// The macro-emitted `impl PluginRuntime` routes each method directly
/// to the user's impl.
pub trait PluginRuntime: Send + 'static {
    /// The plugin's chosen audio sample precision. Either `f32` (the
    /// default - matches host wire format for nearly all formats) or
    /// `f64` (for plugins whose DSP path runs in `f64` end-to-end:
    /// high-order biquads, oscillator phase accumulators, long-running
    /// cumulative state).
    ///
    /// The format wrapper bridges between host buffer precision and
    /// `Self::Sample` at the block boundary - so the plugin's
    /// `process()` always receives `AudioBuffer<Self::Sample>`
    /// regardless of what the host sent. See
    /// `truce_core::RawBufferScratch` for the conversion machinery.
    ///
    /// Drive this from the prelude: `truce::prelude` / `truce::prelude32`
    /// implies `f32`, `truce::prelude64` implies `f64`. The
    /// `truce::plugin!` macro emits `type Sample = …;` based on
    /// which prelude is in scope at the macro call site.
    type Sample: Sample;

    /// Opt into zero-copy in-place I/O. When this returns `true`,
    /// the format wrapper skips its safety memcpy on host-aliased
    /// buffers and hands the plugin the raw shared memory through
    /// `AudioBuffer::in_out_mut(ch)`. The plugin must check
    /// `AudioBuffer::is_in_place(ch)` per channel before reading
    /// `input(ch)` - for in-place channels `input(ch)` returns an
    /// empty slice, and the data lives only in the shared buffer.
    ///
    /// Default `false`: the wrapper copies aliased inputs into scratch
    /// so `input(ch)` and `output(ch)` are always disjoint. Costs one
    /// memcpy per aliased channel per block (a few hundred KB/sec at
    /// audio rates) and lets plugin code stay format-agnostic.
    ///
    /// `where Self: Sized` so a `dyn PluginRuntime` trait object stays
    /// dyn-compatible - the format wrappers consume `P: PluginRuntime`
    /// generically and call the method statically.
    #[must_use]
    fn supports_in_place() -> bool
    where
        Self: Sized,
    {
        false
    }

    /// Static metadata about the plugin.
    ///
    /// Use `plugin_info!()` for zero-boilerplate (reads from truce.toml
    /// + Cargo.toml at compile time - no `build.rs` required).
    fn info() -> PluginInfo
    where
        Self: Sized;

    /// Supported bus layouts. The host picks one.
    #[must_use]
    fn bus_layouts() -> Vec<BusLayout>
    where
        Self: Sized,
    {
        vec![BusLayout::stereo()]
    }

    /// Called once after construction. Not real-time safe.
    fn init(&mut self) {}

    /// Called when sample rate, max block size, or processing mode
    /// changes. Reset filters, delay lines, etc., and size any
    /// mode-dependent buffers off `config.process_mode`. Not real-time
    /// safe.
    fn reset(&mut self, config: &AudioConfig);

    /// Real-time audio processing.
    fn process(
        &mut self,
        buffer: &mut AudioBuffer<Self::Sample>,
        events: &EventList,
        context: &mut ProcessContext,
    ) -> ProcessStatus;

    /// Save extra state beyond parameter values. Empty `Vec` means
    /// "no extra state": matches the user-facing
    /// `truce_plugin::PluginLogic::save_state` shape so the wrapper
    /// bridge is a passthrough rather than an `Option<Vec<u8>>` to
    /// `Vec<u8>` translation.
    ///
    /// **Concurrency contract.** Called on a host or GUI thread under
    /// the wrapper's plugin lock, so it never runs concurrently with
    /// `process()` - any field is safe to read. The flip side: an
    /// audio block that arrives mid-save waits for this to return, so
    /// keep it cheap (copy bytes out; don't compute or compress here).
    fn save_state(&self) -> Vec<u8> {
        Vec::new()
    }

    /// Restore extra state. Matches the user-facing
    /// `truce_plugin::PluginLogic::load_state` `Result` shape so the
    /// wrapper bridge is a passthrough.
    ///
    /// **Concurrency contract.** Called on the audio thread between
    /// blocks (the wrappers queue host loads and apply them at the
    /// top of `process()`), under the same exclusive access
    /// `process()` has - any field is safe to write.
    ///
    /// # Errors
    ///
    /// Returns `Err` when the macro-generated impl forwards a
    /// `PluginLogic::load_state` failure (malformed bytes, version
    /// skew between session file and plugin build, etc).
    fn load_state(&mut self, _data: &[u8]) -> Result<(), crate::state::StateLoadError> {
        Ok(())
    }

    /// Translate foreign state - a previous framework's blob, or a
    /// truce envelope saved under a different plugin id - into truce
    /// params + extra. Format wrappers call this when the host hands
    /// them state that isn't this plugin's envelope, so a plugin
    /// ported to truce can keep its users' old sessions and presets.
    ///
    /// Pure and receiverless by design: it runs synchronously on the
    /// host thread inside the wrapper's state callback (where parsing
    /// a large legacy blob belongs), and taking no `self` means it
    /// can't alias the audio thread's `&mut self`. The result rides
    /// the normal restore pipeline; the next save writes a regular
    /// envelope.
    ///
    /// Default: `None` - unrecognized state fails the load exactly
    /// as it did before this hook existed.
    #[must_use]
    fn migrate_state(_foreign: &crate::state::ForeignState) -> Option<crate::state::MigratedState>
    where
        Self: Sized,
    {
        None
    }

    /// Processing latency in samples. Host uses this for delay compensation.
    /// Return 0 if the plugin adds no latency (default).
    fn latency(&self) -> u32 {
        0
    }

    /// Tail time in samples. Return `u32::MAX` for infinite tail.
    /// Return 0 for no tail (default).
    fn tail(&self) -> u32 {
        0
    }

    /// Read a meter value by ID (0.0–1.0).
    ///
    /// **Concurrency contract.** Shell-internal: format wrappers'
    /// editor closures read meters through the shared
    /// [`crate::meters::MeterStore`] handle
    /// ([`crate::export::PluginExport::meter_store`]), never through
    /// this method, so it has no cross-thread caller. It remains on
    /// the trait for single-threaded consumers (the test driver, the
    /// standalone's locked instance).
    fn get_meter(&self, _meter_id: u32) -> f32 {
        0.0
    }
}
