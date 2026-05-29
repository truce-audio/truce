//! `StaticShell` - embeds the plugin directly into the binary.
//!
//! No dlopen, no file watcher, no Mutex. Same types as `HotShell`
//! but zero runtime overhead. Use via `export_static!`.

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use truce_core::buffer::AudioBuffer;
use truce_core::bus::BusLayout;
use truce_core::editor::Editor;
use truce_core::events::{EventBody, EventList};
use truce_core::info::PluginInfo;
use truce_core::plugin::PluginRuntime;
use truce_core::preset::FactoryPresetInfo;
use truce_core::process::{ProcessContext, ProcessStatus};
use truce_params::Params;
use truce_params::sample::Sample;
use truce_plugin::PluginLogicCore;

// ---------------------------------------------------------------------------
// StaticShell
// ---------------------------------------------------------------------------

/// A static plugin shell that embeds the user's `PluginLogic` impl
/// directly into the format-wrapper binary.
///
/// Same bridging as `HotShell` but without `NativeLoader`, `Mutex`,
/// file watching, or any dynamic loading overhead. Use via `export_static!`.
pub struct StaticShell<P: Params, L: PluginLogicCore<S>, S: Sample = f32> {
    pub params: Arc<P>,
    logic: L,
    meters: Arc<[AtomicU32; 256]>,
    sample_rate: f64,
    _sample: std::marker::PhantomData<fn() -> S>,
}

// SAFETY: `StaticShell` owns `Arc<P>` (params, `Sync` by the
// `Params` trait contract), `L` (the user's logic - `Send + 'static`
// per the `PluginLogicCore` bound), an `AtomicU32`-backed meters
// array, and a `PhantomData<fn() -> S>`. No raw pointers, no
// `!Send` fields, no interior mutability that escapes the shell's
// own `&mut` borrows. The host contract that format wrappers
// invoke methods on a single thread at a time per instance is what
// keeps the embedded `L` safe to access without an inner mutex -
// same model `HotShell` uses through `parking_lot::Mutex`.
unsafe impl<P: Params, L: PluginLogicCore<S>, S: Sample> Send for StaticShell<P, L, S> {}

impl<P: Params + Default + 'static, L: PluginLogicCore<S> + 'static, S: Sample>
    StaticShell<P, L, S>
{
    /// Create from pre-constructed parts. The plugin logic should
    /// hold an `Arc::clone` of the same params.
    pub fn from_parts(params: Arc<P>, logic: L) -> Self {
        Self {
            params,
            logic,
            meters: Arc::new(std::array::from_fn(|_| AtomicU32::new(0))),
            sample_rate: 44100.0,
            _sample: std::marker::PhantomData,
        }
    }

    /// Access the plugin logic (for testing).
    pub fn logic_ref(&self) -> &L {
        &self.logic
    }

    /// Mutable access to the plugin logic (for testing).
    pub fn logic_ref_mut(&mut self) -> &mut L {
        &mut self.logic
    }
}

