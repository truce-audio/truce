//! Shell-side integration: HotShell<P> and HotEditor.
//!
//! HotShell implements truce-core's Plugin + PluginExport traits,
//! delegating all logic to the PluginLogic trait object in the
//! hot-reloadable dylib.

use std::ffi::c_void;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

use parking_lot::Mutex;
use truce_core::buffer::AudioBuffer;
use truce_core::bus::BusLayout;
use truce_core::editor::{Editor, EditorContext, RawWindowHandle};
use truce_core::events::{EventBody, EventList};
use truce_core::info::PluginInfo;
use truce_core::process::{ProcessContext, ProcessStatus};
use truce_core::plugin::Plugin;
use truce_params::Params;

use truce_gui::backend_cpu::CpuBackend;
use truce_gui::interaction::WidgetRegion;
use truce_gui::layout::PluginLayout;
use truce_gui::platform::{PlatformView, ViewCallbacks};
use truce_gui::widgets::WidgetType as WidgetKind;

use crate::loader::NativeLoader;


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
    /// Plugin info (static, kept for potential future use).
    #[allow(dead_code)]
    info: PluginInfo,
    /// Bus layouts (static, kept for potential future use).
    #[allow(dead_code)]
    bus_layouts: Vec<BusLayout>,
}

unsafe impl<P: Params> Send for HotShell<P> {}

impl<P: Params + 'static> HotShell<P> {
    pub fn new(params: P, dylib_path: PathBuf, info: PluginInfo, bus_layouts: Vec<BusLayout>) -> Self {
        let loader = NativeLoader::new(dylib_path);
        Self {
            params: Arc::new(params),
            loader: Arc::new(Mutex::new(loader)),
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
        Some(truce_gui::editor::BuiltinEditor::new_grid(Arc::clone(&self.params), layout))
    }
}

