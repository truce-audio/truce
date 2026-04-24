//! Shell-side integration: HotShell<P> and HotEditor.
//!
//! HotShell implements truce-core's Plugin + PluginExport traits,
//! delegating all logic to the PluginLogic trait object in the
//! hot-reloadable dylib.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;

use parking_lot::Mutex;
use truce_core::buffer::AudioBuffer;
use truce_core::bus::BusLayout;
use truce_core::editor::{Editor, EditorContext, RawWindowHandle};
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
    dylib_path: PathBuf,
    /// Meter values written by DSP, read by GUI.
    meters: Arc<[AtomicU32; 256]>,
    sample_rate: f64,
    max_block_size: usize,
    /// Plugin info (static, kept for potential future use).
    #[allow(dead_code)]
    info: PluginInfo,
    /// Bus layouts (static, kept for potential future use).
    #[allow(dead_code)]
    bus_layouts: Vec<BusLayout>,
}

unsafe impl<P: Params> Send for HotShell<P> {}

impl<P: Params + 'static> HotShell<P> {
    pub fn new(
        params: P,
        dylib_path: PathBuf,
        info: PluginInfo,
        bus_layouts: Vec<BusLayout>,
    ) -> Self {
        let params = Arc::new(params);
        let params_ptr = Arc::as_ptr(&params) as *const ();
        let loader = NativeLoader::new(dylib_path.clone(), params_ptr);
        Self {
            params,
            loader: Arc::new(Mutex::new(loader)),
            dylib_path,
            meters: Arc::new(std::array::from_fn(|_| AtomicU32::new(0))),
            sample_rate: 44100.0,
            max_block_size: 1024,
            info,
            bus_layouts,
        }
    }

    /// Try to get a custom editor from the loaded plugin.
    pub fn try_custom_editor(&self) -> Option<Box<dyn Editor>> {
        let loader = self.loader.lock();
        let plugin = loader.plugin()?;
        plugin.custom_editor()
    }

    /// Try to create a `BuiltinEditor` from the loaded plugin's layout.
    /// Returns `None` if no plugin is loaded or the layout has zero size.
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

        let mut loader = self.loader.lock();
        if let Some(plugin) = loader.plugin_mut() {
            plugin.reset(sample_rate, max_block_size);
        }
    }

    fn process(
        &mut self,
        buffer: &mut AudioBuffer,
        events: &EventList,
        context: &mut ProcessContext,
    ) -> ProcessStatus {
        let mut loader = self.loader.lock();

        // Check for hot-reload.
        if loader.is_reload_pending() {
            if loader.reload() {
                if let Some(plugin) = loader.plugin_mut() {
                    plugin.reset(self.sample_rate, self.max_block_size);
                }
            }
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
            let idx = id.wrapping_sub(256) as usize;
            if let Some(slot) = meters.get(idx) {
                slot.store(v.to_bits(), Ordering::Relaxed);
            }
        };
        let mut output_events = EventList::new();
        let mut ctx = ProcessContext::new(
            context.transport,
            context.sample_rate,
            buffer.num_samples(),
            &mut output_events,
        )
        .with_params(&param_fn)
        .with_meters(&meter_fn);

        let result = plugin.process(buffer, events, &mut ctx);

        // Copy output events back to the host's event list.
        for event in output_events.iter() {
            context.output_events.push(event.clone());
        }
        result
    }

    fn save_state(&self) -> Option<Vec<u8>> {
        let loader = self.loader.lock();
        loader
            .plugin()
            .map(|p| p.save_state())
            .filter(|s| !s.is_empty())
    }

    fn load_state(&mut self, data: &[u8]) {
        let mut loader = self.loader.lock();
        if let Some(plugin) = loader.plugin_mut() {
            plugin.load_state(data);
        }
    }

    fn editor(&mut self) -> Option<Box<dyn Editor>> {
        hot_debug!("[truce-hot] editor() called");
        let params_ptr = Arc::as_ptr(&self.params) as *const ();
        let gui_loader = NativeLoader::new(self.dylib_path.clone(), params_ptr);

        // Custom editor path (egui, iced)
        if let Some(custom) = self.try_custom_editor() {
            hot_debug!("[truce-hot] using custom editor");
            return Some(Box::new(HotEditor::<P>::new_custom(custom)));
        }

        // Built-in editor path (layout + GPU)
        let builtin = self.try_builtin_editor()?;
        hot_debug!("[truce-hot] using builtin editor (GPU path)");
        let inner = Arc::new(std::sync::Mutex::new(builtin));
        let gpu = truce_gpu::GpuEditor::new_shared(Arc::clone(&inner));
        Some(Box::new(HotEditor::new_builtin(
            gpu,
            inner,
            gui_loader,
            Arc::clone(&self.params),
        )))
    }

    fn latency(&self) -> u32 {
        let loader = self.loader.lock();
        loader.plugin().map(|p| p.latency()).unwrap_or(0)
    }

    fn tail(&self) -> u32 {
        let loader = self.loader.lock();
        loader.plugin().map(|p| p.tail()).unwrap_or(0)
    }

    fn get_meter(&self, meter_id: u32) -> f32 {
        let idx = meter_id.wrapping_sub(256) as usize;
        self.meters
            .get(idx)
            .map(|v| f32::from_bits(v.load(Ordering::Relaxed)))
            .unwrap_or(0.0)
    }
}

