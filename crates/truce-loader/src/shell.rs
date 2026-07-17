//! Shell-side integration: the hot-reloadable `HotShell<P, S>`.
//!
//! `HotShell<P, S = f32>` implements truce-core's `Plugin` +
//! `PluginExport` traits, delegating all logic to the flat Rust-ABI
//! symbols the hot-reloadable dylib exports (`export_plugin!`), which
//! it drives over an opaque `state: *mut ()` it owns across reloads.
//! The user's plugin impls one of the leaf traits (`PluginLogic` for
//! `f32` or `PluginLogic64` for `f64`); the blanket bridges in
//! `truce-plugin` lift that into the `PluginLogicCore<S>` the dylib's
//! exported functions call through.
//!
//! `HotShell` is parameterised over `S` so a `prelude64` plugin
//! and its `S = f64` logic dylib can hot-reload too. The chosen
//! precision is stamped into `AbiCanary::sample_precision` at
//! build time, so loading a mismatched dylib fails the canary
//! check before the vtable is touched.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use parking_lot::Mutex;
use truce_core::buffer::AudioBuffer;
use truce_core::bus::BusLayout;
use truce_core::config::{AudioConfig, ProcessMode};
use truce_core::events::{EventBody, EventList};
use truce_core::info::PluginInfo;
use truce_core::plugin::PluginRuntime;
use truce_core::process::{ProcessContext, ProcessStatus};
use truce_params::Params;
use truce_params::sample::Sample;

use crate::loader::{NativeLoader, StateOrigin};

/// How long a GUI / main-thread call into the loader (editor open,
/// state save / load) waits before giving up and returning the
/// "loader busy" fallback. Sized to span a typical audio block
/// (≪ 50 ms) without dragging through a full hot-reload window
/// (codesign + dlopen + canary verify can run 100s of ms on a 5–20
/// MB dylib). Matches the watcher's own `LOCK_WAIT` so the two
/// sides of the mutex have the same patience.
const GUI_LOCK_WAIT: Duration = Duration::from_millis(50);

// ---------------------------------------------------------------------------
// HotShell - the Plugin implementation that delegates to the dylib
// ---------------------------------------------------------------------------

/// A hot-reloadable plugin shell.
///
/// `P` is the parameter type (owned by the shell, survives reload).
/// `S` is the plugin's sample type (defaults to `f32` - the host wire
/// format). A `prelude64` plugin needs `S = f64`; the precision is
/// embedded in `AbiCanary::sample_precision`, so loading an `f64`
/// logic dylib into an `f32` shell (or vice versa) fails the canary
/// check at load time rather than silently binding to a vtable whose
/// `process()` slot expects a different `AudioBuffer<S>`.
///
/// All plugin logic (DSP, GUI rendering, layout) is delegated to the
/// flat exported functions in the loaded dylib, called over the
/// shell-owned opaque `state` pointer.
pub struct HotShell<P: Params, S: Sample = f32> {
    pub params: Arc<P>,
    loader: Arc<Mutex<NativeLoader<S>>>,
    /// The plugin's DSP state, owned by the shell as an erased
    /// `Box<State>` so it can outlive a hot-reload. Null before the
    /// first successful load. Only ever touched while the loader lock is
    /// held (audio thread) or under the wrapper's serialize (state
    /// save / load), never concurrently.
    state: *mut (),
    /// `drop` / `save` functions from the dylib that produced `state` -
    /// kept so the state is freed, and serialized for carry-over, by the
    /// exact code that made it, even after a reload swaps in a different
    /// dylib. `None` while `state` is null.
    state_origin: Option<StateOrigin>,
    /// Meter values written by DSP, read by GUI.
    meters: Arc<truce_core::meters::MeterStore>,
    /// Lock-free publish slot for `snapshot_into`-based state save.
    snapshots: Arc<truce_core::snapshot::SnapshotSlot>,
    /// Latches off if the loaded logic reports no snapshot before it ever
    /// publishes one; a logic that has published stays subscribed.
    try_snapshot: bool,
    /// Last `truce_snapshot_version` the shell published; a block whose
    /// version matches skips re-serialization (see `publish_snapshot_with`).
    last_snapshot_version: Option<u64>,
    sample_rate: f64,
    max_block_size: usize,
    /// Processing mode from the last `reset`. Replayed when the audio
    /// thread re-resets a freshly hot-swapped dylib so the new instance
    /// prepares for the same render mode.
    process_mode: ProcessMode,
    /// The `swap_generation` the live `self.state` allocation was born
    /// under. Every entry point that runs the current dylib's symbols
    /// against `self.state` compares this to `loader.swap_generation()`;
    /// a mismatch means the watcher swapped the dylib since the state was
    /// made, so the state must be carried over (or, under `&self`, saved
    /// through its origin dylib) before the new code touches it. Advances
    /// on successful swaps only, so a failed reload never triggers a
    /// spurious carry-over.
    state_generation: u64,
    /// Set when `reset` couldn't take the loader lock (watcher mid-reload)
    /// and returned without re-preparing the dylib's state. `process`
    /// replays the reset once it holds the lock so a sample-rate / block
    /// -size change is never dropped.
    reset_pending: bool,
    /// Atomic snapshots of the plugin's most recent `latency()` /
    /// `tail()`. Updated by the audio thread on each `process()` so
    /// the host's main-thread queries don't block on the loader mutex.
    latency_cache: AtomicU32,
    tail_cache: AtomicU32,
}