impl<P: Params + Default + 'static, L: PluginLogicCore<S> + 'static, S: Sample> PluginRuntime
    for StaticShell<P, L, S>
{
    type Sample = S;

    fn info() -> PluginInfo
    where
        Self: Sized,
    {
        unreachable!("StaticShell::info() should not be called statically")
    }

    fn bus_layouts() -> Vec<BusLayout>
    where
        Self: Sized,
    {
        unreachable!("StaticShell::bus_layouts() should not be called statically")
    }

    fn factory_presets_static() -> Vec<FactoryPresetInfo>
    where
        Self: Sized,
    {
        unreachable!("StaticShell::factory_presets_static() should not be called statically")
    }

    fn init(&mut self) {}

    fn reset(&mut self, sample_rate: f64, max_block_size: usize) {
        self.sample_rate = sample_rate;
        self.params.set_sample_rate(sample_rate);
        self.logic.reset(sample_rate, max_block_size);
    }

    fn process(
        &mut self,
        buffer: &mut AudioBuffer<S>,
        events: &EventList,
        context: &mut ProcessContext,
    ) -> ProcessStatus {
        // Apply parameter change events to the shell's params.
        // ParamChange values from format wrappers are PLAIN (already
        // denormalized). `set_normalized` here would double-denormalize.
        for e in events.iter() {
            if let EventBody::ParamChange { id, value } = &e.body {
                self.params.set_plain(*id, *value);
            }
        }

        // No sync needed - plugin reads from the same Arc<Params>.

        // Build a ProcessContext with param/meter callbacks for the logic.
        let params = &self.params;
        let meters = &self.meters;
        let param_fn = |id: u32| -> f64 { params.get_plain(id).unwrap_or(0.0) };
        let meter_fn = |id: u32, v: f32| {
            // Meter IDs are offset by `truce_params::METER_ID_BASE`;
            // mirror the offset in `get_meter` exactly.
            let idx = id.wrapping_sub(truce_params::METER_ID_BASE) as usize;
            if let Some(slot) = meters.get(idx) {
                slot.store(v.to_bits(), Ordering::Relaxed);
            }
        };
        let mut ctx = ProcessContext::new(
            context.transport,
            context.sample_rate,
            buffer.num_samples(),
            &mut *context.output_events,
        )
        .with_params(&param_fn)
        .with_meters(&meter_fn);

        self.logic.process(buffer, events, &mut ctx)
    }

    fn save_state(&self) -> Vec<u8> {
        self.logic.save_state()
    }

    fn load_state(&mut self, data: &[u8]) -> Result<(), truce_core::state::StateLoadError> {
        let result = self.logic.load_state(data);
        // Plugin-side cache invalidation runs in the same `&mut`
        // borrow window so the next `process()` block sees the
        // refreshed caches - fire it whether or not load_state
        // succeeded so partial state still triggers a refresh.
        PluginLogicCore::state_changed(&mut self.logic);
        result
    }

    fn load_factory_preset(&self, preset_number: i32) -> bool {
        self.logic.load_factory_preset(preset_number)
    }

    fn editor(&mut self) -> Option<Box<dyn Editor>> {
        Some(PluginLogicCore::editor(&self.logic))
    }

    fn latency(&self) -> u32 {
        self.logic.latency()
    }
    fn tail(&self) -> u32 {
        self.logic.tail()
    }

    fn get_meter(&self, meter_id: u32) -> f32 {
        // Meter IDs live in a dedicated high range starting at
        // `truce_params::METER_ID_BASE`; storage is offset into
        // `self.meters`. `wrapping_sub` keeps out-of-range ids from
        // panicking - they fall through to the `get` -> None path.
        let idx = meter_id.wrapping_sub(truce_params::METER_ID_BASE) as usize;
        if let Some(slot) = self.meters.get(idx) {
            f32::from_bits(slot.load(Ordering::Relaxed))
        } else {
            0.0
        }
    }
}

// ---------------------------------------------------------------------------
// export_static! macro
// ---------------------------------------------------------------------------

