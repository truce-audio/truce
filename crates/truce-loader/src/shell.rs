//! Shell-side integration: `HotShell<P>` and `HotEditor`.
//!
//! `HotShell` implements truce-core's `Plugin` + `PluginExport` traits,
//! delegating all logic to the `PluginLogic` trait object in the
//! hot-reloadable dylib.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use parking_lot::Mutex;
use truce_core::buffer::AudioBuffer;
use truce_core::bus::BusLayout;
use truce_core::editor::{Editor, PluginContext, RawWindowHandle};
use truce_core::events::{EventBody, EventList};
use truce_core::info::PluginInfo;
use truce_core::plugin::Plugin;
use truce_core::process::{ProcessContext, ProcessStatus};
use truce_params::Params;

use crate::loader::NativeLoader;

macro_rules! hot_debug {
    ($($arg:tt)*) => {
        #[cfg(feature = "hot-debug")]
        eprintln!($($arg)*);
    };
}

// ---------------------------------------------------------------------------
// HotShell — the Plugin implementation that delegates to the dylib
// ---------------------------------------------------------------------------

/// A hot-reloadable plugin shell.
///
/// `P` is the parameter type (owned by the shell, survives reload).
/// All plugin logic (DSP, GUI rendering, layout) is delegated to
/// the `PluginLogic` trait object in the loaded dylib.
pub struct HotShell<P: Params> {
    pub params: Arc<P>,
    loader: Arc<Mutex<NativeLoader>>,
    /// Meter values written by DSP, read by GUI.
    meters: Arc<[AtomicU32; 256]>,
    sample_rate: f64,
    max_block_size: usize,
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

unsafe impl<P: Params> Send for HotShell<P> {}

impl<P: Params + 'static> HotShell<P> {
    pub fn new(params: P, dylib_path: PathBuf) -> Self {
        let params = Arc::new(params);
        let params_ptr = Arc::as_ptr(&params).cast::<()>();
        let loader = NativeLoader::new(dylib_path, params_ptr);
        let initial_counter = loader.load_counter();
        let loader = Arc::new(Mutex::new(loader));
        // Drive reloads off the audio thread. The watcher polls the
        // dylib path and runs `reload()` itself when mtime advances.
        NativeLoader::spawn_watcher(&loader);
        Self {
            params,
            loader,
            meters: Arc::new(std::array::from_fn(|_| AtomicU32::new(0))),
            sample_rate: 44100.0,
            max_block_size: 1024,
            last_seen_load_counter: initial_counter,
            latency_cache: AtomicU32::new(0),
            tail_cache: AtomicU32::new(0),
        }
    }

    /// Try to get a custom editor from the loaded plugin.
    #[must_use]
    pub fn try_custom_editor(&self) -> Option<Box<dyn Editor>> {
        let loader = self.loader.lock();
        let plugin = loader.plugin()?;
        plugin.custom_editor()
    }

    /// Try to create a `BuiltinEditor` from the loaded plugin's layout.
    /// Returns `None` if no plugin is loaded or the layout has zero size.
    #[must_use]
    pub fn try_builtin_editor(&self) -> Option<truce_gui::editor::BuiltinEditor<P>> {
        let loader = self.loader.lock();
        let plugin = loader.plugin()?;
        let layout = plugin.layout();
        if layout.width == 0 || layout.height == 0 {
            return None;
        }
        drop(loader);
        Some(truce_gui::editor::BuiltinEditor::new_grid(
            Arc::clone(&self.params),
            layout,
        ))
    }
}

