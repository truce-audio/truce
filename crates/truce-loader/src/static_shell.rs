//! `StaticShell` - embeds the plugin directly into the binary.
//!
//! No dlopen, no file watcher, no Mutex. Same types as `HotShell`
//! but zero runtime overhead. Use via `export_static!`.

use std::sync::Arc;

use truce_core::buffer::AudioBuffer;
use truce_core::bus::BusLayout;
use truce_core::config::AudioConfig;
use truce_core::events::{EventBody, EventList};
use truce_core::info::PluginInfo;
use truce_core::meters::MeterStore;
use truce_core::plugin::PluginRuntime;
use truce_core::process::{ProcessContext, ProcessStatus};
use truce_core::snapshot::{SnapshotPublisher, SnapshotSlot};
use truce_core::state::{ForeignState, MigratedState, StateLoadError};
use truce_core::tasks::{AnyTaskSpawner, InitContext, warm_pool};
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
pub struct StaticShell<P: Params, L: PluginLogicCore<S, Params = P>, S: Sample = f32> {
    pub params: Arc<P>,
    /// The user's mutable DSP state, owned by the shell (not the
    /// descriptor `L`). Built once via `L::init(&params)`.
    state: L::DspState,
    meters: Arc<MeterStore>,
    /// Lock-free publish slot for `snapshot_into`-based state save.
    snapshots: Arc<SnapshotSlot>,
    /// Stays `true` until the logic reports (via `snapshot_into`), on a
    /// block before it ever publishes, that it has no custom snapshot -
    /// after which per-block publishing is skipped so non-opt-in plugins
    /// pay nothing. A plugin that has published once stays subscribed.
    try_snapshot: bool,
    /// Last `snapshot_version` the shell published. A block whose version
    /// matches this skips re-serialization entirely (see
    /// `publish_snapshot_with`). `None` until the first landed publish.
    last_snapshot_version: Option<u64>,
    sample_rate: f64,
    /// Background-task spawner bundle (one lane per declared task type),
    /// when the plugin wired `tasks:` on `plugin!`. Type-erased; stamped
    /// into each block's `ProcessContext` so `ctx.tasks::<T>()` works.
    /// `None` for a plugin with no background tasks.
    tasks: Option<AnyTaskSpawner>,
    _sample: std::marker::PhantomData<fn() -> S>,
}

// SAFETY: `StaticShell` owns `Arc<P>` (params, `Sync` by the
// `Params` trait contract), `L::DspState` (`Send + 'static` per the
// `PluginLogicCore` bound), an atomic-slot `MeterStore`, and a
// `PhantomData<fn() -> S>`. No raw pointers, no `!Send` fields, no
// interior mutability that escapes the shell's own `&mut` borrows. The
// host contract that format wrappers invoke methods on a single thread
// at a time per instance is what keeps the embedded state safe to
// access without an inner mutex - same model `HotShell` uses through
// `parking_lot::Mutex`.
unsafe impl<P: Params, L: PluginLogicCore<S, Params = P>, S: Sample> Send for StaticShell<P, L, S> {}

