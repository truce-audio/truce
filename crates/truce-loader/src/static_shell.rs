//! `StaticShell` - embeds the plugin directly into the binary.
//!
//! No dlopen, no file watcher, no Mutex. Same types as `HotShell`
//! but zero runtime overhead. Use via `export_static!`.

use std::sync::Arc;

use truce_core::buffer::AudioBuffer;
use truce_core::bus::BusLayout;
use truce_core::events::{EventBody, EventList};
use truce_core::info::PluginInfo;
use truce_core::meters::MeterStore;
use truce_core::plugin::PluginRuntime;
use truce_core::process::{ProcessContext, ProcessStatus};
use truce_core::snapshot::SnapshotSlot;
use truce_core::state::{ForeignState, MigratedState, StateLoadError};
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
    meters: Arc<MeterStore>,
    /// Lock-free publish slot for `snapshot_into`-based state save.
    snapshots: Arc<SnapshotSlot>,
    /// Stays `true` until the logic reports (via `snapshot_into`), on a
    /// block before it ever publishes, that it has no custom snapshot -
    /// after which per-block publishing is skipped so non-opt-in plugins
    /// pay nothing. A plugin that has published once stays subscribed.
    try_snapshot: bool,
    sample_rate: f64,
    _sample: std::marker::PhantomData<fn() -> S>,
}

// SAFETY: `StaticShell` owns `Arc<P>` (params, `Sync` by the
// `Params` trait contract), `L` (the user's logic - `Send + 'static`
// per the `PluginLogicCore` bound), an atomic-slot `MeterStore`,
// and a `PhantomData<fn() -> S>`. No raw pointers, no
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
            meters: MeterStore::new(),
            snapshots: SnapshotSlot::new(),
            try_snapshot: true,
            sample_rate: 44100.0,
            _sample: std::marker::PhantomData,
        }
    }

    /// Shared meter storage handle - the GUI-thread-safe channel
    /// for meter reads (see `PluginExport::meter_store`).
    pub fn meter_store(&self) -> Arc<MeterStore> {
        Arc::clone(&self.meters)
    }

    /// Shared snapshot slot for lock-free state save (see
    /// `PluginExport::snapshot_slot`).
    pub fn snapshot_slot(&self) -> Arc<SnapshotSlot> {
        Arc::clone(&self.snapshots)
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
        let meter_fn = |id: u32, v: f32| meters.write(id, v);
        let mut ctx = ProcessContext::new(
            context.transport,
            context.sample_rate,
            buffer.num_samples(),
            &mut *context.output_events,
        )
        .with_params(&param_fn)
        .with_meters(&meter_fn);

        let status = self.logic.process(buffer, events, &mut ctx);
        publish_snapshot(&self.logic, &self.snapshots, &mut self.try_snapshot);
        status
    }

    fn save_state(&self) -> Vec<u8> {
        self.logic.save_state()
    }

    fn load_state(&mut self, data: &[u8]) -> Result<(), StateLoadError> {
        let result = self.logic.load_state(data);
        // Plugin-side cache invalidation runs in the same `&mut`
        // borrow window so the next `process()` block sees the
        // refreshed caches - fire it whether or not load_state
        // succeeded so partial state still triggers a refresh.
        PluginLogicCore::state_changed(&mut self.logic);
        result
    }

    fn migrate_state(foreign: &ForeignState) -> Option<MigratedState>
    where
        Self: Sized,
    {
        <L as PluginLogicCore<S>>::migrate_state(foreign)
    }

    fn latency(&self) -> u32 {
        self.logic.latency()
    }
    fn tail(&self) -> u32 {
        self.logic.tail()
    }

    fn get_meter(&self, meter_id: u32) -> f32 {
        self.meters.read(meter_id)
    }
}

/// Publish the plugin's `snapshot_into` bytes into `slot` on the audio
/// thread. Shared by both shells.
///
/// Opting into snapshots is a static capability: `try_snapshot` latches
/// off only when the logic reports "no snapshot" *before it has ever
/// published one* (the default `snapshot_into` returning false), so a
/// non-opt-in plugin stops paying after one block. Once a plugin has
/// published, it stays subscribed for its lifetime - a plugin that
/// returns true then later false is violating the contract, and we keep
/// calling it rather than silently latching off and serving stale bytes.
/// Never blocks: `SnapshotSlot::publish` skips on reader contention, in
/// which case the closure doesn't run and the latch is left alone.
pub(crate) fn publish_snapshot<S, L>(logic: &L, slot: &SnapshotSlot, try_snapshot: &mut bool)
where
    S: Sample,
    L: PluginLogicCore<S> + ?Sized,
{
    publish_snapshot_with(slot, try_snapshot, |buf| logic.snapshot_into(buf));
}