impl<P: Params + 'static> Plugin for HotShell<P> {
    fn info() -> PluginInfo where Self: Sized {
        unreachable!("HotShell::info() should not be called statically")
    }

    fn bus_layouts() -> Vec<BusLayout> where Self: Sized {
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

        // Sync the plugin's own params from the shell's params.
        // Uses set_plain WITHOUT snap_smoothers — smoothers advance naturally.
        if let Some(plugin_params) = plugin.params_mut() {
            for info in self.params.param_infos() {
                if let Some(value) = self.params.get_plain(info.id) {
                    plugin_params.set_plain(info.id, value);
                }
            }
        }

        // Build a ProcessContext with param/meter callbacks for the logic.
        let params = &self.params;
        let meters = &self.meters;
        let param_fn = |id: u32| -> f64 {
            params.get_plain(id).unwrap_or(0.0)
        };
        let meter_fn = |id: u32, v: f32| {
            if let Some(slot) = meters.get(id as usize) {
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
        loader.plugin().map(|p| p.save_state()).filter(|s| !s.is_empty())
    }

    fn load_state(&mut self, data: &[u8]) {
        let mut loader = self.loader.lock();
        if let Some(plugin) = loader.plugin_mut() {
            plugin.load_state(data);
        }
    }

    fn editor(&mut self) -> Option<Box<dyn Editor>> {
        if let Some(editor) = self.try_custom_editor() {
            return Some(editor);
        }
        self.try_builtin_editor()
            .map(|e| Box::new(e) as Box<dyn Editor>)
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
        self.meters.get(meter_id as usize)
            .map(|v| f32::from_bits(v.load(Ordering::Relaxed)))
            .unwrap_or(0.0)
    }
}

// ---------------------------------------------------------------------------
// HotEditor — delegates rendering to the dylib's render() method
// ---------------------------------------------------------------------------

struct HotEditor<P: Params> {
    loader: Arc<Mutex<NativeLoader>>,
    meters: Arc<[AtomicU32; 256]>,
    params_for_gui: Arc<P>,
    layout: PluginLayout,
    regions: Vec<WidgetRegion>,
    backend: Option<CpuBackend>,
    interaction: HotInteraction,
    context: Option<EditorContext>,
    view: Option<PlatformView>,
    self_ptr: *mut c_void,
}

unsafe impl<P: Params> Send for HotEditor<P> {}

impl<P: Params + 'static> HotEditor<P> {
    fn new(
        loader: Arc<Mutex<NativeLoader>>,
        meters: Arc<[AtomicU32; 256]>,
        layout: PluginLayout,
        params_for_gui: Arc<P>,
    ) -> Self {
        // Build initial regions from the layout.
        let mut interaction_state = truce_gui::interaction::InteractionState::new();
        interaction_state.build_regions(&layout);
        let regions = interaction_state.knob_regions;

        Self {
            loader,
            meters,
            params_for_gui,
            layout,
            regions,
            backend: None,
            interaction: HotInteraction::new(),
            context: None,
            view: None,
            self_ptr: std::ptr::null_mut(),
        }
    }

    fn on_mouse_down(&mut self, x: f32, y: f32) {
        let hit_result = {
            let loader = self.loader.lock();
            let Some(plugin) = loader.plugin() else { return };
            plugin.hit_test(&self.regions, x, y)
        };

        let Some(idx) = hit_result else { return };
        let region = &self.regions[idx];

        if let Some(ref ctx) = self.context {
            let param_id = region.param_id;
            let current = (ctx.get_param)(param_id);

            match region.widget_type {
                WidgetKind::Toggle => {
                    let new_val = if current > 0.5 { 0.0 } else { 1.0 };
                    (ctx.begin_edit)(param_id);
                    (ctx.set_param)(param_id, new_val);
                    (ctx.end_edit)(param_id);
                    self.regions[idx].normalized_value = new_val as f32;
                }
                WidgetKind::Meter => {}
                _ => {
                    (ctx.begin_edit)(param_id);
                    self.interaction.begin_drag(idx, param_id, current, y, region);
                }
            }
        }
    }

    fn on_mouse_dragged(&mut self, x: f32, y: f32) {
        let Some(ref drag) = self.interaction.dragging else { return };
        let Some(ref ctx) = self.context else { return };

        let new_val = match drag.kind {
            WidgetKind::Slider => {
                let margin = 6.0;
                let rel = (x - drag.region_x - margin) / (drag.region_w - margin * 2.0);
                (rel as f64).clamp(0.0, 1.0)
            }
            _ => {
                let dy = drag.start_y - y;
                let delta = dy as f64 / 200.0;
                (drag.start_value + delta).clamp(0.0, 1.0)
            }
        };
        (ctx.set_param)(drag.param_id, new_val);
    }

    fn on_mouse_up(&mut self, _x: f32, _y: f32) {
        if let Some(drag) = self.interaction.dragging.take() {
            if let Some(ref ctx) = self.context {
                (ctx.end_edit)(drag.param_id);
            }
        }
    }

    fn on_scroll(&mut self, x: f32, y: f32, delta_y: f32) {
        let hit_result = {
            let loader = self.loader.lock();
            let Some(plugin) = loader.plugin() else { return };
            plugin.hit_test(&self.regions, x, y)
        };
        let Some(idx) = hit_result else { return };
        let region = &self.regions[idx];

        if region.widget_type == WidgetKind::Meter { return; }

        if let Some(ref ctx) = self.context {
            let param_id = region.param_id;
            let current = (ctx.get_param)(param_id);
            let delta = delta_y as f64 / 200.0;
            let new_val = (current + delta).clamp(0.0, 1.0);
            (ctx.begin_edit)(param_id);
            (ctx.set_param)(param_id, new_val);
            (ctx.end_edit)(param_id);
        }
    }

    fn on_double_click(&mut self, x: f32, y: f32) {
        let hit_result = {
            let loader = self.loader.lock();
            let Some(plugin) = loader.plugin() else { return };
            plugin.hit_test(&self.regions, x, y)
        };
        let Some(idx) = hit_result else { return };
        let region = &self.regions[idx];

        if let Some(ref ctx) = self.context {
            let param_id = region.param_id;
            let default = self.params_for_gui.get_normalized(param_id).unwrap_or(0.5);
            (ctx.begin_edit)(param_id);
            (ctx.set_param)(param_id, default);
            (ctx.end_edit)(param_id);
        }
    }

    fn on_mouse_moved(&mut self, x: f32, y: f32) -> bool {
        let loader = self.loader.lock();
        let Some(plugin) = loader.plugin() else { return false };
        let hit = plugin.hit_test(&self.regions, x, y);
        drop(loader);
        self.interaction.hover_idx = hit;
        hit.is_some()
    }
}

// C callbacks for the platform view.

unsafe extern "C" fn hot_cb_render<P: Params + 'static>(
    ctx: *mut c_void,
    out_w: *mut u32,
    out_h: *mut u32,
) -> *const u8 {
    let editor = &mut *(ctx as *mut HotEditor<P>);

    // Update region values from host params and meters.
    let hover = editor.interaction.hover_idx;
    let _drag_param = editor.interaction.dragging.as_ref().map(|d| d.param_id);
    for region in &mut editor.regions {
        if let Some(ref ectx) = editor.context {
            region.normalized_value = (ectx.get_param)(region.param_id) as f32;
        }
        if region.widget_type == WidgetKind::Meter {
            if let Some(slot) = editor.meters.get(region.param_id as usize) {
                region.normalized_value = f32::from_bits(slot.load(Ordering::Relaxed));
            }
        }
    }

    let backend = match editor.backend.as_mut() {
        Some(b) => b,
        None => {
            *out_w = 0;
            *out_h = 0;
            return std::ptr::null();
        }
    };

    // Call the dylib's render() with the CpuBackend.
    {
        let loader = editor.loader.lock();
        if let Some(plugin) = loader.plugin() {
            plugin.render(backend);
        }
    }

    *out_w = backend.width();
    *out_h = backend.height();
    backend.data().as_ptr()
}