// SAFETY: the load-bearing field is `state: *mut ()` - a raw pointer,
// so the compiler won't infer `Send` and this manual impl is required.
// It is an erased `Box<L::DspState>`, and `PluginLogicCore::DspState:
// Send`, so the pointee is `Send` and safe to move to another thread; the
// shell exclusively owns that allocation (it alone frees it, through
// the origin dylib's `state_origin.drop`). The pointer is only
// dereferenced while the shell holds `&mut self` / `&self`, and the
// format wrapper serializes `process` / `reset` / `save_state` /
// `load_state` behind its plugin lock, so no two threads ever touch
// `state` at once. `state_origin` holds bare `fn` pointers (`Send`).
// The remaining fields are `Send` on their own: `Arc<P>` (`P: Sync` by
// the `Params` contract), `Arc<Mutex<NativeLoader<S>>>` (`Send + Sync`),
// the atomics, and the atomic-slot `MeterStore`.
unsafe impl<P: Params, S: Sample> Send for HotShell<P, S> {}

impl<P: Params + 'static, S: Sample> HotShell<P, S> {
    pub fn new(params: P, dylib_path: PathBuf) -> Self {
        let params = Arc::new(params);
        let params_ptr = Arc::as_ptr(&params).cast::<()>();
        let loader = NativeLoader::new(dylib_path, params_ptr);
        let initial_swap = loader.swap_generation();
        // Allocate the initial DSP state from the freshly loaded dylib
        // (before wrapping the loader in the mutex - no contention yet).
        let (state, state_origin) = loader
            .init_state()
            .map_or((std::ptr::null_mut(), None), |(st, origin)| {
                (st, Some(origin))
            });
        let loader = Arc::new(Mutex::new(loader));
        // Drive reloads off the audio thread. The watcher polls the
        // dylib path and runs `reload()` itself when mtime advances.
        NativeLoader::spawn_watcher(&loader);
        Self {
            params,
            loader,
            state,
            state_origin,
            meters: truce_core::meters::MeterStore::new(),
            snapshots: truce_core::snapshot::SnapshotSlot::new(),
            try_snapshot: true,
            last_snapshot_version: None,
            sample_rate: 44100.0,
            max_block_size: 1024,
            process_mode: ProcessMode::Realtime,
            state_generation: initial_swap,
            reset_pending: false,
            latency_cache: AtomicU32::new(0),
            tail_cache: AtomicU32::new(0),
        }
    }

    /// Ensure `self.state` is a live allocation from the current dylib,
    /// allocating it if the shell came up before any dylib was loaded.
    /// Returns `false` if nothing is loaded (nothing to run).
    fn ensure_state(&mut self, loader: &NativeLoader<S>) -> bool {
        if !self.state.is_null() {
            return true;
        }
        if let Some((st, origin)) = loader.init_state() {
            self.state = st;
            self.state_origin = Some(origin);
            self.state_generation = loader.swap_generation();
            true
        } else {
            false
        }
    }

    /// Free the current state through the dylib that made it, if any.
    fn drop_state(&mut self) {
        if let (false, Some(origin)) = (self.state.is_null(), self.state_origin.take()) {
            (origin.drop)(self.state);
        }
        self.state = std::ptr::null_mut();
    }

    /// Bring `self.state` into agreement with the currently loaded dylib,
    /// then ensure it is allocated. Must hold the loader lock.
    ///
    /// The watcher can swap the dylib between audio blocks, so any entry
    /// point that runs the current dylib's symbols against `self.state`
    /// must reconcile first - otherwise it runs new code over an
    /// allocation the previous dylib made, which is UB when the `State`
    /// layout changed. On a generation mismatch the live state is
    /// serialized through its origin dylib and restored into
    /// freshly-init'd state under the new code (a `None` / empty /
    /// rejected blob just starts fresh), then reset to the current config.
    /// A no-op when no swap happened since `self.state` was born.
    ///
    /// Returns whether live state is available (false only when nothing
    /// is loaded).
    fn reconcile_state(&mut self, loader: &NativeLoader<S>) -> bool {
        let swap = loader.swap_generation();
        if swap != self.state_generation {
            let carried = if loader.preserve_dsp_state() && !self.state.is_null() {
                self.state_origin.map(|o| (o.save)(self.state.cast_const()))
            } else {
                None
            };
            self.drop_state();
            if self.ensure_state(loader) {
                if let Some(bytes) = carried.filter(|b| !b.is_empty())
                    && let Err(e) = loader.load_state(self.state, &bytes)
                {
                    log::warn!(
                        "truce-hot: reloaded dylib rejected carried state, \
                         starting fresh: {e}"
                    );
                }
                let config = AudioConfig::new(self.sample_rate, self.max_block_size)
                    .with_process_mode(self.process_mode);
                loader.reset(self.state, &config);
                // The fresh state is prepared for the current config, so a
                // reset missed under lock contention is satisfied.
                self.reset_pending = false;
            }
            // Mark the swap observed even if nothing is loaded, so a
            // dead reload doesn't re-trigger every call.
            self.state_generation = swap;
        }
        self.ensure_state(loader)
    }

    /// Shared meter storage handle - the GUI-thread-safe channel
    /// for meter reads (see `PluginExport::meter_store`).
    #[must_use]
    pub fn meter_store(&self) -> Arc<truce_core::meters::MeterStore> {
        Arc::clone(&self.meters)
    }

    /// Shared snapshot slot for lock-free state save (see
    /// `PluginExport::snapshot_slot`).
    #[must_use]
    pub fn snapshot_slot(&self) -> Arc<truce_core::snapshot::SnapshotSlot> {
        Arc::clone(&self.snapshots)
    }

    /// A lock-free editor builder that constructs from the *currently
    /// loaded* dylib (via its `truce_build_editor` symbol), so GUI edits
    /// hot-reload - the host picks up the new editor on the next close+
    /// open. The closure takes the shared params `Arc`, `try_lock_for`s
    /// the loader (the audio thread only `try_lock`s it, so this never
    /// stalls audio), and returns `None` during an in-flight reload -
    /// the host retries editor creation on a later idle tick.
    #[must_use]
    pub fn editor_builder(&self) -> truce_core::editor::EditorBuilder<P> {
        let loader = Arc::clone(&self.loader);
        Box::new(move |params: Arc<P>| {
            let params_ptr = Arc::as_ptr(&params).cast::<()>();
            let guard = loader.try_lock_for(GUI_LOCK_WAIT)?;
            guard.build_editor(params_ptr)
        })
    }
}