/// Compile-time static embedding of a `PluginLogic` impl into the binary.
///
/// Produces a `__HotShellWrapper` struct that implements `Plugin + PluginExport`,
/// so format export macros (`export_clap!`, `export_vst3!`, etc.) work unchanged.
/// No dlopen, no file watcher, zero runtime overhead. Bus layouts come from
/// `<$logic as PluginLogic>::bus_layouts()` - override the trait method to
/// pick something other than the stereo default.
///
/// ```ignore
/// export_static! {
///     params: GainParams,
///     info: plugin_info!(...),
///     logic: Gain,
/// }
///
/// #[cfg(feature = "clap")]
/// truce_clap::export_clap!(__HotShellWrapper);
/// ```
#[macro_export]
macro_rules! export_static {
    (
        params: $params:ty,
        info: $info:expr,
        logic: $logic:ty,
    ) => {
        pub struct __HotShellWrapper {
            // `Sample` here resolves to the type alias the user
            // imported from a prelude (`prelude` / `prelude32` →
            // `f32`; `prelude64` → `f64`; `prelude64m` → `f32`). The
            // `PluginLogic<Sample>` bound on the user's impl must
            // match this, so the prelude is what picks the audio
            // buffer precision end-to-end.
            inner: $crate::static_shell::StaticShell<$params, $logic, Sample>,
        }

        impl $crate::__macro_deps::truce_core::plugin::PluginRuntime for __HotShellWrapper {
            type Sample = Sample;

            fn supports_in_place() -> bool
            where
                Self: Sized,
            {
                // `PluginLogicCore<Sample>` is the wrapper-facing
                // trait; the user impl'd one of the leaf traits
                // (`PluginLogic` / `PluginLogic64`), and the blanket
                // bridge defined alongside those traits in
                // `truce-plugin` makes them also satisfy
                // `PluginLogicCore<Sample>` automatically. Sample
                // resolves through the prelude alias in scope at the
                // macro call site.
                <$logic as $crate::__macro_deps::truce_plugin::PluginLogicCore<Sample>>::supports_in_place()
            }

            fn info() -> $crate::__macro_deps::truce_core::info::PluginInfo
            where
                Self: Sized,
            {
                $info
            }

            fn bus_layouts() -> Vec<$crate::__macro_deps::truce_core::bus::BusLayout>
            where
                Self: Sized,
            {
                <$logic as $crate::__macro_deps::truce_plugin::PluginLogicCore<Sample>>::bus_layouts()
            }

            fn factory_presets_static(
            ) -> Vec<$crate::__macro_deps::truce_core::preset::FactoryPresetInfo>
            where
                Self: Sized,
            {
                <$logic as $crate::__macro_deps::truce_plugin::PluginLogicCore<Sample>>::factory_presets_static()
            }

            fn init(&mut self) {
                self.inner.init();
            }

            fn reset(&mut self, sample_rate: f64, max_block_size: usize) {
                self.inner.reset(sample_rate, max_block_size);
            }

            fn process(
                &mut self,
                buffer: &mut $crate::__macro_deps::truce_core::buffer::AudioBuffer<Sample>,
                events: &$crate::__macro_deps::truce_core::events::EventList,
                context: &mut $crate::__macro_deps::truce_core::process::ProcessContext,
            ) -> $crate::__macro_deps::truce_core::process::ProcessStatus {
                self.inner.process(buffer, events, context)
            }

            fn save_state(&self) -> Vec<u8> {
                self.inner.save_state()
            }

            fn load_state(
                &mut self,
                data: &[u8],
            ) -> Result<(), $crate::__macro_deps::truce_core::state::StateLoadError> {
                self.inner.load_state(data)
            }

            fn load_factory_preset(&self, preset_number: i32) -> bool {
                self.inner.load_factory_preset(preset_number)
            }

            fn editor(
                &mut self,
            ) -> Option<Box<dyn $crate::__macro_deps::truce_core::editor::Editor>> {
                self.inner.editor()
            }

            fn latency(&self) -> u32 {
                self.inner.latency()
            }
            fn tail(&self) -> u32 {
                self.inner.tail()
            }
            fn get_meter(&self, meter_id: u32) -> f32 {
                self.inner.get_meter(meter_id)
            }
        }

        impl $crate::__macro_deps::truce_core::export::PluginExport for __HotShellWrapper {
            type Params = $params;

            fn create() -> Self {
                let params = std::sync::Arc::new(<$params>::new());
                let logic = <$logic>::new(std::sync::Arc::clone(&params));
                Self {
                    inner: $crate::static_shell::StaticShell::from_parts(params, logic),
                }
            }

            fn params(&self) -> &$params {
                &self.inner.params
            }

            fn params_arc(&self) -> std::sync::Arc<$params> {
                std::sync::Arc::clone(&self.inner.params)
            }
        }
    };
}