impl<P: Params + Default + 'static, L: PluginLogicCore<S, Params = P> + 'static, S: Sample>
    StaticShell<P, L, S>
{
    /// Build the shell from shared params, constructing the initial DSP
    /// state via `L::init(&params, &cx)`. The descriptor `L` is a
    /// type-only marker; the shell owns the state it produces. `tasks` is
    /// the plugin's background-task spawner (`Some` only when the plugin
    /// wired `tasks:` on `plugin!`); it reaches `init` through the
    /// `InitContext` and each block through the `ProcessContext`.
    pub fn from_parts(params: Arc<P>, tasks: Option<AnyTaskSpawner>) -> Self {
        // A wired spawner means the plugin may schedule background work,
        // possibly first from `process()` (a filter that only rebuilds on a
        // knob move, with nothing in `init` to warm the pool). Start the
        // pool here, on the instantiation (main) thread, so the first
        // audio-thread schedule never cold-starts worker threads inside the
        // callback - keeping the spawner's "safe from the audio thread"
        // guarantee true regardless of where the plugin first schedules.
        if tasks.is_some() {
            warm_pool();
        }
        // Build the snapshot slot before `init` so the plugin can capture
        // a publisher for the off-thread (large-state) lane. Pre-warm the
        // inline buffer to the plugin's hint so a first small publish
        // doesn't allocate on the audio thread.
        let snapshots = SnapshotSlot::with_capacity(L::snapshot_prealloc_hint());
        let init_ctx =
            InitContext::new(tasks.clone()).with_snapshot(SnapshotPublisher::new(&snapshots));
        let state = L::init(&params, &init_ctx);
        Self {
            params,
            state,
            meters: MeterStore::new(),
            snapshots,
            try_snapshot: true,
            last_snapshot_version: None,
            sample_rate: 44100.0,
            tasks,
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

    /// The plugin's background-task spawner (see
    /// `PluginExport::task_spawner`). `None` unless the plugin wired
    /// `tasks:` on `plugin!`.
    pub fn task_spawner(&self) -> Option<AnyTaskSpawner> {
        self.tasks.clone()
    }

    /// Access the plugin's DSP state (for testing).
    pub fn state_ref(&self) -> &L::DspState {
        &self.state
    }

    /// Mutable access to the plugin's DSP state (for testing).
    pub fn state_ref_mut(&mut self) -> &mut L::DspState {
        &mut self.state
    }
}

impl<P: Params + Default + 'static, L: PluginLogicCore<S, Params = P> + 'static, S: Sample>
    PluginRuntime for StaticShell<P, L, S>
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

    fn reset(&mut self, config: &AudioConfig) {
        self.sample_rate = config.sample_rate;
        // Params plumbing is the shell's job, not the plugin's: settle
        // smoother coefficients and state before the user's `reset` so
        // its body reads post-snap values.
        self.params.set_sample_rate(config.sample_rate);
        self.params.snap_smoothers();
        L::reset(&mut self.state, &self.params, config);
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
        let ctx = ProcessContext::new(
            context.transport,
            context.sample_rate,
            buffer.num_samples(),
            &mut *context.output_events,
        )
        .with_process_mode(context.process_mode)
        .with_params(&param_fn)
        .with_meters(&meter_fn);
        // Stamp the background-task spawner so `ctx.tasks::<T>()` works.
        let mut ctx = match &self.tasks {
            Some(t) => ctx.with_tasks(t),
            None => ctx,
        };

        let status = L::process(&mut self.state, &self.params, buffer, events, &mut ctx);
        publish_snapshot::<S, L>(
            &self.state,
            &self.snapshots,
            &mut self.try_snapshot,
            &mut self.last_snapshot_version,
        );
        status
    }

    fn save_state(&self) -> Vec<u8> {
        L::save_state(&self.state)
    }

    fn republish_snapshot(&mut self) {
        publish_snapshot::<S, L>(
            &self.state,
            &self.snapshots,
            &mut self.try_snapshot,
            &mut self.last_snapshot_version,
        );
    }

    fn load_state(&mut self, data: &[u8]) -> Result<(), StateLoadError> {
        let result = L::load_state(&mut self.state, data);
        // Plugin-side cache invalidation runs in the same `&mut`
        // borrow window so the next `process()` block sees the
        // refreshed caches - fire it whether or not load_state
        // succeeded so partial state still triggers a refresh.
        L::state_changed(&mut self.state, &self.params);
        // Invalidate the snapshot-version gate: the load replaced state
        // without necessarily bumping `snapshot_version` (a counter that
        // round-trips through the blob, or an author who forgets), so the
        // next publish - the `republish_snapshot` the wrapper calls right
        // after this - must re-serialize rather than skip on a stale
        // version and leave pre-load bytes in the slot.
        self.last_snapshot_version = None;
        result
    }

    fn migrate_state(foreign: &ForeignState) -> Option<MigratedState>
    where
        Self: Sized,
    {
        <L as PluginLogicCore<S>>::migrate_state(foreign)
    }

    fn latency(&self) -> u32 {
        L::latency(&self.state)
    }
    fn tail(&self) -> u32 {
        L::tail(&self.state)
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
pub(crate) fn publish_snapshot<S, L>(
    state: &L::DspState,
    slot: &SnapshotSlot,
    try_snapshot: &mut bool,
    last_version: &mut Option<u64>,
) where
    S: Sample,
    L: PluginLogicCore<S>,
{
    let version = L::snapshot_version(state);
    publish_snapshot_with(slot, try_snapshot, last_version, version, |buf| {
        L::snapshot_into(state, buf)
    });
}

/// Latch logic behind [`publish_snapshot`], parameterized over the raw
/// `snapshot_into` closure so it can be unit-tested without a full
/// `PluginLogicCore` mock. `pub(crate)` so `HotShell` can drive it with
/// a closure over the reloadable dylib's `truce_snapshot_into` symbol.
///
/// `version` is the plugin's [`PluginLogicCore::snapshot_version`] this
/// block. When it's `Some(v)` and equals `*last_version`, the state is
/// unchanged since the last landed publish, so the whole publish is
/// skipped - no lock, no copy, O(1). `None` re-serializes every block
/// (the historical behavior). `last_version` advances only on a landed
/// write, so a block skipped on reader contention retries next time.
pub(crate) fn publish_snapshot_with(
    slot: &SnapshotSlot,
    try_snapshot: &mut bool,
    last_version: &mut Option<u64>,
    version: Option<u64>,
    snapshot_into: impl FnOnce(&mut Vec<u8>) -> bool,
) {
    if !*try_snapshot {
        return;
    }
    // Version gate: a versioned plugin whose token is unchanged since the
    // last landed publish keeps the previous snapshot - the common path.
    if let Some(v) = version
        && *last_version == Some(v)
    {
        return;
    }
    let ran_unsupported = std::cell::Cell::new(false);
    let landed = slot.publish(|buf| {
        let wrote = snapshot_into(buf);
        ran_unsupported.set(!wrote);
        wrote
    });
    // Record the version only on a landed real write, so a block skipped
    // on reader contention retries next time instead of latching a
    // version whose bytes never reached the slot.
    if landed && !ran_unsupported.get() {
        *last_version = version;
    }
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
        $(tasks: [$($task:ty),+],)?
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

            fn reset(&mut self, config: &$crate::__macro_deps::truce_core::config::AudioConfig) {
                self.inner.reset(config);
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
                // Each `tasks: [..]` type gets its own lane (queue + mode).
                // The bundle collapses to `None` when no types were listed,
                // so a plugin with no tasks runs with no pool.
                #[allow(unused_mut)]
                let mut __task_bundle =
                    $crate::__macro_deps::truce_core::tasks::TaskSpawnerBundle::new();
                $(
                    $({
                        let __task_run = {
                            let params = std::sync::Arc::clone(&params);
                            move |task| {
                                <$task as $crate::__macro_deps::truce_plugin::BackgroundTask>::run(
                                    task, &params,
                                )
                            }
                        };
                        // `SERIALIZED` picks one-slot vs concurrent draining
                        // for this lane; the const folds the branch at
                        // compile time.
                        let __spawner = if <$task as $crate::__macro_deps::truce_plugin::BackgroundTask>::SERIALIZED {
                            $crate::__macro_deps::truce_core::tasks::TaskSpawner::<$task>::new_serialized(__task_run)
                        } else {
                            $crate::__macro_deps::truce_core::tasks::TaskSpawner::<$task>::new(__task_run)
                        };
                        __task_bundle.push(__spawner);
                    })+
                )?
                let tasks = __task_bundle.into_any();
                // The descriptor `$logic` is stateless; `from_parts`
                // builds the DSP state via `<$logic>::init(&params, &cx)`.
                Self {
                    inner: $crate::static_shell::StaticShell::from_parts(params, tasks),
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

            fn task_spawner(
                &self,
            ) -> ::core::option::Option<
                $crate::__macro_deps::truce_core::tasks::AnyTaskSpawner,
            > {
                self.inner.task_spawner()
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
    use std::cell::Cell;
    use truce_core::snapshot::SnapshotSlot;

    #[test]
    fn non_opt_in_latches_off_on_first_block() {
        let slot = SnapshotSlot::new();
        let mut try_snapshot = true;
        let mut last = None;

        // Default `snapshot_into` (returns false) before any publish:
        // latch off so we stop paying every block.
        publish_snapshot_with(&slot, &mut try_snapshot, &mut last, None, |_| false);
        assert!(!try_snapshot, "first false must latch off");
        assert!(!slot.is_supported());

        // Subsequent blocks short-circuit and never call the closure.
        let mut called = false;
        publish_snapshot_with(&slot, &mut try_snapshot, &mut last, None, |_| {
            called = true;
            false
        });
        assert!(!called, "latched-off slot must not call snapshot_into");
    }

    #[test]
    fn opt_in_then_contract_violation_stays_subscribed() {
        let slot = SnapshotSlot::new();
        let mut try_snapshot = true;
        let mut last = None;

        // Block 1: plugin publishes - it has opted in for its lifetime.
        publish_snapshot_with(&slot, &mut try_snapshot, &mut last, None, |buf| {
            buf.clear();
            buf.extend_from_slice(&[1, 2, 3]);
            true
        });
        assert!(try_snapshot);
        assert!(slot.is_supported());
        assert_eq!(slot.read(), Some(vec![1, 2, 3]));

        // Block 2: a contract-violating false must NOT latch us off - we
        // keep calling the plugin rather than silently going dark.
        publish_snapshot_with(&slot, &mut try_snapshot, &mut last, None, |_| false);
        assert!(try_snapshot, "a post-opt-in false must not latch off");

        // Block 3: still subscribed, so a fresh publish still lands.
        publish_snapshot_with(&slot, &mut try_snapshot, &mut last, None, |buf| {
            buf.clear();
            buf.extend_from_slice(&[4]);
            true
        });
        assert_eq!(slot.read(), Some(vec![4]));
    }

    #[test]
    fn unchanged_version_skips_the_copy() {
        let slot = SnapshotSlot::new();
        let mut try_snapshot = true;
        let mut last = None;
        let calls = Cell::new(0);
        let publish = |ver, try_s: &mut bool, last: &mut Option<u64>| {
            publish_snapshot_with(&slot, try_s, last, Some(ver), |buf| {
                calls.set(calls.get() + 1);
                buf.extend_from_slice(&[u8::try_from(ver).unwrap_or(0)]);
                true
            });
        };

        // Version 7 lands and is recorded.
        publish(7, &mut try_snapshot, &mut last);
        assert_eq!(calls.get(), 1);
        assert_eq!(last, Some(7));
        assert_eq!(slot.read(), Some(vec![7]));

        // Same version twice more: the writer never runs again.
        publish(7, &mut try_snapshot, &mut last);
        publish(7, &mut try_snapshot, &mut last);
        assert_eq!(calls.get(), 1, "unchanged version must skip the copy");

        // A new version re-serializes.
        publish(9, &mut try_snapshot, &mut last);
        assert_eq!(calls.get(), 2);
        assert_eq!(slot.read(), Some(vec![9]));
    }

    #[test]
    fn unversioned_none_publishes_every_block() {
        // The default (no `snapshot_version`) must keep re-serializing
        // every block - the historical behavior, unchanged.
        let slot = SnapshotSlot::new();
        let mut try_snapshot = true;
        let mut last = None;
        let calls = Cell::new(0);
        for _ in 0..3 {
            publish_snapshot_with(&slot, &mut try_snapshot, &mut last, None, |buf| {
                calls.set(calls.get() + 1);
                buf.push(1);
                true
            });
        }
        assert_eq!(calls.get(), 3, "None version re-serializes every block");
    }

    #[test]
    fn load_reset_forces_republish_at_unchanged_version() {
        // A load clears `last_snapshot_version` in both shells' `load_state`,
        // so the `republish_snapshot` the wrapper fires right after a load
        // re-serializes even though the plugin's version token didn't change
        // (a counter that round-trips through the blob, or an author who
        // forgot to bump). Without the reset the gate stays closed and the
        // slot keeps pre-load bytes - the host's next save reverts the load.
        let slot = SnapshotSlot::new();
        let mut try_snapshot = true;
        let mut last = None;
        let calls = Cell::new(0);
        let publish = |ver, try_s: &mut bool, last: &mut Option<u64>| {
            publish_snapshot_with(&slot, try_s, last, Some(ver), |buf| {
                calls.set(calls.get() + 1);
                buf.push(u8::try_from(ver).unwrap_or(0));
                true
            });
        };

        publish(5, &mut try_snapshot, &mut last);
        publish(5, &mut try_snapshot, &mut last);
        assert_eq!(calls.get(), 1, "unchanged version skips");

        // `load_state` clears the gate.
        last = None;
        publish(5, &mut try_snapshot, &mut last);
        assert_eq!(
            calls.get(),
            2,
            "a republish after a load must re-serialize even at the same version"
        );
    }
}