impl<P: Params + 'static, S: Sample> PluginRuntime for HotShell<P, S> {
    type Sample = S;

    fn info() -> PluginInfo
    where
        Self: Sized,
    {
        unreachable!("HotShell::info() should not be called statically")
    }

    fn bus_layouts() -> Vec<BusLayout>
    where
        Self: Sized,
    {
        unreachable!("HotShell::bus_layouts() should not be called statically")
    }

    fn init(&mut self) {}

    fn reset(&mut self, config: &AudioConfig) {
        self.sample_rate = config.sample_rate;
        self.max_block_size = config.max_block_size;
        self.process_mode = config.process_mode;
        // Params plumbing is the shell's job, not the plugin's: settle
        // smoother coefficients and state before the dylib's `reset` so
        // its body reads post-snap values. Runs even when the loader
        // lock below is contended - params live host-side.
        self.params.set_sample_rate(config.sample_rate);
        self.params.snap_smoothers();

        // CLAP / VST3 may call `reset` on the audio thread; same
        // priority-inversion concern as `process`. If anything holds the
        // loader (watcher reload, GUI editor build, state save / load),
        // flag the reset and bail rather than block: a blocked audio
        // thread is not recoverable, a deferred reset is. `process`
        // replays the flag so a sample-rate / block-size change is never
        // dropped, leaving coefficients tuned for the old rate.
        // Lock through a cloned handle (one relaxed atomic inc, RT-safe)
        // so the guard borrows the local `Arc`, not `self` - leaving the
        // shell's state fields free to mutate under the lock.
        let loader_arc = Arc::clone(&self.loader);
        let Some(loader) = loader_arc.try_lock() else {
            self.reset_pending = true;
            return;
        };
        // Reconcile first: a watcher swap since the last block can leave
        // `self.state` a previous dylib's allocation, which `loader.reset`
        // would run new code over.
        if self.reconcile_state(&loader) {
            loader.reset(self.state, config);
            self.latency_cache
                .store(loader.latency(self.state), Ordering::Relaxed);
            self.tail_cache
                .store(loader.tail(self.state), Ordering::Relaxed);
            self.reset_pending = false;
        }
    }

    fn process(
        &mut self,
        buffer: &mut AudioBuffer<S>,
        events: &EventList,
        context: &mut ProcessContext,
    ) -> ProcessStatus {
        // Lock-free on the audio thread: if the watcher thread holds
        // the loader (reload in flight - codesign + dlopen + canary
        // probe takes 100s of ms on a 5–20 MB dylib), skip this block
        // rather than block. A skipped block is silent for one buffer
        // (`Normal` returns the host's already-zeroed output) - better
        // than parking the audio thread under priority inversion. The
        // watcher takes the lock briefly per reload (mtime poll loop's
        // `try_lock_for(50ms)`), so contention is bounded to the
        // reload window itself.
        // Lock through a cloned handle (RT-safe atomic inc) so the guard
        // borrows the local, not `self` - the state fields stay mutable.
        let loader_arc = Arc::clone(&self.loader);
        let Some(loader) = loader_arc.try_lock() else {
            return ProcessStatus::Normal;
        };

        // The watcher thread drives reload directly; the audio thread
        // observes the swap here and carries the live DSP state into the
        // new code (serialize through the origin dylib, re-init under the
        // new one), so the old allocation's bytes are never reinterpreted
        // under a changed `State` layout.
        if !self.reconcile_state(&loader) {
            return ProcessStatus::Normal;
        }

        // A `reset` that lost the loader lock left `reset_pending` set.
        // `reconcile_state` above clears it when a swap reset the fresh
        // state; if no swap was observed (lock held by a GUI editor build
        // or state save / load), replay it here so the sample-rate /
        // block-size change still reaches the dylib's coefficients.
        if self.reset_pending {
            let config = AudioConfig::new(self.sample_rate, self.max_block_size)
                .with_process_mode(self.process_mode);
            loader.reset(self.state, &config);
            self.reset_pending = false;
        }

        // Apply parameter change events to our atomic params.
        // ParamChange values from format wrappers are PLAIN (already
        // denormalized). No smoother snap here: events set targets and
        // the smoothers ramp toward them; snapping belongs to `reset`
        // and state loads only.
        for e in events.iter() {
            if let EventBody::ParamChange { id, value } = &e.body {
                self.params.set_plain(*id, *value);
            }
        }

        // No sync needed - the dylib reads from the same Arc<Params>
        // via the shell's params pointer.

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
        .with_process_mode(context.process_mode)
        .with_params(&param_fn)
        .with_meters(&meter_fn);

        let status = loader.process(self.state, buffer, events, &mut ctx);

        let state = self.state.cast_const();
        let version = loader.snapshot_version(state);
        crate::static_shell::publish_snapshot_with(
            &self.snapshots,
            &mut self.try_snapshot,
            &mut self.last_snapshot_version,
            version,
            |buf| loader.snapshot_into(state, buf),
        );

        // Refresh latency / tail caches so host-thread queries don't
        // have to take the loader lock.
        self.latency_cache
            .store(loader.latency(state), Ordering::Relaxed);
        self.tail_cache.store(loader.tail(state), Ordering::Relaxed);

        status
    }

    fn save_state(&self) -> Vec<u8> {
        // Hosts call this on the main / UI thread (e.g. project save,
        // preset capture). Bounded `try_lock_for` keeps a concurrent
        // hot-reload from hanging the host for the full reload window;
        // on miss the host receives an empty blob - same observable
        // shape as a plugin that has no extra state. Matches the host
        // contract better than a UI hang.
        // The lock still serializes against the audio thread's `process`
        // (both take the loader mutex), but the save runs through the
        // dylib that MADE `self.state` - its `state_origin`, not the
        // currently loaded one. A watcher swap between blocks can leave
        // `self.state` an older dylib's allocation, and the new dylib's
        // `save_state` over it would read the old bytes under the new
        // layout (UB). The origin fn is always correct for `self.state`,
        // and in the common no-swap case it *is* the current dylib's.
        let Some(_loader) = self.loader.try_lock_for(GUI_LOCK_WAIT) else {
            return Vec::new();
        };
        match self.state_origin {
            Some(origin) if !self.state.is_null() => (origin.save)(self.state.cast_const()),
            _ => Vec::new(),
        }
    }

    fn snapshot_into(&self, buf: &mut Vec<u8>) -> bool {
        // Same bounded-lock trade-off as `save_state`: on a hot-reload
        // miss return "no snapshot" rather than hang the host.
        let Some(loader) = self.loader.try_lock_for(GUI_LOCK_WAIT) else {
            return false;
        };
        // A watcher swap can leave `self.state` a previous dylib's
        // allocation; the current dylib's `snapshot_into` over it would
        // read old bytes under the new layout. Until `process` / `reset`
        // reconciles, report "no snapshot" - the host falls back to
        // `save_state`, which serializes through the state's origin dylib.
        if self.state.is_null() || loader.swap_generation() != self.state_generation {
            return false;
        }
        loader.snapshot_into(self.state.cast_const(), buf)
    }

    fn republish_snapshot(&mut self) {
        let loader_arc = Arc::clone(&self.loader);
        let Some(loader) = loader_arc.try_lock_for(GUI_LOCK_WAIT) else {
            return;
        };
        // Reconcile before snapshotting: the current dylib's symbols must
        // not run over a previous dylib's allocation.
        if !self.reconcile_state(&loader) {
            return;
        }
        let state = self.state.cast_const();
        let version = loader.snapshot_version(state);
        crate::static_shell::publish_snapshot_with(
            &self.snapshots,
            &mut self.try_snapshot,
            &mut self.last_snapshot_version,
            version,
            |buf| loader.snapshot_into(state, buf),
        );
    }

    fn load_state(&mut self, data: &[u8]) -> Result<(), truce_core::state::StateLoadError> {
        // Same trade-off as `save_state`: bounded wait keeps the UI
        // thread from blocking through a reload. On timeout we report
        // success-with-no-op so the host doesn't surface a load
        // failure for what is effectively a reload race. If the host
        // load was carrying real preset bytes, the watcher's reload
        // will pull them back from the next user-driven preset
        // refresh; the alternative (UI hang) is worse.
        let loader_arc = Arc::clone(&self.loader);
        let Some(loader) = loader_arc.try_lock_for(GUI_LOCK_WAIT) else {
            return Ok(());
        };
        // Reconcile before restoring: `self.state` must be an allocation
        // of the current dylib before its `load_state` writes into it.
        if !self.reconcile_state(&loader) {
            return Ok(());
        }
        // The loader restores into `state` and fires `state_changed` in
        // the same window (so the next `process` sees refreshed caches),
        // matching the static shell's policy.
        let result = loader.load_state(self.state, data);
        drop(loader);
        // Invalidate the snapshot-version gate so the `republish_snapshot`
        // the wrapper calls right after a load re-serializes, rather than
        // skipping on a stale version and leaving pre-load bytes in the
        // slot (see `StaticShell::load_state`).
        self.last_snapshot_version = None;
        result
    }

    fn migrate_state(
        _foreign: &truce_core::state::ForeignState,
    ) -> Option<truce_core::state::MigratedState>
    where
        Self: Sized,
    {
        // Receiverless: the logic type lives behind the loader's
        // `Box<dyn PluginLogicCore>` per instance, which an
        // associated function can't reach. Shell mode is a dev
        // configuration; legacy-state migration only runs in static
        // builds (the shape every shipped plugin uses).
        log::warn!(
            "truce-hot: host offered foreign state but --shell builds don't \
             route migrate_state; load will be reported as failed"
        );
        None
    }

    fn latency(&self) -> u32 {
        // Read the audio-thread-updated atomic snapshot rather than
        // dispatching through `&PluginLogic` (which would race with
        // the audio thread's `&mut PluginLogic` and require the
        // loader lock).
        self.latency_cache.load(Ordering::Relaxed)
    }

    fn tail(&self) -> u32 {
        self.tail_cache.load(Ordering::Relaxed)
    }

    fn get_meter(&self, meter_id: u32) -> f32 {
        self.meters.read(meter_id)
    }
}

impl<P: Params, S: Sample> Drop for HotShell<P, S> {
    fn drop(&mut self) {
        // Free the DSP state through the dylib that produced it (its
        // `Drop` glue lives there). The library is leaked, never closed,
        // so the drop function is still mapped. The loader's own `Drop`
        // then tears down the symbol table + leaked handles.
        self.drop_state();
    }
}

// Hot-reload is single-crate via `--features shell`, generated by
// `truce::plugin!` in `truce/src/plugin_macro.rs`. `HotShell<P>` is
// public-but-unadvertised because `__plugin_hot_reload!` wraps it via
// `truce::__reexport::HotShell`. The shell now hands the format
// wrapper whatever `PluginLogic::editor()` returns - no wrapper /
// watcher / hot-swap is mediated by truce-loader. Editor-side
// reload (swap-on-dylib-change while the window is open) is no
// longer supported; reopening the editor picks up the new build.