impl<P: Params + 'static> Plugin for HotShell<P> {
    // Hot-reload mode is `f32`-only for now: the dylib boundary
    // (`Box<dyn PluginLogic>`) takes the trait's default `S = f32`,
    // so a `prelude64` plugin built as a logic dylib won't satisfy
    // the loader's expected vtable. Static-mode shells (the common
    // case for release builds) support `f64` plugins end-to-end.
    type Sample = f32;

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

    fn reset(&mut self, sample_rate: f64, max_block_size: usize) {
        self.sample_rate = sample_rate;
        self.max_block_size = max_block_size;
        self.params.set_sample_rate(sample_rate);

        // CLAP / VST3 may call `reset` on the audio thread; same
        // priority-inversion concern as `process`. The watcher's hold
        // window is bounded; if we miss this reset, the next
        // `process` call will still pick up the new sample rate via
        // the `last_seen_load_counter` path. So a missed reset here
        // is recoverable, while a blocked audio thread is not.
        let Some(mut loader) = self.loader.try_lock() else {
            return;
        };
        if let Some(plugin) = loader.plugin_mut() {
            plugin.reset(sample_rate, max_block_size);
            self.latency_cache
                .store(plugin.latency(), Ordering::Relaxed);
            self.tail_cache.store(plugin.tail(), Ordering::Relaxed);
        }
    }

    fn process(
        &mut self,
        buffer: &mut AudioBuffer,
        events: &EventList,
        context: &mut ProcessContext,
    ) -> ProcessStatus {
        // Lock-free on the audio thread: if the watcher thread holds
        // the loader (reload in flight — codesign + dlopen + canary
        // probe takes 100s of ms on a 5–20 MB dylib), skip this block
        // rather than block. A skipped block is silent for one buffer
        // (`Normal` returns the host's already-zeroed output) — better
        // than parking the audio thread under priority inversion. The
        // watcher takes the lock briefly per reload (mtime poll loop's
        // `try_lock_for(50ms)`), so contention is bounded to the
        // reload window itself.
        let Some(mut loader) = self.loader.try_lock() else {
            return ProcessStatus::Normal;
        };

        // The watcher thread drives reload directly; the audio thread
        // only observes the swap and resets the new plugin to the
        // current sample rate / block size.
        let counter = loader.load_counter();
        if counter != self.last_seen_load_counter {
            if let Some(plugin) = loader.plugin_mut() {
                plugin.reset(self.sample_rate, self.max_block_size);
            }
            self.last_seen_load_counter = counter;
        }

        let Some(plugin) = loader.plugin_mut() else {
            return ProcessStatus::Normal;
        };

        // Apply parameter change events to our atomic params.
        // ParamChange values from format wrappers are PLAIN (already denormalized).
        for e in events.iter() {
            if let EventBody::ParamChange { id, value } = &e.body {
                self.params.set_plain(*id, *value);
            }
        }
        self.params.snap_smoothers();

        // No sync needed — plugin reads from the same Arc<Params>.

        // Build a ProcessContext with param/meter callbacks for the logic.
        let params = &self.params;
        let meters = &self.meters;
        let param_fn = |id: u32| -> f64 { params.get_plain(id).unwrap_or(0.0) };
        let meter_fn = |id: u32, v: f32| {
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

        let status = plugin.process(buffer, events, &mut ctx);

        // Refresh latency / tail caches so host-thread queries don't
        // have to take the loader lock (and don't dispatch through
        // `&PluginLogic` while audio holds `&mut PluginLogic`).
        self.latency_cache
            .store(plugin.latency(), Ordering::Relaxed);
        self.tail_cache.store(plugin.tail(), Ordering::Relaxed);

        status
    }

    fn save_state(&self) -> Vec<u8> {
        let loader = self.loader.lock();
        loader
            .plugin()
            .map(truce_gui::PluginLogic::save_state)
            .unwrap_or_default()
    }

    fn load_state(&mut self, data: &[u8]) -> Result<(), truce_core::state::StateLoadError> {
        let mut loader = self.loader.lock();
        let Some(plugin) = loader.plugin_mut() else {
            return Ok(());
        };
        let result = plugin.load_state(data);
        // Plugin-side cache invalidation runs in the same `&mut`
        // borrow window so the next `process()` block sees the
        // refreshed caches — fire even on partial state.
        plugin.state_changed();
        result
    }

    fn editor(&mut self) -> Option<Box<dyn Editor>> {
        hot_debug!("[truce-hot] editor() called");

        // Custom editor path (egui, iced)
        if let Some(custom) = self.try_custom_editor() {
            hot_debug!("[truce-hot] using custom editor");
            return Some(Box::new(HotEditor::<P>::new_custom(custom)));
        }

        // Built-in editor path (layout + GPU). Shares `self.loader`
        // with the audio path so the GUI and audio thread always render
        // the same dylib version — a separate NativeLoader for the GUI
        // could otherwise pick up a newer build than the audio thread is
        // still processing through. The watcher uses `try_lock` so the
        // audio thread keeps priority on the mutex.
        let builtin = self.try_builtin_editor()?;
        hot_debug!("[truce-hot] using builtin editor (GPU path)");
        let inner = Arc::new(StdMutex::new(builtin));
        let gpu = truce_gpu::GpuEditor::new_shared(Arc::clone(&inner));
        Some(Box::new(HotEditor::new_builtin(
            gpu,
            &inner,
            &self.loader,
            &self.params,
        )))
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
        let idx = meter_id.wrapping_sub(truce_params::METER_ID_BASE) as usize;
        self.meters
            .get(idx)
            .map_or(0.0, |v| f32::from_bits(v.load(Ordering::Relaxed)))
    }
}