// ---------------------------------------------------------------------------
// HotEditor — wraps editors for GUI hot-reload
// ---------------------------------------------------------------------------

enum HotEditorInner<P: Params> {
    /// Built-in GUI: swap BuiltinEditor inside shared mutex on reload.
    /// GPU rendering continues seamlessly.
    Builtin {
        gpu: truce_gpu::GpuEditor<P>,
        inner: Arc<std::sync::Mutex<truce_gui::editor::BuiltinEditor<P>>>,
    },
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
        inner: Arc<std::sync::Mutex<truce_gui::editor::BuiltinEditor<P>>>,
        mut gui_loader: NativeLoader,
        params: Arc<P>,
    ) -> Self {
        let stop = Arc::new(AtomicBool::new(false));

        // Spawn a background thread that watches for dylib changes
        // and swaps the BuiltinEditor inside the shared mutex.
        let inner_for_thread = Arc::clone(&inner);
        let params_for_thread = Arc::clone(&params);
        let stop_flag = Arc::clone(&stop);
        let watcher = std::thread::Builder::new()
            .name("truce-gui-reload".into())
            .spawn(move || {
                hot_debug!("[truce-gui-reload] watcher thread started");
                loop {
                    std::thread::sleep(std::time::Duration::from_millis(500));
                    if stop_flag.load(Ordering::Relaxed) {
                        hot_debug!("[truce-gui-reload] watcher thread stopping (editor dropped)");
                        return;
                    }
                    if gui_loader.is_reload_pending() {
                        hot_debug!("[truce-gui-reload] reload pending, attempting reload...");
                        if gui_loader.reload() {
                            hot_debug!("[truce-gui-reload] dylib reloaded successfully");
                            if let Some(plugin) = gui_loader.plugin() {
                                let layout = plugin.layout();
                                hot_debug!("[truce-gui-reload] layout: {}x{}", layout.width, layout.height);
                                if layout.width > 0 && layout.height > 0 {
                                    let new_builtin = truce_gui::editor::BuiltinEditor::new_grid(
                                        Arc::clone(&params_for_thread), layout,
                                    );
                                    if let Ok(mut guard) = inner_for_thread.lock() {
                                        let had_ctx = guard.take_context();
                                        hot_debug!("[truce-gui-reload] old editor had context: {}", had_ctx.is_some());
                                        *guard = new_builtin;
                                        if let Some(ctx) = had_ctx {
                                            guard.set_context(ctx);
                                            hot_debug!("[truce-gui-reload] context restored on new editor");
                                        } else {
                                            hot_debug!("[truce-gui-reload] WARNING: no context to restore!");
                                        }
                                    } else {
                                        hot_debug!("[truce-gui-reload] ERROR: failed to lock inner mutex");
                                    }
                                } else {
                                    hot_debug!("[truce-gui-reload] skipping: layout has zero size");
                                }
                            } else {
                                hot_debug!("[truce-gui-reload] ERROR: no plugin after reload");
                            }
                        } else {
                            hot_debug!("[truce-gui-reload] reload failed");
                        }
                    }
                }
            })
            .ok();

        Self {
            kind: HotEditorInner::Builtin { gpu, inner },
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

    fn open(&mut self, parent: RawWindowHandle, context: EditorContext) {
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
}

// ---------------------------------------------------------------------------
// export_hot! macro
// ---------------------------------------------------------------------------

/// Generate a hot-reloadable plugin shell.
///
/// Creates a wrapper struct that implements Plugin + PluginExport,
/// loading the plugin logic from a hot-reloadable dylib.
#[macro_export]
macro_rules! export_hot {
    (
        params: $params:ty,
        info: $info:expr,
        bus_layouts: [$($layout:expr),* $(,)?],
        logic_dylib: $dylib_name:expr,
    ) => {
        pub struct __HotShellWrapper {
            inner: $crate::shell::HotShell<$params>,
        }

        impl __HotShellWrapper {
            fn dylib_path() -> std::path::PathBuf {
                if let Ok(p) = std::env::var("TRUCE_LOGIC_PATH") {
                    return std::path::PathBuf::from(p);
                }

                let mut path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
                loop {
                    if path.join("target").is_dir() {
                        break;
                    }
                    if !path.pop() {
                        path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
                        break;
                    }
                }
                path.push("target");
                if cfg!(debug_assertions) {
                    path.push("debug");
                } else {
                    path.push("release");
                }

                #[cfg(target_os = "macos")]
                path.push(concat!("lib", $dylib_name, ".dylib"));
                #[cfg(target_os = "linux")]
                path.push(concat!("lib", $dylib_name, ".so"));
                #[cfg(target_os = "windows")]
                path.push(concat!($dylib_name, ".dll"));

                path
            }
        }

        impl $crate::__macro_deps::truce_core::plugin::Plugin for __HotShellWrapper {
            fn info() -> $crate::__macro_deps::truce_core::info::PluginInfo where Self: Sized {
                $info
            }

            fn bus_layouts() -> Vec<$crate::__macro_deps::truce_core::bus::BusLayout> where Self: Sized {
                vec![$($layout),*]
            }

            fn init(&mut self) {
                self.inner.init();
            }

            fn reset(&mut self, sample_rate: f64, max_block_size: usize) {
                self.inner.reset(sample_rate, max_block_size);
            }

            fn process(
                &mut self,
                buffer: &mut $crate::__macro_deps::truce_core::buffer::AudioBuffer,
                events: &$crate::__macro_deps::truce_core::events::EventList,
                context: &mut $crate::__macro_deps::truce_core::process::ProcessContext,
            ) -> $crate::__macro_deps::truce_core::process::ProcessStatus {
                self.inner.process(buffer, events, context)
            }

            fn save_state(&self) -> Option<Vec<u8>> {
                self.inner.save_state()
            }

            fn load_state(&mut self, data: &[u8]) {
                self.inner.load_state(data);
            }

            fn editor(&mut self) -> Option<Box<dyn $crate::__macro_deps::truce_core::editor::Editor>> {
                self.inner.editor()
            }

            fn latency(&self) -> u32 { self.inner.latency() }
            fn tail(&self) -> u32 { self.inner.tail() }
            fn get_meter(&self, meter_id: u32) -> f32 { self.inner.get_meter(meter_id) }
        }

        impl $crate::__macro_deps::truce_core::export::PluginExport for __HotShellWrapper {
            type Params = $params;

            fn create() -> Self {
                let params = <$params>::new();
                let info = <Self as $crate::__macro_deps::truce_core::plugin::Plugin>::info();
                let bus_layouts = <Self as $crate::__macro_deps::truce_core::plugin::Plugin>::bus_layouts();
                let path = Self::dylib_path();
                Self {
                    inner: $crate::shell::HotShell::new(params, path, info, bus_layouts),
                }
            }

            fn params(&self) -> &$params {
                &self.inner.params
            }

            fn params_mut(&mut self) -> &mut $params {
                // SAFETY: Only called during activate/deactivate when the editor
                // is not open (no concurrent Arc refs to params).
                std::sync::Arc::get_mut(&mut self.inner.params)
                    .expect("params_mut called while Arc has other refs")
            }

            fn params_arc(&self) -> std::sync::Arc<$params> {
                std::sync::Arc::clone(&self.inner.params)
            }
        }
    };
}
