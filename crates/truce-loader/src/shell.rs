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

use crate::loader::NativeLoader;

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
    /// Fingerprint of the dylib that produced `state`. Compared against
    /// a reloaded dylib's fingerprint to decide preservation.
    state_fingerprint: u64,
    /// `drop` function from the dylib that produced `state` - kept so
    /// the state is freed by the exact code that made it, even after a
    /// reload swaps in a different dylib.
    state_dropper: Option<fn(*mut ())>,
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
    /// Last `load_counter` value the audio path observed. When the
    /// file watcher drives a reload, this lags behind
    /// `loader.load_counter()` until `process()` runs `plugin.reset()`
    /// to match the new instance.
    last_seen_load_counter: u64,
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
// the origin dylib's `state_dropper`). The pointer is only
// dereferenced while the shell holds `&mut self` / `&self`, and the
// format wrapper serializes `process` / `reset` / `save_state` /
// `load_state` behind its plugin lock, so no two threads ever touch
// `state` at once. `state_dropper` is a bare `fn` pointer (`Send`).
// The remaining fields are `Send` on their own: `Arc<P>` (`P: Sync` by
// the `Params` contract), `Arc<Mutex<NativeLoader<S>>>` (`Send + Sync`),
// the atomics, and the atomic-slot `MeterStore`.
unsafe impl<P: Params, S: Sample> Send for HotShell<P, S> {}

impl<P: Params + 'static, S: Sample> HotShell<P, S> {
    pub fn new(params: P, dylib_path: PathBuf) -> Self {
        let params = Arc::new(params);
        let params_ptr = Arc::as_ptr(&params).cast::<()>();
        let loader = NativeLoader::new(dylib_path, params_ptr);
        let initial_counter = loader.load_counter();
        // Allocate the initial DSP state from the freshly loaded dylib
        // (before wrapping the loader in the mutex - no contention yet).
        let (state, state_fingerprint, state_dropper) = loader
            .init_state()
            .map_or((std::ptr::null_mut(), 0, None), |(st, fp, d)| {
                (st, fp, Some(d))
            });
        let loader = Arc::new(Mutex::new(loader));
        // Drive reloads off the audio thread. The watcher polls the
        // dylib path and runs `reload()` itself when mtime advances.
        NativeLoader::spawn_watcher(&loader);
        Self {
            params,
            loader,
            state,
            state_fingerprint,
            state_dropper,
            meters: truce_core::meters::MeterStore::new(),
            snapshots: truce_core::snapshot::SnapshotSlot::new(),
            try_snapshot: true,
            last_snapshot_version: None,
            sample_rate: 44100.0,
            max_block_size: 1024,
            process_mode: ProcessMode::Realtime,
            last_seen_load_counter: initial_counter,
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
        if let Some((st, fp, dropper)) = loader.init_state() {
            self.state = st;
            self.state_fingerprint = fp;
            self.state_dropper = Some(dropper);
            true
        } else {
            false
        }
    }

    /// Free the current state through the dylib that made it, if any.
    fn drop_state(&mut self) {
        if let (false, Some(dropper)) = (self.state.is_null(), self.state_dropper.take()) {
            dropper(self.state);
        }
        self.state = std::ptr::null_mut();
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
        // priority-inversion concern as `process`. The watcher's hold
        // window is bounded; if we miss this reset, the next
        // `process` call will still pick up the new sample rate via
        // the `last_seen_load_counter` path. So a missed reset here
        // is recoverable, while a blocked audio thread is not.
        // Lock through a cloned handle (one relaxed atomic inc, RT-safe)
        // so the guard borrows the local `Arc`, not `self` - leaving the
        // shell's state fields free to mutate under the lock.
        let loader_arc = Arc::clone(&self.loader);
        let Some(loader) = loader_arc.try_lock() else {
            return;
        };
        if self.ensure_state(&loader) {
            loader.reset(self.state, config);
            self.latency_cache
                .store(loader.latency(self.state), Ordering::Relaxed);
            self.tail_cache
                .store(loader.tail(self.state), Ordering::Relaxed);
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
        // observes the swap here and decides what happens to the live
        // DSP state.
        let counter = loader.load_counter();
        if counter != self.last_seen_load_counter {
            let config = AudioConfig::new(self.sample_rate, self.max_block_size)
                .with_process_mode(self.process_mode);
            match loader.state_fingerprint() {
                // Same `State` layout as our live state: the author
                // changed only code. Keep the allocation and run the new
                // `process` on it - the reverb tail keeps ringing, no
                // reset. This is the hot-reload payoff. `may_preserve`
                // rejects `NO_PRESERVE` (a stateless `()` state, or an
                // explicit opt-out) even against itself, so those re-init.
                Some(fp)
                    if !self.state.is_null()
                        && truce_core::dsp_state::may_preserve(fp, self.state_fingerprint) => {}
                // Different (or first) layout: the old bytes aren't a
                // valid new `State`. Drop them through the origin dylib,
                // allocate fresh from the new one, and reset.
                Some(_) => {
                    self.drop_state();
                    if self.ensure_state(&loader) {
                        loader.reset(self.state, &config);
                    }
                }
                // Nothing loaded now (failed reload): keep our state.
                None => {}
            }
            self.last_seen_load_counter = counter;
        }

        if !self.ensure_state(&loader) {
            return ProcessStatus::Normal;
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
        let Some(loader) = self.loader.try_lock_for(GUI_LOCK_WAIT) else {
            return Vec::new();
        };
        if self.state.is_null() {
            return Vec::new();
        }
        loader.save_state(self.state.cast_const())
    }

    fn snapshot_into(&self, buf: &mut Vec<u8>) -> bool {
        // Same bounded-lock trade-off as `save_state`: on a hot-reload
        // miss return "no snapshot" rather than hang the host.
        let Some(loader) = self.loader.try_lock_for(GUI_LOCK_WAIT) else {
            return false;
        };
        if self.state.is_null() {
            return false;
        }
        loader.snapshot_into(self.state.cast_const(), buf)
    }

    fn republish_snapshot(&mut self) {
        let Some(loader) = self.loader.try_lock_for(GUI_LOCK_WAIT) else {
            return;
        };
        if self.state.is_null() {
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
        let Some(loader) = self.loader.try_lock_for(GUI_LOCK_WAIT) else {
            return Ok(());
        };
        if self.state.is_null() {
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