// ---------------------------------------------------------------------------
// HotEditor — wraps editors for GUI hot-reload
// ---------------------------------------------------------------------------

enum HotEditorInner<P: Params> {
    /// Built-in GUI: swap `BuiltinEditor` inside shared mutex on reload.
    /// GPU rendering continues seamlessly. The shared mutex is owned
    /// by `gpu` and the watcher thread; `HotEditor` itself doesn't
    /// need a third clone.
    Builtin { gpu: truce_gpu::GpuEditor<P> },
    /// Custom GUI (egui, iced): close/reopen on reload.
    Custom { editor: Box<dyn Editor> },
}

struct HotEditor<P: Params> {
    kind: HotEditorInner<P>,
    /// Background thread handle for the GUI reload watcher.
    _watcher: Option<std::thread::JoinHandle<()>>,
    /// Set to true when the editor is dropped so the watcher thread exits.
    stop: Arc<AtomicBool>,
}

unsafe impl<P: Params> Send for HotEditor<P> {}

impl<P: Params> Drop for HotEditor<P> {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        hot_debug!("[truce-gui-reload] stop flag set (editor dropped)");
    }
}

impl<P: Params + 'static> HotEditor<P> {
    fn new_builtin(
        gpu: truce_gpu::GpuEditor<P>,
        inner: &Arc<StdMutex<truce_gui::editor::BuiltinEditor<P>>>,
        loader: &Arc<Mutex<NativeLoader>>,
        params: &Arc<P>,
    ) -> Self {
        let stop = Arc::new(AtomicBool::new(false));

        // Spawn a background thread that watches for dylib changes
        // and swaps the `BuiltinEditor` inside the shared mutex.
        //
        // The watcher shares the audio-path `NativeLoader` so the
        // version the GUI renders is always the version the audio
        // path is running.
        //
        // Lock contention with the audio thread is handled with
        // `try_lock_for(50ms)` rather than `try_lock`. The audio
        // thread holds the loader lock for the entire `process()`
        // call (a few ms per buffer); a bare `try_lock` against a
        // process-rate of ~344 Hz routinely misses, and under
        // sustained audio load (large blocks, heavy DSP) the bare
        // `try_lock` can starve indefinitely. 50 ms is large enough
        // to wait through several audio buffers but small enough that
        // the watcher tick still feels live.
        //
        // The 500 ms poll cadence is chunked into 50 ms stop-flag
        // checks so that dropping the editor (Drop calls
        // `stop_flag.store(true)`) wakes the watcher within ~50 ms
        // instead of having to wait the full poll interval. Same
        // shape as `loader::watch_loop`.
        //
        // The file watcher in `NativeLoader::spawn_watcher` is what
        // actually drives reload; this thread only observes
        // `load_counter` advances and rebuilds the GUI to match.
        let inner_for_thread = Arc::clone(inner);
        let params_for_thread = Arc::clone(params);
        let loader_for_thread = Arc::clone(loader);
        let stop_flag = Arc::clone(&stop);
        let watcher = std::thread::Builder::new()
            .name("truce-gui-reload".into())
            .spawn(move || {
                const POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(500);
                const STOP_CHECK: std::time::Duration = std::time::Duration::from_millis(50);
                const LOCK_WAIT: std::time::Duration = std::time::Duration::from_millis(50);

                hot_debug!("[truce-gui-reload] watcher thread started");
                // Both constants are sub-second; the u128 → u32 cast
                // is bounded.
                #[allow(clippy::cast_possible_truncation)]
                let chunks = (POLL_INTERVAL.as_millis() / STOP_CHECK.as_millis()) as u32;

                let mut last_seen_counter: u64 = 0;
                if let Some(guard) = loader_for_thread.try_lock_for(LOCK_WAIT) {
                    last_seen_counter = guard.load_counter();
                }
                loop {
                    for _ in 0..chunks {
                        std::thread::sleep(STOP_CHECK);
                        if stop_flag.load(Ordering::Relaxed) {
                            hot_debug!(
                                "[truce-gui-reload] watcher thread stopping (editor dropped)"
                            );
                            return;
                        }
                    }

                    // Wait briefly for the loader lock — the audio
                    // thread holds it across each `process()` call,
                    // and a bare `try_lock` would routinely miss
                    // under sustained audio activity. 50 ms is big
                    // enough to span multiple buffers but small
                    // enough that the watcher tick still feels live.
                    let Some(guard) = loader_for_thread.try_lock_for(LOCK_WAIT) else {
                        hot_debug!("[truce-gui-reload] loader busy (audio holds lock); retrying");
                        continue;
                    };

                    let mut new_layout = None;

                    if guard.load_counter() != last_seen_counter {
                        hot_debug!(
                            "[truce-gui-reload] reload detected (counter {} → {}); resyncing GUI",
                            last_seen_counter,
                            guard.load_counter()
                        );
                        last_seen_counter = guard.load_counter();
                        if let Some(plugin) = guard.plugin() {
                            new_layout = Some(plugin.layout());
                        }
                    }

                    // Release the loader lock before touching the
                    // BuiltinEditor mutex so the audio thread can
                    // resume process() the moment we hand the layout
                    // off.
                    drop(guard);

                    if let Some(layout) = new_layout {
                        hot_debug!(
                            "[truce-gui-reload] layout: {}x{}",
                            layout.width,
                            layout.height
                        );
                        if layout.width == 0 || layout.height == 0 {
                            hot_debug!("[truce-gui-reload] skipping: layout has zero size");
                            continue;
                        }
                        let new_builtin = truce_gui::editor::BuiltinEditor::new_grid(
                            Arc::clone(&params_for_thread),
                            layout,
                        );
                        match inner_for_thread.lock() {
                            Ok(mut g) => {
                                let had_ctx = g.take_context();
                                hot_debug!(
                                    "[truce-gui-reload] old editor had context: {}",
                                    had_ctx.is_some()
                                );
                                *g = new_builtin;
                                if let Some(ctx) = had_ctx {
                                    g.set_context(ctx);
                                    hot_debug!("[truce-gui-reload] context restored on new editor");
                                } else {
                                    hot_debug!(
                                        "[truce-gui-reload] WARNING: no context to restore!"
                                    );
                                }
                            }
                            Err(_) => {
                                hot_debug!("[truce-gui-reload] ERROR: failed to lock inner mutex");
                            }
                        }
                    }
                }
            })
            .ok();

        Self {
            kind: HotEditorInner::Builtin { gpu },
            _watcher: watcher,
            stop,
        }
    }

    fn new_custom(editor: Box<dyn Editor>) -> Self {
        // Custom editors don't get background reload (yet).
        // Developer closes/reopens the window manually.
        Self {
            kind: HotEditorInner::Custom { editor },
            _watcher: None,
            stop: Arc::new(AtomicBool::new(false)),
        }
    }
}