unsafe extern "C" fn hot_cb_mouse_down<P: Params + 'static>(ctx: *mut c_void, x: f32, y: f32) {
    let editor = &mut *(ctx as *mut HotEditor<P>);
    editor.on_mouse_down(x, y);
}

unsafe extern "C" fn hot_cb_mouse_dragged<P: Params + 'static>(ctx: *mut c_void, x: f32, y: f32) {
    let editor = &mut *(ctx as *mut HotEditor<P>);
    editor.on_mouse_dragged(x, y);
}

unsafe extern "C" fn hot_cb_mouse_up<P: Params + 'static>(ctx: *mut c_void, x: f32, y: f32) {
    let editor = &mut *(ctx as *mut HotEditor<P>);
    editor.on_mouse_up(x, y);
}

unsafe extern "C" fn hot_cb_scroll<P: Params + 'static>(ctx: *mut c_void, x: f32, y: f32, dy: f32) {
    let editor = &mut *(ctx as *mut HotEditor<P>);
    editor.on_scroll(x, y, dy);
}

unsafe extern "C" fn hot_cb_double_click<P: Params + 'static>(ctx: *mut c_void, x: f32, y: f32) {
    let editor = &mut *(ctx as *mut HotEditor<P>);
    editor.on_double_click(x, y);
}

unsafe extern "C" fn hot_cb_mouse_moved<P: Params + 'static>(ctx: *mut c_void, x: f32, y: f32) -> u8 {
    let editor = &mut *(ctx as *mut HotEditor<P>);
    editor.on_mouse_moved(x, y) as u8
}

impl<P: Params + 'static> Editor for HotEditor<P> {
    fn size(&self) -> (u32, u32) {
        (self.layout.width, self.layout.height)
    }

    fn open(&mut self, parent: RawWindowHandle, context: EditorContext) {
        let (w, h) = self.size();
        self.backend = CpuBackend::new(w, h);
        self.context = Some(context);

        let parent_ptr = match parent {
            RawWindowHandle::AppKit(ptr) => ptr,
            #[allow(unused)]
            _ => std::ptr::null_mut(),
        };

        if !parent_ptr.is_null() {
            let self_ptr = self as *mut HotEditor<P> as *mut c_void;
            self.self_ptr = self_ptr;

            let callbacks = ViewCallbacks {
                render: Some(hot_cb_render::<P>),
                mouse_down: Some(hot_cb_mouse_down::<P>),
                mouse_dragged: Some(hot_cb_mouse_dragged::<P>),
                mouse_up: Some(hot_cb_mouse_up::<P>),
                scroll: Some(hot_cb_scroll::<P>),
                double_click: Some(hot_cb_double_click::<P>),
                mouse_moved: Some(hot_cb_mouse_moved::<P>),
            };

            self.view = unsafe { PlatformView::new(parent_ptr, w, h, self_ptr, &callbacks) };
        }
    }

    fn close(&mut self) {
        self.view = None;
        self.context = None;
        self.backend = None;
        self.self_ptr = std::ptr::null_mut();
    }

    fn idle(&mut self) {
        // Platform view handles its own repaint timer.
    }
}

// ---------------------------------------------------------------------------
// Simplified interaction state for the hot editor
// ---------------------------------------------------------------------------

struct HotInteraction {
    hover_idx: Option<usize>,
    dragging: Option<HotDragState>,
}

struct HotDragState {
    param_id: u32,
    start_value: f64,
    start_y: f32,
    kind: WidgetKind,
    region_x: f32,
    region_w: f32,
}

impl HotInteraction {
    fn new() -> Self {
        Self { hover_idx: None, dragging: None }
    }

    fn begin_drag(&mut self, _idx: usize, param_id: u32, current: f64, mouse_y: f32, region: &WidgetRegion) {
        self.dragging = Some(HotDragState {
            param_id,
            start_value: current,
            start_y: mouse_y,
            kind: region.widget_type,
            region_x: region.x,
            region_w: region.w,
        });
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
        editor: { $($editor_body:tt)* },
    ) => {
        struct __HotShellWrapper {
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
                if let Some(e) = self.inner.try_custom_editor() {
                    return Some(e);
                }
                if let Some(builtin) = self.inner.try_builtin_editor() {
                    return Some($($editor_body)*(builtin));
                }
                None
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