/// Latch logic behind [`publish_snapshot`], parameterized over the raw
/// `snapshot_into` closure so it can be unit-tested without a full
/// `PluginLogicCore` mock.
fn publish_snapshot_with(
    slot: &SnapshotSlot,
    try_snapshot: &mut bool,
    snapshot_into: impl FnOnce(&mut Vec<u8>) -> bool,
) {
    if !*try_snapshot {
        return;
    }
    let ran_unsupported = std::cell::Cell::new(false);
    slot.publish(|buf| {
        let wrote = snapshot_into(buf);
        ran_unsupported.set(!wrote);
        wrote
    });
    // First-block opt-out only: a plugin that has already published is
    // committed for its lifetime, so a later false never latches us off.
    if ran_unsupported.get() && !slot.is_supported() {
        *try_snapshot = false;
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

            fn migrate_state(
                foreign: &$crate::__macro_deps::truce_core::state::ForeignState,
            ) -> Option<$crate::__macro_deps::truce_core::state::MigratedState>
            where
                Self: Sized,
            {
                <$logic as $crate::__macro_deps::truce_plugin::PluginLogicCore<Sample>>::migrate_state(foreign)
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

            fn meter_store(
                &self,
            ) -> std::sync::Arc<$crate::__macro_deps::truce_core::meters::MeterStore> {
                self.inner.meter_store()
            }

            fn snapshot_slot(
                &self,
            ) -> std::sync::Arc<$crate::__macro_deps::truce_core::snapshot::SnapshotSlot> {
                self.inner.snapshot_slot()
            }

            fn editor_builder(
                &self,
            ) -> $crate::__macro_deps::truce_core::editor::EditorBuilder<$params> {
                // Builds from the lock-free param store, never the
                // embedded logic - the audio thread's `&mut logic` is
                // irrelevant here, so opening the editor takes no lock.
                Box::new(|params| {
                    Some(
                        <$logic as $crate::__macro_deps::truce_plugin::PluginEditor<Sample>>::editor(
                            params,
                        ),
                    )
                })
            }
        }
    };
}

#[cfg(test)]
mod tests {
    use super::publish_snapshot_with;
    use truce_core::snapshot::SnapshotSlot;

    #[test]
    fn non_opt_in_latches_off_on_first_block() {
        let slot = SnapshotSlot::new();
        let mut try_snapshot = true;

        // Default `snapshot_into` (returns false) before any publish:
        // latch off so we stop paying every block.
        publish_snapshot_with(&slot, &mut try_snapshot, |_| false);
        assert!(!try_snapshot, "first false must latch off");
        assert!(!slot.is_supported());

        // Subsequent blocks short-circuit and never call the closure.
        let mut called = false;
        publish_snapshot_with(&slot, &mut try_snapshot, |_| {
            called = true;
            false
        });
        assert!(!called, "latched-off slot must not call snapshot_into");
    }

    #[test]
    fn opt_in_then_contract_violation_stays_subscribed() {
        let slot = SnapshotSlot::new();
        let mut try_snapshot = true;

        // Block 1: plugin publishes - it has opted in for its lifetime.
        publish_snapshot_with(&slot, &mut try_snapshot, |buf| {
            buf.clear();
            buf.extend_from_slice(&[1, 2, 3]);
            true
        });
        assert!(try_snapshot);
        assert!(slot.is_supported());
        assert_eq!(slot.read(), Some(vec![1, 2, 3]));

        // Block 2: a contract-violating false must NOT latch us off - we
        // keep calling the plugin rather than silently going dark.
        publish_snapshot_with(&slot, &mut try_snapshot, |_| false);
        assert!(try_snapshot, "a post-opt-in false must not latch off");

        // Block 3: still subscribed, so a fresh publish still lands.
        publish_snapshot_with(&slot, &mut try_snapshot, |buf| {
            buf.clear();
            buf.extend_from_slice(&[4]);
            true
        });
        assert_eq!(slot.read(), Some(vec![4]));
    }
}