impl<P: Params + 'static> Editor for HotEditor<P> {
    fn size(&self) -> (u32, u32) {
        match &self.kind {
            HotEditorInner::Builtin { gpu, .. } => gpu.size(),
            HotEditorInner::Custom { editor } => editor.size(),
        }
    }

    fn open(&mut self, parent: RawWindowHandle, context: PluginContext) {
        match &mut self.kind {
            HotEditorInner::Builtin { gpu, .. } => gpu.open(parent, context),
            HotEditorInner::Custom { editor } => editor.open(parent, context),
        }
    }

    fn close(&mut self) {
        match &mut self.kind {
            HotEditorInner::Builtin { gpu, .. } => gpu.close(),
            HotEditorInner::Custom { editor } => editor.close(),
        }
    }

    fn idle(&mut self) {
        match &mut self.kind {
            HotEditorInner::Builtin { gpu, .. } => gpu.idle(),
            HotEditorInner::Custom { editor } => editor.idle(),
        }
    }

    fn can_resize(&self) -> bool {
        match &self.kind {
            HotEditorInner::Builtin { gpu, .. } => gpu.can_resize(),
            HotEditorInner::Custom { editor } => editor.can_resize(),
        }
    }

    fn state_changed(&mut self) {
        match &mut self.kind {
            HotEditorInner::Builtin { gpu, .. } => gpu.state_changed(),
            HotEditorInner::Custom { editor } => editor.state_changed(),
        }
    }

    fn screenshot(&mut self, params: Arc<dyn truce_params::Params>) -> Option<(Vec<u8>, u32, u32)> {
        match &mut self.kind {
            HotEditorInner::Builtin { gpu, .. } => gpu.screenshot(params),
            HotEditorInner::Custom { editor } => editor.screenshot(params),
        }
    }
}

// Hot-reload is single-crate via `--features shell`, generated by
// `truce::plugin!` in `truce/src/plugin_macro.rs`. `HotShell<P>` is
// public-but-unadvertised because `__plugin_hot_reload!` wraps it via
// `truce::__reexport::HotShell`.
